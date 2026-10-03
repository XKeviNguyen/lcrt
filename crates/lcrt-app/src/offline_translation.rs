//! Local MT runs independently of Whisper. One in-flight window and one
//! replaceable pending window bound work; source captions never wait for MT.
use std::{
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
use lcrt_openai::lanes::{TargetControl, TargetStatusCallback};

pub(crate) const UNSUPPORTED: &str = "Offline translation is currently available for Japanese↔English and Vietnamese↔English. Choose a spoken language and a supported target.";
const DECODE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TEXT_BYTES: usize = 2048;

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
                    {
                        let mut slot = work.request.lock().expect("MT request mutex");
                        while slot.is_none() && !work.stopped.load(Ordering::Acquire) {
                            slot = work.changed.wait(slot).expect("MT request mutex");
                        }
                        if work.stopped.load(Ordering::Acquire)
                            || slot
                                .as_ref()
                                .is_none_or(|next| next.revision == request.revision)
                        {
                            break;
                        }
                    }
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
                            let revision = control
                                .snapshot()
                                .map(|state| state.0)
                                .unwrap_or(request.revision);
                            *delivered.lock().expect("MT result mutex") = Some(Response {
                                id: request.id,
                                revision,
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

pub(crate) struct LocalTranslation {
    speech: Box<dyn Transcriber>,
    source: Language,
    control: TargetControl,
    status: TargetStatusCallback,
    worker: Option<Worker>,
    original: String,
    targets: Vec<TargetText>,
    revision: u64,
    /// Only the current rolling hypothesis is translated. Final utterances
    /// exclude an already committed prefix; history is never retranslated.
    stable: String,
    last_request: String,
    submitted: u64,
    completed: u64,
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
            stable: String::new(),
            last_request: String::new(),
            submitted: 0,
            completed: 0,
        })
    }

    fn reconcile(&mut self) -> Vec<Language> {
        let Some((revision, targets, stopped)) = self.control.snapshot() else {
            return Vec::new();
        };
        if revision != self.revision {
            self.revision = revision;
            self.last_request.clear();
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

    fn collect(&mut self) -> Result<bool, TranscriptionError> {
        let mut changed = false;
        if let Some(worker) = &self.worker
            && let Some(response) = worker.results.lock().expect("MT result mutex").take()
        {
            if response.revision != self.revision
                || self
                    .control
                    .snapshot()
                    .is_none_or(|state| state.0 != response.revision)
            {
                return Ok(false);
            }
            self.completed = response.id;
            if let Some(error) = response.error {
                return Err(TranscriptionError::new(error));
            }
            for target in response.targets {
                self.targets
                    .retain(|previous| previous.language != target.language);
                self.targets.push(target);
                changed = true;
            }
        }
        Ok(changed)
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
        active: Vec<Language>,
    ) -> Vec<TranscriptUpdate> {
        let mut result = Vec::new();
        for update in updates {
            let text = if update.status() == CaptionStatus::Final {
                update
                    .text()
                    .strip_prefix(&self.stable)
                    .unwrap_or(update.text())
            } else {
                update.partial_text()
            };
            let text = tail(text, MAX_TEXT_BYTES).trim().to_owned();
            self.stable = update.stable_text().to_owned();
            self.original = update.text().to_owned();
            if !text.is_empty() && text != self.last_request && !active.is_empty() {
                if let Some(worker) = &self.worker {
                    self.submitted += 1;
                    worker.submit(Request {
                        id: self.submitted,
                        revision: self.revision,
                        targets: active.clone(),
                        text: text.clone(),
                    });
                }
                self.last_request = text;
            }
            // The source lane is returned immediately, before translation.
            result.extend(self.update(CaptionStatus::Partial));
            if update.status() == CaptionStatus::Final {
                self.stable.clear();
                self.last_request.clear();
            }
        }
        result
    }
}

fn tail(text: &str, limit: usize) -> &str {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

impl Transcriber for LocalTranslation {
    fn push_audio(
        &mut self,
        chunk: AudioChunk,
    ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        let active = self.reconcile();
        let translated = self.collect()?;
        let updates = self.speech.push_audio(chunk)?;
        let mut result = self.consume(updates, active);
        if translated && result.is_empty() {
            result = self.update(CaptionStatus::Partial);
        }
        Ok(result)
    }
    fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        let active = self.reconcile();
        let updates = self.speech.finish()?;
        let mut result = self.consume(updates, active);
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if self.collect()? {
                result.extend(self.update(CaptionStatus::Partial));
            }
            if self.completed >= self.submitted {
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
            Ok(vec![TranscriptUpdate::partial("source speech").unwrap()])
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
        let updates = adapter.push_audio(chunk).unwrap();
        assert_eq!(
            updates[0].translation_lanes().unwrap().original,
            "source speech"
        );
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

    #[test]
    fn final_drain_waits_for_the_final_request_after_a_partial_result() {
        use lcrt_core::TranslationTargets;
        struct FinalSpeech;
        impl Transcriber for FinalSpeech {
            fn push_audio(
                &mut self,
                _: AudioChunk,
            ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
                Ok(Vec::new())
            }
            fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
                Ok(vec![TranscriptUpdate::finalized("final speech").unwrap()])
            }
        }
        let root = fixture("final-drain");
        std::fs::write(
            root.join("worker.py"),
            r#"import sys,json,time
from pathlib import Path
root=Path(sys.argv[1])
print('{"ready":true}',flush=True)
for line in sys.stdin:
 while not (root/'release').exists(): time.sleep(.001)
 print(json.dumps({'text':json.loads(line)['text']}),flush=True)
"#,
        )
        .unwrap();
        let control = TargetControl::new(TranslationTargets::resolve(
            Some(Language::English),
            None,
            Some(Language::Japanese),
        ));
        let mut adapter = LocalTranslation::new(
            Box::new(FinalSpeech),
            Language::Japanese,
            control,
            &root,
            Arc::new(|_, _| {}),
        )
        .unwrap();
        adapter.submitted = 1;
        *adapter.worker.as_ref().unwrap().results.lock().unwrap() = Some(Response {
            id: 1,
            revision: 0,
            targets: vec![TargetText {
                language: Language::English,
                text: "partial".into(),
            }],
            error: None,
        });
        let results = adapter.worker.as_ref().unwrap().results.clone();
        let release = root.join("release");
        let observer = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while results.lock().unwrap().is_some() {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
            // The final decode may complete only after Stop collects partial.
            std::fs::write(release, "").unwrap();
        });
        let updates = adapter.finish().unwrap();
        assert_eq!(
            updates.last().unwrap().translation_lanes().unwrap().targets[0].text,
            "final speech"
        );
        observer.join().unwrap();
        assert_eq!(adapter.completed, 2);
        std::fs::remove_dir_all(root).unwrap();
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
    fn chunk_limit_preserves_utf8() {
        let text = "日本語".repeat(1000);
        let chunk = tail(&text, MAX_TEXT_BYTES);
        assert!(chunk.len() <= MAX_TEXT_BYTES);
        assert!(text.ends_with(chunk));
    }
    #[test]
    fn the_pending_slot_keeps_only_the_latest_window() {
        let pending = Pending::default();
        for revision in 0..100 {
            *pending.request.lock().unwrap() = Some(Request {
                id: revision,
                revision,
                targets: vec![Language::English],
                text: "latest".into(),
            });
        }
        assert_eq!(pending.request.lock().unwrap().take().unwrap().revision, 99);
    }
}
