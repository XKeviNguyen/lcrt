//! Local MT runs independently of Whisper; source captions never wait for it.
//!
//! Only accepted speech is translated: sentences Whisper will not rewrite, and
//! the rest of an utterance once it is final. Each accepted segment is decoded
//! once, in order, one at a time, and its translation is appended to the lane,
//! so a translation the viewer has read never changes meaning under them.
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::sync_channel,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use lcrt_core::{
    AudioChunk, CaptionStatus, Language, TargetStatus, TargetText, Transcriber, TranscriptUpdate,
    TranscriptionError, TranslationLanes,
};
use lcrt_openai::{
    lanes::{TargetControl, TargetStatusCallback},
    protocol::bounded_tail,
    translation::MAX_LANE_BYTES,
};
use tracing::warn;

pub(crate) const UNSUPPORTED: &str = "Offline translation is currently available for Japanese↔English and Vietnamese↔English. Choose a spoken language and a supported target.";
const DECODE_TIMEOUT: Duration = Duration::from_secs(10);
/// One request's source text: well inside the worker's 2,048-byte and
/// 256-token input limits, so no accepted speech is cut off.
const MAX_SEGMENT_BYTES: usize = 512;
/// Accepted segments waiting for the worker before the oldest is skipped.
const MAX_QUEUED_SEGMENTS: usize = 8;
/// Marks speech left untranslated.
const GAP: &str = "…";

pub(crate) fn supports(source: Language, target: Language) -> bool {
    use Language::{English, Japanese, Vietnamese};
    matches!(
        (source, target),
        (Japanese | Vietnamese, English) | (English, Japanese | Vietnamese)
    )
}

struct Request {
    id: u64,
    revision: u64,
    targets: Vec<Language>,
    text: String,
}
struct Response {
    id: u64,
    revision: u64,
    targets: Vec<TargetText>,
    error: Option<String>,
}
#[derive(Default)]
struct Pending {
    request: Mutex<Option<Request>>,
    changed: Condvar,
    stopped: AtomicBool,
}

/// Owns a local child, so cancellation kills native inference as well as
/// retiring Rust work. The pipe reader has a byte cap and a bounded mailbox.
struct Worker {
    pending: Arc<Pending>,
    child: Arc<Mutex<Option<Child>>>,
    results: Arc<Mutex<Option<Response>>>,
    thread: Option<JoinHandle<()>>,
}

struct Process {
    input: std::process::ChildStdin,
    lines: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
    child: Arc<Mutex<Option<Child>>>,
}
impl Process {
    fn start(
        root: &Path,
        source: Language,
        targets: &[Language],
        child_slot: &Arc<Mutex<Option<Child>>>,
        pending: &Pending,
    ) -> Result<Self, TranscriptionError> {
        let mut child = Command::new("/usr/bin/python3")
            .args(["-I", "-u"])
            .arg(root.join("worker.py"))
            .arg(root)
            .arg(std::process::id().to_string())
            .args(
                targets
                    .iter()
                    .map(|target| format!("{}-{}", source.code(), target.code())),
            )
            .env("HF_HUB_OFFLINE", "1")
            .env("TRANSFORMERS_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| {
                TranscriptionError::new("Couldn't start local translation. Reinstall LILOPOP.")
            })?;
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        let mut slot = child_slot.lock().expect("MT child mutex");
        if pending.stopped.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TranscriptionError::new("Translation cancelled."));
        }
        *slot = Some(child);
        drop(slot);
        let (lines_tx, lines) = sync_channel(1);
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut line = Vec::new();
                // Read at most one byte beyond the protocol limit, even if a
                // corrupt child never emits a newline.
                match (&mut reader).take(8193).read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.len() > 8192 => break,
                    Ok(_) => {
                        if lines_tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let ready = lines
            .recv_timeout(DECODE_TIMEOUT)
            .ok()
            .and_then(|line| serde_json::from_slice::<serde_json::Value>(&line).ok())
            .is_some_and(|message| message["ready"] == true);
        let process = Self {
            input,
            lines: Some(lines),
            reader: Some(reader),
            child: child_slot.clone(),
        };
        if !ready {
            return Err(TranscriptionError::new(
                "Local translation runtime or selected model is missing or damaged. Reinstall LILOPOP.",
            ));
        }
        Ok(process)
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().expect("MT child mutex").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.lines.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Worker {
    fn start(
        root: &Path,
        source: Language,
        control: TargetControl,
    ) -> Result<Self, TranscriptionError> {
        let pending = Arc::new(Pending::default());
        let child = Arc::new(Mutex::new(None));
        let targets: Vec<_> = control
            .targets()
            .ok_or_else(|| TranscriptionError::new("Translation controls unavailable."))?
            .iter()
            .collect();
        let mut process = Process::start(root, source, &targets, &child, &pending)?;
        let work = pending.clone();
        let child_slot = child.clone();
        let root = root.to_owned();
        let results = Arc::new(Mutex::new(None));
        let delivered = results.clone();
        let worker = thread::spawn(move || {
            while !work.stopped.load(Ordering::Acquire) {
                let request = {
                    let mut slot = work.request.lock().expect("MT request mutex");
                    while slot.is_none() && !work.stopped.load(Ordering::Acquire) {
                        slot = work.changed.wait(slot).expect("MT request mutex");
                    }
                    slot.take()
                };
                let Some(request) = request else { break };
                let mut response = Response {
                    id: request.id,
                    revision: request.revision,
                    targets: Vec::new(),
                    error: None,
                };
                for target in request.targets {
                    if work.stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let message = serde_json::json!({"pair": format!("{}-{}", source.code(), target.code()), "text": request.text});
                    let result = (|| {
                        writeln!(process.input, "{message}")
                            .and_then(|()| process.input.flush())
                            .map_err(|_| "Local translation worker stopped.")?;
                        let line = process
                            .lines
                            .as_ref()
                            .expect("MT reader")
                            .recv_timeout(DECODE_TIMEOUT)
                            .map_err(|_| "Local translation timed out. Stop and start again.")?;
                        let value: serde_json::Value = serde_json::from_slice(&line)
                            .map_err(|_| "Invalid local translation response.")?;
                        value["text"]
                            .as_str()
                            .map(str::to_owned)
                            .ok_or("Local translation failed. Stop and start again.")
                    })();
                    match result {
                        Ok(text) if !text.trim().is_empty() => response.targets.push(TargetText {
                            language: target,
                            text,
                        }),
                        Ok(_) => {}
                        Err(error) => {
                            response.error = Some(error.to_owned());
                            break;
                        }
                    }
                }
                let failed = response.error.is_some();
                // Never block Stop behind a full result mailbox.
                *delivered.lock().expect("MT result mutex") = Some(response);
                if failed {
                    // Wait for replacement work before deciding whether the
                    // failed configuration is still authoritative. A control
                    // change can race with delivery of this failure.
                    let (next_id, next_revision) = {
                        let mut slot = work.request.lock().expect("MT request mutex");
                        while slot.is_none() && !work.stopped.load(Ordering::Acquire) {
                            slot = work.changed.wait(slot).expect("MT request mutex");
                        }
                        match slot.as_ref() {
                            Some(next)
                                if !work.stopped.load(Ordering::Acquire)
                                    && next.revision != request.revision =>
                            {
                                (next.id, next.revision)
                            }
                            _ => break,
                        }
                    };
                    // An obsolete decode may have broken the pipe. Recreate it
                    // on this worker thread, never on the source caption path.
                    drop(process);
                    let targets: Vec<_> = control
                        .targets()
                        .map(|set| set.iter().collect())
                        .unwrap_or_default();
                    match Process::start(&root, source, &targets, &child_slot, &work) {
                        Ok(replacement) => process = replacement,
                        Err(error) => {
                            // Answer the request the adapter is waiting for.
                            *delivered.lock().expect("MT result mutex") = Some(Response {
                                id: next_id,
                                revision: next_revision,
                                targets: Vec::new(),
                                error: Some(error.to_string()),
                            });
                            return;
                        }
                    }
                }
            }
        });
        Ok(Self {
            pending,
            child,
            results,
            thread: Some(worker),
        })
    }

    fn submit(&self, request: Request) {
        *self.pending.request.lock().expect("MT request mutex") = Some(request);
        self.pending.changed.notify_one();
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let mut slot = self.pending.request.lock().expect("MT request mutex");
        self.pending.stopped.store(true, Ordering::Release);
        slot.take();
        drop(slot);
        self.pending.changed.notify_one();
        if let Ok(mut child) = self.child.lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.kill();
        }
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

/// Accepted source speech waiting for translation. Whisper no longer
/// rewrites it, so it is translated once and its translation only appended.
struct Segment {
    text: String,
    /// Older accepted speech was skipped because translation fell behind.
    after_gap: bool,
}

pub(crate) struct LocalTranslation {
    speech: Box<dyn Transcriber>,
    source: Language,
    control: TargetControl,
    status: TargetStatusCallback,
    worker: Option<Worker>,
    original: String,
    /// Each target's published translation. It is only ever appended to, so
    /// a meaning once shown is never rewritten by a newer hypothesis.
    targets: Vec<TargetText>,
    revision: u64,
    /// Bytes of the current utterance accepted for translation: sentences of
    /// Whisper's stable prefix, then the rest once the utterance is final.
    /// The replaceable hypothesis is shown as source captions only.
    accepted: usize,
    /// The text of the latest accepted segment, to find `accepted` again
    /// after Whisper trims the front of a very long utterance.
    anchor: String,
    /// Accepted segments in speech order. Never replaced, only bounded.
    queue: VecDeque<Segment>,
    /// The one segment being decoded, with its request id.
    in_flight: Option<(u64, Segment)>,
    submitted: u64,
}

impl LocalTranslation {
    pub(crate) fn new(
        speech: Box<dyn Transcriber>,
        source: Language,
        control: TargetControl,
        root: &Path,
        status: TargetStatusCallback,
    ) -> Result<Self, TranscriptionError> {
        let targets = control
            .targets()
            .ok_or_else(|| TranscriptionError::new("Translation controls unavailable."))?;
        for target in targets.iter() {
            if !supports(source, target) {
                return Err(TranscriptionError::new(UNSUPPORTED));
            }
            for file in ["model.bin", "config.json", "source.spm", "target.spm"] {
                if !root
                    .join(format!("{}-{}", source.code(), target.code()))
                    .join(file)
                    .is_file()
                {
                    return Err(TranscriptionError::new(
                        "LILOPOP's offline translation models are missing. Reinstall LILOPOP.",
                    ));
                }
            }
        }
        let worker = Worker::start(root, source, control.clone())?;
        for target in targets.iter() {
            status(target, TargetStatus::Active);
        }
        Ok(Self {
            speech,
            source,
            control,
            status,
            worker: Some(worker),
            original: String::new(),
            targets: Vec::new(),
            revision: 0,
            accepted: 0,
            anchor: String::new(),
            queue: VecDeque::new(),
            in_flight: None,
            submitted: 0,
        })
    }

    fn reconcile(&mut self) -> Vec<Language> {
        let Some((revision, targets, stopped)) = self.control.snapshot() else {
            return Vec::new();
        };
        if revision != self.revision {
            self.revision = revision;
            self.targets.retain(|text| targets.contains(text.language));
            for target in targets.iter() {
                (self.status)(
                    target,
                    if stopped.contains(&target) {
                        TargetStatus::Paused
                    } else {
                        TargetStatus::Active
                    },
                );
            }
        }
        targets
            .iter()
            .filter(|target| !stopped.contains(target) && supports(self.source, *target))
            .collect()
    }

    /// Publishes the in-flight segment's translation, at most once.
    fn collect(&mut self) -> Result<bool, TranscriptionError> {
        let Some(response) = self
            .worker
            .as_ref()
            .and_then(|worker| worker.results.lock().expect("MT result mutex").take())
        else {
            return Ok(false);
        };
        // A result for no outstanding request, such as a duplicate.
        let Some((_, segment)) = self.in_flight.take_if(|(id, _)| *id == response.id) else {
            return Ok(false);
        };
        if response.revision < self.revision {
            // The targets changed during the decode: translate the segment
            // again for the current targets instead of losing it.
            self.queue.push_front(segment);
            return Ok(false);
        }
        if let Some(error) = response.error {
            return Err(TranscriptionError::new(error));
        }
        let changed = !response.targets.is_empty();
        for translated in response.targets {
            if segment.after_gap {
                self.publish(translated.language, GAP);
            }
            self.publish(translated.language, &translated.text);
        }
        Ok(changed)
    }

    fn publish(&mut self, language: Language, text: &str) {
        let index = self
            .targets
            .iter()
            .position(|lane| lane.language == language)
            .unwrap_or_else(|| {
                self.targets.push(TargetText {
                    language,
                    text: String::new(),
                });
                self.targets.len() - 1
            });
        let lane = &mut self.targets[index].text;
        // Japanese sentences follow each other without a space.
        if !lane.is_empty() && language != Language::Japanese {
            lane.push(' ');
        }
        lane.push_str(text.trim());
        if lane.len() > MAX_LANE_BYTES {
            *lane = bounded_tail(lane, MAX_LANE_BYTES).to_owned();
        }
    }

    fn update(&self, status: CaptionStatus) -> Vec<TranscriptUpdate> {
        let targets = self
            .control
            .targets()
            .map(|targets| {
                targets
                    .iter()
                    .map(|language| {
                        self.targets
                            .iter()
                            .find(|text| text.language == language)
                            .cloned()
                            .unwrap_or(TargetText {
                                language,
                                text: String::new(),
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();
        TranscriptUpdate::lanes(
            TranslationLanes {
                original: self.original.clone(),
                targets,
            },
            status,
        )
        .ok()
        .into_iter()
        .collect()
    }

    fn consume(
        &mut self,
        updates: Vec<TranscriptUpdate>,
        active: &[Language],
    ) -> Vec<TranscriptUpdate> {
        let mut result = Vec::new();
        for update in updates {
            self.original = update.text().to_owned();
            self.accept(&update, active);
            // The source lane is returned immediately, before translation.
            result.extend(self.update(CaptionStatus::Partial));
        }
        result
    }

    /// Queues the newly accepted part of `update`: the complete sentences of
    /// its stable prefix, or everything left once the utterance is final.
    fn accept(&mut self, update: &TranscriptUpdate, active: &[Language]) {
        let text = update.text();
        let closed = update.status() == CaptionStatus::Final;
        let end = if closed {
            text.len()
        } else {
            update.stable_text().len()
        };
        if !text
            .get(..self.accepted)
            .is_some_and(|accepted| accepted.ends_with(&self.anchor))
        {
            // Whisper trimmed the front of a very long utterance.
            self.accepted = text[..end]
                .rfind(&self.anchor)
                .map_or(0, |start| start + self.anchor.len());
        }
        while let Some(cut) = segment_end(text, self.accepted, end, closed) {
            let segment = &text[self.accepted..cut];
            if !segment.trim().is_empty() && !active.is_empty() {
                self.enqueue(segment.trim());
            }
            self.anchor = segment.to_owned();
            self.accepted = cut;
        }
        if closed {
            self.accepted = 0;
            self.anchor.clear();
        }
    }

    fn enqueue(&mut self, text: &str) {
        self.queue.push_back(Segment {
            text: text.to_owned(),
            after_gap: false,
        });
        if self.queue.len() > MAX_QUEUED_SEGMENTS {
            // Bounded overload: skip the oldest waiting speech and mark the
            // gap, rather than fall ever further behind the speaker.
            self.queue.pop_front();
            if let Some(next) = self.queue.front_mut() {
                next.after_gap = true;
            }
            warn!("Offline translation fell behind; skipped a sentence");
        }
    }

    /// Sends the oldest accepted segment once the worker is free, so newer
    /// speech never replaces accepted speech that is still untranslated.
    fn dispatch(&mut self, active: &[Language]) {
        if self.in_flight.is_some() || active.is_empty() {
            return;
        }
        let Some(worker) = &self.worker else { return };
        let Some(segment) = self.queue.pop_front() else {
            return;
        };
        self.submitted += 1;
        worker.submit(Request {
            id: self.submitted,
            revision: self.revision,
            targets: active.to_vec(),
            text: segment.text.clone(),
        });
        self.in_flight = Some((self.submitted, segment));
    }

    fn idle(&self, active: &[Language]) -> bool {
        self.in_flight.is_none() && (self.queue.is_empty() || active.is_empty())
    }
}

/// Sentence ends that need no following space.
const FULL_STOPS: [char; 4] = ['。', '！', '？', '．'];
/// Sentence ends that count only before a space, so "3.5" stays whole.
const SPACED_STOPS: [char; 4] = ['.', '!', '?', '…'];
/// Closing quotes and brackets: a sentence end before one is quoted speech.
const CLOSERS: [char; 7] = ['」', '』', '）', ')', '"', '”', '’'];
/// Where a sentence too long for one request may be split.
const CLAUSE_MARKS: [char; 7] = ['、', '，', ',', '；', ';', '：', ':'];

/// The end of the first segment of `text[start..end]`: a complete sentence,
/// a piece of a sentence too long for one request, or, once the utterance
/// is `closed`, whatever is left.
///
/// Punctuation is evidence of a boundary only inside text that is already
/// stable or final; it never makes a replaceable hypothesis translatable.
fn segment_end(text: &str, start: usize, end: usize, closed: bool) -> Option<usize> {
    let span = text.get(start..end)?;
    if span.trim().is_empty() {
        return None;
    }
    let mut split = None;
    for (index, mark) in span.char_indices() {
        let after = index + mark.len_utf8();
        if after > MAX_SEGMENT_BYTES {
            break;
        }
        let next = text[start + after..].chars().next();
        if (FULL_STOPS.contains(&mark) && !next.is_some_and(|next| CLOSERS.contains(&next)))
            || (SPACED_STOPS.contains(&mark) && next.is_none_or(char::is_whitespace))
        {
            return Some(start + after);
        }
        if CLAUSE_MARKS.contains(&mark) || mark.is_whitespace() {
            split = Some(start + after);
        }
    }
    if span.len() <= MAX_SEGMENT_BYTES {
        return closed.then_some(end);
    }
    Some(split.unwrap_or_else(|| {
        let mut limit = start + MAX_SEGMENT_BYTES;
        while !text.is_char_boundary(limit) {
            limit -= 1;
        }
        limit
    }))
}

impl Transcriber for LocalTranslation {
    fn push_audio(
        &mut self,
        chunk: AudioChunk,
    ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        let active = self.reconcile();
        let translated = self.collect()?;
        let updates = self.speech.push_audio(chunk)?;
        let mut result = self.consume(updates, &active);
        self.dispatch(&active);
        if translated && result.is_empty() {
            result = self.update(CaptionStatus::Partial);
        }
        Ok(result)
    }
    fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        let active = self.reconcile();
        let updates = self.speech.finish()?;
        let mut result = self.consume(updates, &active);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if self.collect()? {
                result.extend(self.update(CaptionStatus::Partial));
            }
            self.dispatch(&active);
            if self.idle(&active) {
                break;
            }
            if Instant::now() >= deadline {
                // Show that speech is missing instead of ending on a
                // translation that looks complete.
                for &language in &active {
                    self.publish(language, GAP);
                }
                break;
            }
            // A bounded final drain; normal caption delivery never waits.
            thread::park_timeout(Duration::from_millis(10));
        }
        self.worker.take();
        result.extend(self.update(CaptionStatus::Final));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Speech;
    impl Transcriber for Speech {
        fn push_audio(
            &mut self,
            _: AudioChunk,
        ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
            Ok(vec![TranscriptUpdate::finalized("source speech").unwrap()])
        }
        fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
            Ok(Vec::new())
        }
    }
    fn fixture(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("lilopop-mt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ja-en")).unwrap();
        for file in ["model.bin", "config.json", "source.spm", "target.spm"] {
            std::fs::write(root.join("ja-en").join(file), "fixture").unwrap();
        }
        // Ready, then block indefinitely in local inference. Drop must kill it.
        std::fs::write(root.join("worker.py"), "import sys\nprint('{\"ready\":true}', flush=True)\nsys.stdin.readline()\nsys.stdin.readline()\n").unwrap();
        root
    }
    #[test]
    fn stalled_mt_never_blocks_source_and_drop_kills_the_child() {
        use lcrt_core::TranslationTargets;
        let root = fixture("stalled");
        let control = TargetControl::new(TranslationTargets::resolve(
            Some(Language::English),
            None,
            Some(Language::Japanese),
        ));
        let mut adapter = LocalTranslation::new(
            Box::new(Speech),
            Language::Japanese,
            control.clone(),
            &root,
            Arc::new(|_, _| {}),
        )
        .unwrap();
        let started = Instant::now();
        let chunk = AudioChunk::new(vec![0.0; 320], 16000, 1).unwrap();
        for _ in 0..3 {
            let updates = adapter.push_audio(chunk.clone()).unwrap();
            assert_eq!(
                updates[0].translation_lanes().unwrap().original,
                "source speech"
            );
        }
        assert!(adapter.in_flight.is_some());
        assert!(started.elapsed() < Duration::from_secs(1));
        let process = adapter.worker.as_ref().unwrap().child.clone();
        let began = Instant::now();
        drop(adapter);
        assert!(began.elapsed() < Duration::from_secs(1));
        assert!(process.lock().unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn an_empty_hypothesis_keeps_the_worker_ready_for_more_speech() {
        let root = fixture("empty");
        std::fs::write(root.join("worker.py"), "import sys\nprint('{\"ready\":true}', flush=True)\nfor i,line in enumerate(sys.stdin):\n print('{\"text\":\"\"}' if i == 0 else '{\"text\":\"Hello\"}', flush=True)\n").unwrap();
        let worker = Worker::start(
            &root,
            Language::Japanese,
            TargetControl::new(lcrt_core::TranslationTargets::resolve(
                Some(Language::English),
                None,
                Some(Language::Japanese),
            )),
        )
        .unwrap();
        for expected in ["", "Hello"] {
            worker.submit(Request {
                id: 1,
                revision: 0,
                targets: vec![Language::English],
                text: "speech".into(),
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            let response = loop {
                if let Some(response) = worker.results.lock().unwrap().take() {
                    break response;
                }
                assert!(Instant::now() < deadline, "worker did not respond");
                thread::yield_now();
            };
            assert!(response.error.is_none());
            if expected.is_empty() {
                assert!(response.targets.is_empty());
            } else {
                assert_eq!(response.targets[0].text, expected);
            }
        }
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pause_and_resume_discard_a_previous_revision_result() {
        use lcrt_core::{TargetChange, TranslationTargets};
        let root = fixture("revision");
        let control = TargetControl::new(TranslationTargets::resolve(
            Some(Language::English),
            None,
            Some(Language::Japanese),
        ));
        let mut adapter = LocalTranslation::new(
            Box::new(Speech),
            Language::Japanese,
            control.clone(),
            &root,
            Arc::new(|_, _| {}),
        )
        .unwrap();
        assert!(control.apply(TargetChange::Pause(Language::English)));
        assert!(adapter.reconcile().is_empty());
        adapter.in_flight = Some((1, segment("spoken before the pause")));
        *adapter.worker.as_ref().unwrap().results.lock().unwrap() = Some(Response {
            id: 1,
            revision: 0,
            targets: vec![TargetText {
                language: Language::English,
                text: "stale".into(),
            }],
            error: None,
        });
        assert!(!adapter.collect().unwrap());
        assert!(adapter.targets.is_empty());
        // Accepted speech is translated again, never lost or misfiled.
        assert_eq!(adapter.queue[0].text, "spoken before the pause");
        assert!(control.apply(TargetChange::Resume(Language::English)));
        assert_eq!(adapter.reconcile(), [Language::English]);
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_superseded_worker_failure_restarts_only_the_local_worker() {
        use lcrt_core::{TargetChange, TranslationTargets};
        let root = fixture("worker-recovery");
        std::fs::write(
            root.join("worker.py"),
            r#"import sys,json,time
from pathlib import Path
root=Path(sys.argv[1])
print('{"ready":true}',flush=True)
for line in sys.stdin:
 if not (root/'entered').exists():
  (root/'entered').touch()
  while not (root/'release').exists(): time.sleep(.001)
  print('{"error":"obsolete failure"}',flush=True)
 else: print('{"text":"recovered"}',flush=True)
"#,
        )
        .unwrap();
        let control = TargetControl::new(TranslationTargets::resolve(
            Some(Language::English),
            None,
            Some(Language::Japanese),
        ));
        let worker = Worker::start(&root, Language::Japanese, control.clone()).unwrap();
        worker.submit(Request {
            id: 1,
            revision: 0,
            targets: vec![Language::English],
            text: "partial".into(),
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !root.join("entered").exists() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        control.apply(TargetChange::Pause(Language::English));
        control.apply(TargetChange::Resume(Language::English));
        let revision = control.snapshot().unwrap().0;
        worker.submit(Request {
            id: 2,
            revision,
            targets: vec![Language::English],
            text: "new speech".into(),
        });
        std::fs::write(root.join("release"), "").unwrap();
        loop {
            if let Some(response) = worker.results.lock().unwrap().take()
                && response.revision == revision
            {
                assert!(response.error.is_none());
                assert_eq!(response.targets[0].text, "recovered");
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn obsolete_failure_does_not_end_source_captions() {
        use lcrt_core::{TargetChange, TranslationTargets};
        let root = fixture("obsolete-error");
        let control = TargetControl::new(TranslationTargets::resolve(
            Some(Language::English),
            None,
            Some(Language::Japanese),
        ));
        let mut adapter = LocalTranslation::new(
            Box::new(Speech),
            Language::Japanese,
            control.clone(),
            &root,
            Arc::new(|_, _| {}),
        )
        .unwrap();
        control.apply(TargetChange::Pause(Language::English));
        adapter.reconcile();
        adapter.in_flight = Some((1, segment("source speech")));
        *adapter.worker.as_ref().unwrap().results.lock().unwrap() = Some(Response {
            id: 1,
            revision: 0,
            targets: Vec::new(),
            error: Some("obsolete failure".into()),
        });
        assert!(!adapter.collect().unwrap());
        assert_eq!(
            adapter
                .push_audio(AudioChunk::new(vec![0.0; 320], 16000, 1).unwrap())
                .unwrap()[0]
                .translation_lanes()
                .unwrap()
                .original,
            "source speech"
        );
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Whisper stand-in: one scripted batch of updates per speech chunk;
    /// silent chunks only poll for translation results.
    struct Script {
        steps: std::collections::VecDeque<Vec<TranscriptUpdate>>,
        last: Vec<TranscriptUpdate>,
    }
    impl Transcriber for Script {
        fn push_audio(
            &mut self,
            chunk: AudioChunk,
        ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
            if chunk.samples()[0] == 0.0 {
                return Ok(Vec::new());
            }
            Ok(self.steps.pop_front().unwrap_or_default())
        }
        fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
            Ok(std::mem::take(&mut self.last))
        }
    }
    fn partial(stable: &str, tail: &str) -> Vec<TranscriptUpdate> {
        vec![TranscriptUpdate::incremental(stable, tail).unwrap()]
    }
    fn finalized(text: &str) -> Vec<TranscriptUpdate> {
        vec![TranscriptUpdate::finalized(text).unwrap()]
    }
    /// Deterministic MT: `[text]`. Retranslating a longer hypothesis is
    /// therefore always a visible rewrite, never an extension. Decode `n`
    /// creates `entered{n}` and waits while `hold{n}` exists.
    fn echo_root(name: &str) -> std::path::PathBuf {
        let root = fixture(name);
        std::fs::write(
            root.join("worker.py"),
            r#"import sys,json,time
from pathlib import Path
root=Path(sys.argv[1])
print('{"ready":true}',flush=True)
for line in sys.stdin:
 count=len(list(root.glob('entered*')))+1
 (root/f'entered{count}').touch()
 while (root/f'hold{count}').exists(): time.sleep(.001)
 print(json.dumps({'text':'['+json.loads(line)['text']+']'},ensure_ascii=False),flush=True)
"#,
        )
        .unwrap();
        root
    }
    fn chunk() -> AudioChunk {
        AudioChunk::new(vec![0.0; 320], 16000, 1).unwrap()
    }
    fn speech() -> AudioChunk {
        AudioChunk::new(vec![0.5; 320], 16000, 1).unwrap()
    }
    fn english_control() -> TargetControl {
        TargetControl::new(lcrt_core::TranslationTargets::resolve(
            Some(Language::English),
            None,
            Some(Language::Japanese),
        ))
    }
    fn adapter(root: &Path, script: Script, control: TargetControl) -> LocalTranslation {
        speaking(Language::Japanese, root, script, control)
    }
    fn speaking(
        source: Language,
        root: &Path,
        script: Script,
        control: TargetControl,
    ) -> LocalTranslation {
        LocalTranslation::new(Box::new(script), source, control, root, Arc::new(|_, _| {})).unwrap()
    }
    /// The English lane of every update, in delivery order.
    fn english(updates: &[TranscriptUpdate], seen: &mut Vec<String>) {
        lane(updates, Language::English, seen);
    }
    /// The changes of one target lane, in delivery order.
    fn lane(updates: &[TranscriptUpdate], language: Language, seen: &mut Vec<String>) {
        for update in updates {
            let text = update
                .translation_lanes()
                .and_then(|lanes| lanes.target(language))
                .unwrap_or_default();
            if seen.last().is_none_or(|last| last != text) {
                seen.push(text.to_owned());
            }
        }
    }
    fn idle(adapter: &LocalTranslation) -> bool {
        adapter.in_flight.is_none() && adapter.queue.is_empty()
    }
    fn segment(text: &str) -> Segment {
        Segment {
            text: text.into(),
            after_gap: false,
        }
    }
    /// Pushes audio until every submitted request has been delivered.
    fn settle(adapter: &mut LocalTranslation, seen: &mut Vec<String>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            english(&adapter.push_audio(chunk()).unwrap(), seen);
            if idle(adapter) {
                return;
            }
            assert!(Instant::now() < deadline, "translation did not settle");
            thread::park_timeout(Duration::from_millis(1));
        }
    }
    /// Visible changes that replace shown text instead of extending it.
    fn rewrites(seen: &[String]) -> usize {
        seen.windows(2)
            .filter(|pair| !pair[1].starts_with(pair[0].as_str()))
            .count()
    }

    #[test]
    fn changing_partials_never_rewrite_a_shown_translation() {
        let root = echo_root("flicker");
        let script = Script {
            steps: [
                partial("", "今日は"),
                partial("", "今日は雨が"),
                partial("", "今日は雨が降らない"),
                partial("", "今日は雨が降らないと思っていた"),
                finalized("今日は雨が降らないと思っていたけど、降りました。"),
            ]
            .into(),
            last: Vec::new(),
        };
        let mut adapter = adapter(&root, script, english_control());
        let mut seen = vec![String::new()];
        for _ in 0..5 {
            english(&adapter.push_audio(speech()).unwrap(), &mut seen);
            settle(&mut adapter, &mut seen);
        }
        assert_eq!(rewrites(&seen), 0, "visible translations: {seen:?}");
        assert_eq!(
            seen.last().unwrap(),
            "[今日は雨が降らないと思っていたけど、降りました。]"
        );
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_late_result_for_a_superseded_hypothesis_is_never_shown() {
        let root = echo_root("late");
        for hold in ["hold1", "hold2"] {
            std::fs::write(root.join(hold), "").unwrap();
        }
        let script = Script {
            steps: [
                partial("", "明日は"),
                partial("", "明日は晴れ"),
                partial("", "明日は晴れでしょう"),
                finalized("明日は晴れでしょう。"),
            ]
            .into(),
            last: Vec::new(),
        };
        let mut adapter = adapter(&root, script, english_control());
        let mut seen = vec![String::new()];
        // Polls until decode `n` has started or nothing is outstanding.
        let wait = |adapter: &mut LocalTranslation, seen: &mut Vec<String>, n: u32| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !root.join(format!("entered{n}")).exists() && !idle(adapter) {
                assert!(Instant::now() < deadline, "decode {n} never started");
                english(&adapter.push_audio(chunk()).unwrap(), seen);
                thread::park_timeout(Duration::from_millis(1));
            }
            english(&adapter.push_audio(chunk()).unwrap(), seen);
        };
        english(&adapter.push_audio(speech()).unwrap(), &mut seen);
        wait(&mut adapter, &mut seen, 1);
        // Newer hypotheses and the final arrive while decode 1 is held.
        for _ in 0..3 {
            english(&adapter.push_audio(speech()).unwrap(), &mut seen);
        }
        std::fs::remove_file(root.join("hold1")).unwrap();
        wait(&mut adapter, &mut seen, 2);
        std::fs::remove_file(root.join("hold2")).unwrap();
        settle(&mut adapter, &mut seen);
        assert_eq!(seen, ["", "[明日は晴れでしょう。]"]);
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn continuous_speech_is_translated_sentence_by_sentence_before_it_ends() {
        let root = echo_root("continuous");
        let script = Script {
            steps: [
                partial("", "雨が降っています。風も"),
                partial("", "雨が降っています。風も強いです"),
                partial("雨が降っています。", "風も強いです。気をつけて"),
                partial("雨が降っています。風も強", "いです。気をつけてください"),
                partial("雨が降っています。風も強いです。気を", "つけてください"),
                // The final hypothesis of the open tail differs from the last
                // partial; only its own text is translated, once.
                finalized("雨が降っています。風も強いです。気をつけて下さいね。"),
            ]
            .into(),
            last: Vec::new(),
        };
        let mut adapter = adapter(&root, script, english_control());
        let mut seen = vec![String::new()];
        let mut after_step = Vec::new();
        for _ in 0..6 {
            english(&adapter.push_audio(speech()).unwrap(), &mut seen);
            settle(&mut adapter, &mut seen);
            after_step.push(seen.last().unwrap().clone());
        }
        assert_eq!(
            after_step,
            [
                "",
                "",
                "[雨が降っています。]",
                "[雨が降っています。]",
                "[雨が降っています。] [風も強いです。]",
                "[雨が降っています。] [風も強いです。] [気をつけて下さいね。]",
            ]
        );
        assert_eq!(rewrites(&seen), 0);
        assert_eq!(adapter.accepted, 0, "a final utterance resets the cursor");
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_front_trimmed_long_utterance_is_neither_repeated_nor_skipped() {
        let root = echo_root("trimmed");
        let script = Script {
            steps: [
                partial("一つ。二つ。", "三つ"),
                // Whisper's byte budget dropped the front of the utterance.
                partial("… 二つ。三つ。", "四つ"),
                finalized("… 二つ。三つ。四つ。"),
            ]
            .into(),
            last: Vec::new(),
        };
        let mut adapter = adapter(&root, script, english_control());
        let mut seen = vec![String::new()];
        for _ in 0..3 {
            english(&adapter.push_audio(speech()).unwrap(), &mut seen);
            settle(&mut adapter, &mut seen);
        }
        assert_eq!(seen.last().unwrap(), "[一つ。] [二つ。] [三つ。] [四つ。]");
        assert_eq!(rewrites(&seen), 0);
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn falling_behind_skips_the_oldest_waiting_speech_and_marks_the_gap() {
        let root = echo_root("overload");
        std::fs::write(root.join("hold1"), "").unwrap();
        let backlog: String = (2..=12).map(|n| format!("文{n}。")).collect();
        let script = Script {
            steps: [
                partial("文1。", "続き"),
                partial(&format!("文1。{backlog}"), "続き"),
            ]
            .into(),
            last: Vec::new(),
        };
        let mut adapter = adapter(&root, script, english_control());
        let mut seen = vec![String::new()];
        english(&adapter.push_audio(speech()).unwrap(), &mut seen);
        english(&adapter.push_audio(speech()).unwrap(), &mut seen);
        assert_eq!(adapter.queue.len(), MAX_QUEUED_SEGMENTS);
        std::fs::remove_file(root.join("hold1")).unwrap();
        settle(&mut adapter, &mut seen);
        assert_eq!(
            seen.last().unwrap(),
            "[文1。] … [文5。] [文6。] [文7。] [文8。] [文9。] [文10。] [文11。] [文12。]"
        );
        assert_eq!(rewrites(&seen), 0);
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_duplicate_result_is_published_once() {
        let root = echo_root("duplicate");
        let script = Script {
            steps: [finalized("一度だけ。")].into(),
            last: Vec::new(),
        };
        let mut adapter = adapter(&root, script, english_control());
        let mut seen = vec![String::new()];
        english(&adapter.push_audio(speech()).unwrap(), &mut seen);
        settle(&mut adapter, &mut seen);
        *adapter.worker.as_ref().unwrap().results.lock().unwrap() = Some(Response {
            id: adapter.submitted,
            revision: adapter.revision,
            targets: vec![TargetText {
                language: Language::English,
                text: "[一度だけ。]".into(),
            }],
            error: None,
        });
        assert!(!adapter.collect().unwrap());
        assert_eq!(adapter.targets[0].text, "[一度だけ。]");
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn english_speech_fills_japanese_and_vietnamese_lanes_in_order() {
        let root = echo_root("english");
        for pair in ["en-ja", "en-vi"] {
            std::fs::create_dir_all(root.join(pair)).unwrap();
            for file in ["model.bin", "config.json", "source.spm", "target.spm"] {
                std::fs::write(root.join(pair).join(file), "fixture").unwrap();
            }
        }
        let control = TargetControl::new(lcrt_core::TranslationTargets::resolve(
            Some(Language::Japanese),
            Some(Language::Vietnamese),
            Some(Language::English),
        ));
        let script = Script {
            steps: [
                partial("It costs 3.5 dollars. ", "Take"),
                finalized("It costs 3.5 dollars. Take care!"),
            ]
            .into(),
            last: Vec::new(),
        };
        let mut adapter = speaking(Language::English, &root, script, control);
        let (mut japanese, mut vietnamese) = (vec![String::new()], vec![String::new()]);
        for _ in 0..2 {
            let updates = adapter.push_audio(speech()).unwrap();
            lane(&updates, Language::Japanese, &mut japanese);
            lane(&updates, Language::Vietnamese, &mut vietnamese);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !idle(&adapter) {
                assert!(Instant::now() < deadline, "translation did not settle");
                let updates = adapter.push_audio(chunk()).unwrap();
                lane(&updates, Language::Japanese, &mut japanese);
                lane(&updates, Language::Vietnamese, &mut vietnamese);
                thread::park_timeout(Duration::from_millis(1));
            }
        }
        assert_eq!(
            japanese.last().unwrap(),
            "[It costs 3.5 dollars.][Take care!]"
        );
        assert_eq!(
            vietnamese.last().unwrap(),
            "[It costs 3.5 dollars.] [Take care!]"
        );
        assert_eq!(rewrites(&japanese) + rewrites(&vietnamese), 0);
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_current_worker_failure_is_reported_not_hidden() {
        let root = fixture("failure");
        std::fs::write(
            root.join("worker.py"),
            "import sys\nprint('{\"ready\":true}', flush=True)\nfor line in sys.stdin:\n print('{\"error\":\"Local translation failed.\"}', flush=True)\n",
        )
        .unwrap();
        let script = Script {
            steps: [finalized("失敗します。")].into(),
            last: Vec::new(),
        };
        let mut adapter = adapter(&root, script, english_control());
        let mut result = adapter.push_audio(speech());
        let deadline = Instant::now() + Duration::from_secs(5);
        while result.is_ok() {
            assert!(Instant::now() < deadline, "the failure was never reported");
            thread::park_timeout(Duration::from_millis(1));
            result = adapter.push_audio(chunk());
        }
        assert_eq!(
            result.unwrap_err().to_string(),
            "Local translation failed. Stop and start again."
        );
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stop_translates_every_final_sentence_in_order() {
        let root = echo_root("stop");
        let script = Script {
            steps: [partial("", "一つ目です。二つ目")].into(),
            last: finalized("一つ目です。二つ目です。"),
        };
        let mut adapter = adapter(&root, script, english_control());
        let mut seen = vec![String::new()];
        english(&adapter.push_audio(speech()).unwrap(), &mut seen);
        let updates = adapter.finish().unwrap();
        english(&updates, &mut seen);
        assert_eq!(
            seen,
            ["", "[一つ目です。]", "[一つ目です。] [二つ目です。]"]
        );
        assert_eq!(updates.last().unwrap().status(), CaptionStatus::Final);
        assert!(adapter.worker.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stop_marks_speech_it_could_not_translate_in_time() {
        let root = echo_root("stop-deadline");
        std::fs::write(root.join("hold1"), "").unwrap();
        let script = Script {
            steps: [finalized("届く文です。")].into(),
            last: finalized("止まる直前の文です。"),
        };
        let mut adapter = adapter(&root, script, english_control());
        english(&adapter.push_audio(speech()).unwrap(), &mut Vec::new());
        let child = adapter.worker.as_ref().unwrap().child.clone();
        let began = Instant::now();
        let updates = adapter.finish().unwrap();
        assert!(began.elapsed() < Duration::from_secs(2));
        let last = updates.last().unwrap();
        assert_eq!(last.status(), CaptionStatus::Final);
        // Neither the held sentence nor the one after it was translated.
        assert_eq!(
            last.translation_lanes().unwrap().target(Language::English),
            Some("…")
        );
        assert!(child.lock().unwrap().is_none(), "the worker outlived Stop");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn segments_end_at_sentences_within_stable_text() {
        let cases = [
            // Vietnamese: diacritics, spaced full stops.
            (
                "Trời đang mưa. Gió cũng mạnh",
                "Trời đang mưa. ",
                Some("Trời đang mưa."),
            ),
            // A decimal point is not a sentence end.
            ("It is 3.5 metres long", "It is 3.", None),
            // A full stop inside quoted speech does not end the sentence.
            (
                "「はい。」と言った。次",
                "「はい。」と言った。",
                Some("「はい。」と言った。"),
            ),
            // Stable text without a sentence end waits for more speech.
            ("今日は雨が降らないと", "今日は雨が", None),
        ];
        for (text, stable, expected) in cases {
            let end = segment_end(text, 0, stable.len(), false);
            assert_eq!(end.map(|end| &text[..end]), expected, "{text}");
        }
        // Once final, the rest is one segment even without punctuation.
        assert_eq!(segment_end("chào bạn", 0, 10, true), Some(10));
        assert_eq!(segment_end("chào bạn", 10, 10, true), None);
    }

    #[test]
    fn only_the_four_shipped_pairs_are_supported() {
        for source in Language::ALL {
            for target in Language::ALL {
                let expected = source != target
                    && [Language::English, Language::Japanese, Language::Vietnamese]
                        .contains(&source)
                    && [Language::English, Language::Japanese, Language::Vietnamese]
                        .contains(&target)
                    && (source == Language::English || target == Language::English);
                assert_eq!(supports(source, target), expected);
            }
        }
    }
    #[test]
    fn an_over_long_sentence_is_split_on_character_boundaries() {
        let text = "日本語".repeat(1000);
        let mut start = 0;
        while let Some(end) = segment_end(&text, start, text.len(), true) {
            assert!(end - start <= MAX_SEGMENT_BYTES);
            start = end;
        }
        assert_eq!(start, text.len());
    }
}
