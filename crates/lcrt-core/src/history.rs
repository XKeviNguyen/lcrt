//! Caption history: the accepted transcript of a session, kept on this device
//! only while the user has turned it on.
//!
//! Each session is one append-only JSON Lines file named after its id. The
//! first line describes the session and an end record closes it; a file
//! without one was interrupted. A damaged line, such as one cut short by a
//! crash, is skipped rather than failing the whole session. No audio is ever
//! stored.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{AudioSourceKind, Language, LanguageSelection, ProcessingMode};

/// Version of the session file format this build writes.
pub const FORMAT_VERSION: u32 = 1;
/// Default storage budget for all saved sessions.
pub const DEFAULT_STORAGE_LIMIT_MB: u32 = 100;
/// Storage budgets the user can choose from, in megabytes.
pub const STORAGE_LIMITS_MB: [u32; 5] = [25, 100, 250, 500, 1000];
/// Longest text kept for one segment or translation. Whisper's own bound on
/// an utterance is 16 KiB, so this only cuts runaway input.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
const EXTENSION: &str = "jsonl";
/// A line longer than this is damage, not a record.
const MAX_LINE_BYTES: u64 = 4 * MAX_TEXT_BYTES as u64;
/// How much of a file's end is read to find its end record.
const TAIL_BYTES: u64 = 4 * 1024;
/// The shortest cue: long enough to read a word.
const MIN_CUE: Duration = Duration::from_millis(700);
/// Subtitle line width, counting a wide (CJK) character as two.
const SUBTITLE_LINE_WIDTH: usize = 42;
/// Lines per subtitle cue before a long segment is split.
const SUBTITLE_CUE_LINES: usize = 2;

/// Identifies a saved session. It is also its file name, so it only ever
/// contains ASCII digits and dashes.
#[derive(Clone, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    /// The id of a session that started at `started_at_ms` (Unix time) as
    /// session `generation` of this process.
    pub fn new(started_at_ms: u64, generation: u64) -> Self {
        Self(format!("{started_at_ms}-{generation}"))
    }

    /// Accepts only what [`SessionId::new`] can produce, so an id from the
    /// UI or a directory listing can never name another file.
    pub fn parse(text: &str) -> Option<Self> {
        let valid = (3..=48).contains(&text.len())
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
            && text.as_bytes()[0].is_ascii_digit();
        valid.then(|| Self(text.to_owned()))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn file_name(&self) -> String {
        format!("{}.{EXTENSION}", self.0)
    }

    /// The same id with `suffix` appended, for a name that is taken.
    fn with_suffix(&self, suffix: u32) -> Self {
        Self(format!("{}-{suffix}", self.0))
    }
}

/// Where the audio came from, without naming the device.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// System (output) audio.
    SystemAudio,
    /// A microphone.
    Microphone,
}

impl From<AudioSourceKind> for SourceKind {
    fn from(kind: AudioSourceKind) -> Self {
        match kind {
            AudioSourceKind::SystemOutput => Self::SystemAudio,
            AudioSourceKind::Microphone => Self::Microphone,
        }
    }
}

impl SourceKind {
    /// User-facing name.
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemAudio => "System audio",
            Self::Microphone => "Microphone",
        }
    }
}

/// One line of a session file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryRecord {
    /// The first line: what the session was.
    Session(SessionHeader),
    /// Accepted source speech.
    Segment(SegmentRecord),
    /// A segment's translation into one target.
    Translation(TranslationRecord),
    /// Speech that happened here was not recorded.
    Gap(GapRecord),
    /// The last line of a session that ended.
    End(EndRecord),
}

/// What a session was, written when its recording begins.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Format version of the file.
    pub version: u32,
    /// When recording began, in Unix milliseconds.
    pub started_at_ms: u64,
    /// The processing mode.
    pub mode: ProcessingMode,
    /// The spoken language as chosen; `Auto` when it was detected.
    pub spoken_language: LanguageSelection,
    /// Translation targets when the session started; targets may change
    /// later, and each translation names its own language.
    pub targets: Vec<Language>,
    /// The kind of audio source.
    pub source: SourceKind,
    /// Recording began after captions had started, so earlier speech of the
    /// session is missing.
    pub started_late: bool,
}

/// Accepted source speech.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRecord {
    /// The segment, unique within the session.
    pub id: u64,
    /// Approximate start, from the start of the session's audio.
    pub start_ms: u64,
    /// Approximate end, from the start of the session's audio.
    pub end_ms: u64,
    /// The accepted text.
    pub text: String,
    /// Speech right before this segment was not recorded.
    #[serde(default)]
    pub after_gap: bool,
}

/// One translation of a segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationRecord {
    /// The translated segment.
    pub segment: u64,
    /// The target language.
    pub language: Language,
    /// The translation.
    pub text: String,
}

/// Why speech went unrecorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    /// The user turned history off for a while.
    Paused,
    /// The history writer fell behind and skipped text.
    Overloaded,
}

/// Speech that happened here was not recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GapRecord {
    /// Why.
    pub reason: GapReason,
}

/// How a session's recording ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The session stopped normally.
    Completed,
    /// The session ended with an error.
    Failed,
    /// Recording stopped because history reached its storage limit.
    StorageFull,
}

/// The last line of a session that ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EndRecord {
    /// When it ended, in Unix milliseconds.
    pub ended_at_ms: u64,
    /// How it ended.
    pub reason: EndReason,
    /// Audio processed while the session ran, in milliseconds.
    pub audio_ms: u64,
    /// Segments recorded.
    pub segments: u64,
    /// Gaps recorded.
    pub gaps: u64,
}

/// What the library shows about a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStatus {
    /// Being recorded now.
    Recording,
    /// Recorded from start to end without gaps.
    Completed,
    /// Recorded, but some speech is missing: recording started late,
    /// paused, fell behind, failed or ran out of storage.
    Incomplete,
    /// Never closed: LILOPOP quit or crashed while recording.
    Interrupted,
}

impl SessionStatus {
    /// Derives the status; `active` when this session is being recorded.
    pub fn of(header: &SessionHeader, end: Option<&EndRecord>, gaps: u64, active: bool) -> Self {
        match end {
            _ if active => Self::Recording,
            None => Self::Interrupted,
            Some(end)
                if end.reason == EndReason::Completed && gaps == 0 && !header.started_late =>
            {
                Self::Completed
            }
            Some(_) => Self::Incomplete,
        }
    }

    /// User-facing name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Recording => "Recording",
            Self::Completed => "Complete",
            Self::Incomplete => "Incomplete",
            Self::Interrupted => "Interrupted",
        }
    }
}

/// A session as listed, without its text.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    /// The session.
    pub id: SessionId,
    /// What it was.
    pub header: SessionHeader,
    /// Its end, if it ended.
    pub end: Option<EndRecord>,
    /// Segments recorded.
    pub segments: u64,
    /// Gaps recorded.
    pub gaps: u64,
    /// How long it ran, by its audio.
    pub duration: Duration,
    /// Size on disk.
    pub size_bytes: u64,
}

/// The saved sessions, newest first, and how many files could not be read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Library {
    /// Readable sessions, newest first.
    pub sessions: Vec<SessionSummary>,
    /// Session files whose first line is damaged.
    pub unreadable: usize,
}

/// One accepted segment with its translations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptSegment {
    /// The segment.
    pub id: u64,
    /// Approximate start, from the start of the session's audio.
    pub start: Duration,
    /// Approximate end.
    pub end: Duration,
    /// The source text.
    pub text: String,
    /// Translations by language, in the order they arrived.
    pub translations: Vec<(Language, String)>,
    /// Speech right before this segment was not recorded.
    pub after_gap: bool,
}

impl TranscriptSegment {
    /// The translation into `language`, if one was saved.
    pub fn translation(&self, language: Language) -> Option<&str> {
        self.translations
            .iter()
            .find(|(target, _)| *target == language)
            .map(|(_, text)| text.as_str())
    }
}

/// A whole saved session.
#[derive(Clone, Debug, PartialEq)]
pub struct HistorySession {
    /// The session.
    pub id: SessionId,
    /// What it was.
    pub header: SessionHeader,
    /// Its segments in speech order.
    pub segments: Vec<TranscriptSegment>,
    /// Its end, if it ended.
    pub end: Option<EndRecord>,
    /// Gaps, including segments recorded after one.
    pub gaps: u64,
    /// Speech at the very end was not recorded (a trailing gap).
    pub ends_with_gap: bool,
    /// Lines that could not be read and were skipped.
    pub damaged_lines: usize,
}

impl HistorySession {
    /// Every language the session has translations into, in first-seen
    /// order.
    pub fn translation_languages(&self) -> Vec<Language> {
        let mut languages: Vec<Language> = Vec::new();
        for segment in &self.segments {
            for (language, _) in &segment.translations {
                if !languages.contains(language) {
                    languages.push(*language);
                }
            }
        }
        languages
    }

    /// What can be exported: the original, and for each translated
    /// language its translation alone and side by side with the original.
    pub fn export_contents(&self) -> Vec<ExportContent> {
        let mut contents = vec![ExportContent::Original];
        for language in self.translation_languages() {
            contents.push(ExportContent::Translation(language));
            contents.push(ExportContent::Bilingual(language));
        }
        contents
    }

    /// How long the session ran, by its audio.
    pub fn duration(&self) -> Duration {
        self.end.map_or_else(
            || self.segments.last().map_or(Duration::ZERO, |last| last.end),
            |end| Duration::from_millis(end.audio_ms),
        )
    }

    fn from_records(
        id: SessionId,
        records: impl Iterator<Item = Option<HistoryRecord>>,
    ) -> Option<Self> {
        let mut session: Option<Self> = None;
        let mut damaged_lines = 0;
        let mut gap_pending = false;
        for record in records {
            let Some(record) = record else {
                damaged_lines += 1;
                continue;
            };
            let Some(session) = session.as_mut() else {
                // The header must come first; without it nothing is known.
                match record {
                    HistoryRecord::Session(header) => {
                        session = Some(Self {
                            id: id.clone(),
                            header,
                            segments: Vec::new(),
                            end: None,
                            gaps: 0,
                            ends_with_gap: false,
                            damaged_lines: 0,
                        });
                    }
                    _ => return None,
                }
                continue;
            };
            match record {
                HistoryRecord::Session(_) => damaged_lines += 1,
                HistoryRecord::Segment(segment) => {
                    // A segment repeated by a damaged writer is kept once.
                    if session.segments.iter().any(|known| known.id == segment.id) {
                        continue;
                    }
                    let after_gap = segment.after_gap || gap_pending;
                    gap_pending = false;
                    if after_gap {
                        session.gaps += 1;
                    }
                    session.segments.push(TranscriptSegment {
                        id: segment.id,
                        start: Duration::from_millis(segment.start_ms),
                        end: Duration::from_millis(segment.end_ms),
                        text: segment.text,
                        translations: Vec::new(),
                        after_gap,
                    });
                }
                HistoryRecord::Translation(translation) => {
                    // Translations of segments recorded before history was
                    // turned on have nothing to belong to.
                    if let Some(segment) = session
                        .segments
                        .iter_mut()
                        .rev()
                        .find(|segment| segment.id == translation.segment)
                    {
                        match segment
                            .translations
                            .iter_mut()
                            .find(|(language, _)| *language == translation.language)
                        {
                            Some((_, text)) => {
                                text.push(' ');
                                text.push_str(&translation.text);
                            }
                            None => segment
                                .translations
                                .push((translation.language, translation.text)),
                        }
                    }
                }
                HistoryRecord::Gap(_) => gap_pending = true,
                HistoryRecord::End(end) => session.end = Some(end),
            }
        }
        let mut session = session?;
        if gap_pending {
            session.gaps += 1;
            session.ends_with_gap = true;
        }
        session.damaged_lines = damaged_lines;
        Some(session)
    }
}

/// The saved sessions in one directory.
#[derive(Clone, Debug)]
pub struct HistoryStore {
    dir: PathBuf,
}

impl HistoryStore {
    /// The store in `dir`. Nothing is created until a session is saved.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Where sessions are saved.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &SessionId) -> PathBuf {
        self.dir.join(id.file_name())
    }

    /// Session files by id, skipping everything else in the directory.
    fn files(&self) -> io::Result<Vec<(SessionId, PathBuf, fs::Metadata)>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_suffix(&format!(".{EXTENSION}")))
                .and_then(SessionId::parse)
            else {
                continue;
            };
            // `symlink_metadata`: a link is never followed or deleted through.
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                // Deleted since the directory was read.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.is_file() {
                files.push((id, entry.path(), metadata));
            }
        }
        Ok(files)
    }

    /// Creates the directory, private to the user, if it is missing.
    pub fn ensure_dir(&self) -> io::Result<()> {
        create_private_dir(&self.dir)
    }

    /// Bytes used by saved sessions.
    pub fn usage(&self) -> io::Result<u64> {
        Ok(self
            .files()?
            .iter()
            .map(|(_, _, metadata)| metadata.len())
            .sum())
    }

    /// Lists the saved sessions, newest first. Only each file's first line
    /// and end are read, unless it never ended.
    pub fn list(&self) -> io::Result<Library> {
        let mut library = Library::default();
        for (id, path, metadata) in self.files()? {
            match summarize(&id, &path, metadata.len()) {
                Ok(Some(summary)) => library.sessions.push(summary),
                Ok(None) | Err(_) => library.unreadable += 1,
            }
        }
        library.sessions.sort_by(|left, right| {
            right
                .header
                .started_at_ms
                .cmp(&left.header.started_at_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(library)
    }

    /// Reads a whole session.
    pub fn load(&self, id: &SessionId) -> io::Result<HistorySession> {
        let file = File::open(self.path(id))?;
        HistorySession::from_records(id.clone(), records(BufReader::new(file)))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unreadable session"))
    }

    /// Deletes one session. Deleting one that is already gone succeeds.
    pub fn delete(&self, id: &SessionId) -> io::Result<()> {
        match fs::remove_file(self.path(id)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    /// Deletes every saved session but `keep` (the one being recorded), and
    /// nothing else in the directory. Returns how many were deleted.
    pub fn clear(&self, keep: Option<&SessionId>) -> io::Result<usize> {
        let mut deleted = 0;
        for (id, _, _) in self.files()? {
            if Some(&id) != keep {
                self.delete(&id)?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Creates the file of a new session, private to the user, and writes
    /// its header. An id that is taken gets a suffix.
    pub fn create(&self, id: SessionId, header: &SessionHeader) -> io::Result<HistoryFile> {
        create_private_dir(&self.dir)?;
        let mut candidate = id.clone();
        for suffix in 1..=16 {
            match open_private(&self.path(&candidate)) {
                Ok(file) => {
                    let mut file = HistoryFile {
                        path: self.path(&candidate),
                        id: candidate,
                        file,
                    };
                    file.append(&HistoryRecord::Session(header.clone()))?;
                    return Ok(file);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    candidate = id.with_suffix(suffix);
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free session name",
        ))
    }

    /// Ids of the sessions whose text contains `query`, ignoring case,
    /// newest first. Stops early, returning what it found, once `cancelled`.
    pub fn search(&self, library: &Library, query: &str, cancelled: &AtomicBool) -> Vec<SessionId> {
        let query = fold(query);
        if query.is_empty() {
            return library
                .sessions
                .iter()
                .map(|summary| summary.id.clone())
                .collect();
        }
        let mut found = Vec::new();
        for summary in &library.sessions {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            let Ok(file) = File::open(self.path(&summary.id)) else {
                continue;
            };
            let hit = records(BufReader::new(file))
                .flatten()
                .any(|record| match record {
                    HistoryRecord::Segment(SegmentRecord { text, .. })
                    | HistoryRecord::Translation(TranslationRecord { text, .. }) => {
                        contains_folded(&fold(&text), &query)
                    }
                    _ => false,
                });
            if hit {
                found.push(summary.id.clone());
            }
        }
        found
    }
}

/// An open session file, appended to one record at a time.
#[derive(Debug)]
pub struct HistoryFile {
    id: SessionId,
    path: PathBuf,
    file: File,
}

impl HistoryFile {
    /// The session.
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Whether the file is still saved under its name. The user may delete
    /// a session, or all of them, while it is being written; appending to
    /// the open file would then save nothing anyone can see.
    pub fn exists(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.is_file())
    }

    /// Appends one record as one line, written in a single call. Returns
    /// the bytes written.
    pub fn append(&mut self, record: &HistoryRecord) -> io::Result<u64> {
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        Ok(line.len() as u64)
    }

    /// Makes what was written durable.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// Cuts `text` to at most [`MAX_TEXT_BYTES`] at a character boundary.
/// Returns whether anything was cut.
pub fn bound_text(text: &mut String) -> bool {
    if text.len() <= MAX_TEXT_BYTES {
        return false;
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir)?;
        // An existing directory may have been created more openly.
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    builder.create(dir)
}

fn open_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Each line of a session file as a record, or `None` for a damaged line.
/// A line that is too long is skipped without being held in memory.
fn records(mut reader: impl BufRead) -> impl Iterator<Item = Option<HistoryRecord>> {
    let mut line = Vec::new();
    std::iter::from_fn(move || {
        line.clear();
        let read = (&mut reader)
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)
            .ok()?;
        if read == 0 {
            return None;
        }
        if line.last() != Some(&b'\n') && read as u64 > MAX_LINE_BYTES {
            // Skip the rest of the overlong line.
            let mut rest = Vec::new();
            while (&mut reader)
                .take(MAX_LINE_BYTES)
                .read_until(b'\n', &mut rest)
                .ok()
                .is_some_and(|read| read > 0 && rest.last() != Some(&b'\n'))
            {
                rest.clear();
            }
            return Some(None);
        }
        Some(serde_json::from_slice(&line).ok())
    })
}

fn summarize(id: &SessionId, path: &Path, size_bytes: u64) -> io::Result<Option<SessionSummary>> {
    let mut file = File::open(path)?;
    let header = match records(BufReader::new(&mut file)).next().flatten() {
        Some(HistoryRecord::Session(header)) => header,
        _ => return Ok(None),
    };
    let tail_start = size_bytes.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(tail_start))?;
    let mut tail = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut tail)?;
    let end = tail
        .split(|byte| *byte == b'\n')
        .rev()
        .find(|line| !line.is_empty())
        .and_then(|line| serde_json::from_slice::<HistoryRecord>(line).ok());
    if let Some(HistoryRecord::End(end)) = end {
        return Ok(Some(SessionSummary {
            id: id.clone(),
            header,
            end: Some(end),
            segments: end.segments,
            gaps: end.gaps,
            duration: Duration::from_millis(end.audio_ms),
            size_bytes,
        }));
    }
    // Never ended: count what is there.
    let file = File::open(path)?;
    Ok(
        HistorySession::from_records(id.clone(), records(BufReader::new(file))).map(|session| {
            SessionSummary {
                id: id.clone(),
                duration: session.duration(),
                segments: session.segments.len() as u64,
                gaps: session.gaps,
                end: session.end,
                header: session.header,
                size_bytes,
            }
        }),
    )
}

/// Estimates when accepted speech was spoken, from the audio clock.
///
/// Backends do not report word timing, so a segment is taken to span from
/// when captions first changed after the previous segment to when it was
/// accepted. That is close to the speech, but late by recognition latency;
/// cues are therefore approximate. Cues are ordered, never overlap, and
/// last at least 0.7 s.
#[derive(Clone, Debug, Default)]
pub struct CueClock {
    speech_since: Option<Duration>,
    last_end: Duration,
}

impl CueClock {
    /// Captions changed at `audio`.
    pub fn presented(&mut self, audio: Duration) {
        self.speech_since.get_or_insert(audio);
    }

    /// Cues for segments accepted together at `audio`, sharing the time in
    /// proportion to their `weights` (such as their lengths).
    pub fn cues(&mut self, audio: Duration, weights: &[usize]) -> Vec<(Duration, Duration)> {
        if weights.is_empty() {
            return Vec::new();
        }
        let start = self
            .speech_since
            .take()
            .unwrap_or(audio)
            .min(audio)
            .max(self.last_end);
        let span = audio.saturating_sub(start);
        let total: usize = weights.iter().map(|weight| (*weight).max(1)).sum();
        let mut at = start;
        let cues = weights
            .iter()
            .map(|weight| {
                let share = span.mul_f64((*weight).max(1) as f64 / total as f64);
                let cue = (at, at + share.max(MIN_CUE));
                at = cue.1;
                cue
            })
            .collect();
        self.last_end = at;
        cues
    }
}

/// Folds case for search, one character for one, so match positions in the
/// folded text are positions in the original.
fn fold(text: &str) -> Vec<char> {
    text.chars()
        .map(|character| character.to_lowercase().next().unwrap_or(character))
        .collect()
}

fn contains_folded(text: &[char], query: &[char]) -> bool {
    !query.is_empty() && text.windows(query.len()).any(|window| window == query)
}

/// Where `query` occurs in `text`, ignoring case, as character ranges.
///
/// Matching is exact apart from case: Vietnamese tone marks and other
/// diacritics must match, so "viet" does not find "Việt". Saved text is
/// never altered for searching.
pub fn find_matches(text: &str, query: &str) -> Vec<Range<usize>> {
    let (text, query) = (fold(text), fold(query));
    let mut matches = Vec::new();
    if query.is_empty() {
        return matches;
    }
    let mut start = 0;
    while start + query.len() <= text.len() {
        if text[start..start + query.len()] == query[..] {
            matches.push(start..start + query.len());
            start += query.len();
        } else {
            start += 1;
        }
    }
    matches
}

/// A file format to export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportFormat {
    /// Plain text with elapsed times.
    Text,
    /// SubRip subtitles.
    SubRip,
    /// WebVTT subtitles.
    WebVtt,
}

impl ExportFormat {
    /// All formats in presentation order.
    pub const ALL: [Self; 3] = [Self::Text, Self::SubRip, Self::WebVtt];

    /// The file name extension.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Text => "txt",
            Self::SubRip => "srt",
            Self::WebVtt => "vtt",
        }
    }

    /// User-facing name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Plain Text (.txt)",
            Self::SubRip => "SubRip (.srt)",
            Self::WebVtt => "WebVTT (.vtt)",
        }
    }
}

/// Which text to export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportContent {
    /// The source speech.
    Original,
    /// The translation into one language.
    Translation(Language),
    /// The source speech with its translation into one language.
    Bilingual(Language),
}

impl ExportContent {
    /// User-facing name.
    pub fn label(self) -> String {
        match self {
            Self::Original => "Original".to_owned(),
            Self::Translation(language) => format!("{} translation", language.label()),
            Self::Bilingual(language) => format!("Original + {}", language.label()),
        }
    }

    /// The lines of `segment` this content shows, or `None` when it has
    /// nothing to show for it.
    fn lines(self, segment: &TranscriptSegment) -> Option<Vec<String>> {
        let original = one_line(&segment.text);
        match self {
            Self::Original => (!original.is_empty()).then(|| vec![original]),
            Self::Translation(language) => segment
                .translation(language)
                .map(one_line)
                .filter(|text| !text.is_empty())
                .map(|text| vec![text]),
            Self::Bilingual(language) => {
                let mut lines = vec![original];
                lines.extend(segment.translation(language).map(one_line));
                lines.retain(|line| !line.is_empty());
                (!lines.is_empty()).then_some(lines)
            }
        }
    }
}

/// Whitespace and line breaks collapsed, so text can't end a cue early.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Exports `session` deterministically. `title` heads a text export, such
/// as the session's name and local start time.
pub fn export(
    session: &HistorySession,
    format: ExportFormat,
    content: ExportContent,
    title: &str,
) -> String {
    match format {
        ExportFormat::Text => export_text(session, content, title),
        ExportFormat::SubRip | ExportFormat::WebVtt => export_subtitles(session, format, content),
    }
}

fn export_text(session: &HistorySession, content: ExportContent, title: &str) -> String {
    let mut out = String::new();
    out.push_str(&one_line(title));
    out.push('\n');
    out.push_str(&content.label());
    out.push_str(" · times are approximate, from the start of the session\n");
    if session.header.started_late {
        out.push_str("Saving began after captions had started; earlier speech is not included.\n");
    }
    out.push('\n');
    for segment in &session.segments {
        if segment.after_gap {
            out.push_str("[Some speech here was not saved.]\n\n");
        }
        let lines = match content {
            ExportContent::Translation(language) | ExportContent::Bilingual(language)
                if segment.translation(language).is_none() =>
            {
                vec![one_line(&segment.text), "[No translation saved]".to_owned()]
            }
            _ => content.lines(segment).unwrap_or_default(),
        };
        let stamp = format!("[{}] ", clock_time(segment.start));
        let indent = " ".repeat(stamp.len());
        for (index, line) in lines.iter().enumerate() {
            out.push_str(if index == 0 { &stamp } else { &indent });
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    if session.ends_with_gap {
        out.push_str("[Some speech here was not saved.]\n");
    }
    out
}

fn export_subtitles(
    session: &HistorySession,
    format: ExportFormat,
    content: ExportContent,
) -> String {
    let mut out = String::new();
    if format == ExportFormat::WebVtt {
        out.push_str("WEBVTT\n\nNOTE Exported from LILOPOP. Cue times are approximate.\n\n");
    }
    let mut number = 0;
    let mut last_end = Duration::ZERO;
    for segment in &session.segments {
        let Some(lines) = content.lines(segment) else {
            continue;
        };
        // A long single-language segment becomes several cues of at most
        // two lines, sharing its time by length; a bilingual cue keeps
        // both languages together.
        let groups: Vec<Vec<String>> = match content {
            ExportContent::Bilingual(_) => vec![lines.iter().flat_map(|line| wrap(line)).collect()],
            _ => wrap(&lines[0])
                .chunks(SUBTITLE_CUE_LINES)
                .map(<[String]>::to_vec)
                .collect(),
        };
        let start = segment.start.max(last_end);
        let end = segment.end.max(start + MIN_CUE);
        let total: usize = groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|line| line.chars().count())
                    .sum::<usize>()
                    .max(1)
            })
            .sum();
        let mut at = start;
        for group in &groups {
            let weight = group
                .iter()
                .map(|line| line.chars().count())
                .sum::<usize>()
                .max(1);
            let cue_end = (at + (end - start).mul_f64(weight as f64 / total as f64))
                .max(at + Duration::from_millis(1));
            number += 1;
            if format == ExportFormat::SubRip {
                out.push_str(&format!("{number}\n"));
            }
            out.push_str(&format!(
                "{} --> {}\n",
                cue_time(at, format),
                cue_time(cue_end, format)
            ));
            for line in group {
                if format == ExportFormat::WebVtt {
                    out.push_str(&escape_vtt(line));
                } else {
                    out.push_str(&line.replace("-->", "→"));
                }
                out.push('\n');
            }
            out.push('\n');
            at = cue_end;
        }
        last_end = at;
    }
    out
}

fn escape_vtt(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `HH:MM:SS,mmm` for SubRip, `HH:MM:SS.mmm` for WebVTT.
fn cue_time(time: Duration, format: ExportFormat) -> String {
    let millis = time.as_millis();
    let separator = if format == ExportFormat::SubRip {
        ','
    } else {
        '.'
    };
    format!(
        "{:02}:{:02}:{:02}{separator}{:03}",
        millis / 3_600_000,
        millis / 60_000 % 60,
        millis / 1_000 % 60,
        millis % 1_000
    )
}

/// Elapsed time as `MM:SS`, or `H:MM:SS` from an hour.
pub fn clock_time(time: Duration) -> String {
    let seconds = time.as_secs();
    if seconds >= 3_600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3_600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

/// Whether a character takes two columns, as CJK text does.
fn is_wide(character: char) -> bool {
    matches!(character as u32,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD)
}

fn columns(character: char) -> usize {
    if is_wide(character) { 2 } else { 1 }
}

/// Wraps one line of text to subtitle width: at spaces in spaced scripts,
/// and between characters in CJK text, which has no spaces.
fn wrap(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut width = 0;
    // Byte offset in `line` where it may break.
    let mut last_break: Option<usize> = None;
    for character in text.chars() {
        let char_width = columns(character);
        if width + char_width > SUBTITLE_LINE_WIDTH && !line.is_empty() {
            let cut = last_break.unwrap_or(line.len());
            let rest = line.split_off(cut);
            lines.push(line.trim_end().to_owned());
            line = rest.trim_start().to_owned();
            width = line.chars().map(columns).sum();
            last_break = None;
        }
        if character == ' ' && line.is_empty() {
            continue;
        }
        if is_wide(character) && !line.is_empty() {
            last_break = Some(line.len());
        }
        line.push(character);
        width += char_width;
        if character == ' ' || is_wide(character) {
            last_break = Some(line.len());
        }
    }
    if !line.trim().is_empty() {
        lines.push(line.trim_end().to_owned());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> SessionHeader {
        SessionHeader {
            version: FORMAT_VERSION,
            started_at_ms: 1_760_000_000_000,
            mode: ProcessingMode::OfflineTranslation,
            spoken_language: LanguageSelection::Language(Language::Japanese),
            targets: vec![Language::English],
            source: SourceKind::SystemAudio,
            started_late: false,
        }
    }

    fn segment(id: u64, start_ms: u64, end_ms: u64, text: &str) -> HistoryRecord {
        HistoryRecord::Segment(SegmentRecord {
            id,
            start_ms,
            end_ms,
            text: text.to_owned(),
            after_gap: false,
        })
    }

    fn translation(segment: u64, language: Language, text: &str) -> HistoryRecord {
        HistoryRecord::Translation(TranslationRecord {
            segment,
            language,
            text: text.to_owned(),
        })
    }

    fn end(segments: u64) -> HistoryRecord {
        HistoryRecord::End(EndRecord {
            ended_at_ms: 1_760_000_060_000,
            reason: EndReason::Completed,
            audio_ms: 60_000,
            segments,
            gaps: 0,
        })
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lcrt-history-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn save(store: &HistoryStore, id: &str, records: &[HistoryRecord]) -> SessionId {
        let mut file = store
            .create(SessionId::parse(id).unwrap(), &header())
            .unwrap();
        for record in records {
            file.append(record).unwrap();
        }
        file.id().clone()
    }

    fn fixture() -> HistorySession {
        let lines = [
            HistoryRecord::Session(header()),
            segment(1, 1_000, 4_000, "こんにちは。"),
            segment(
                2,
                4_000,
                9_000,
                "今日は新しいプロジェクトについて話します。",
            ),
            translation(1, Language::English, "Hello."),
            translation(
                2,
                Language::English,
                "Today we discuss a <new> project & more.",
            ),
            HistoryRecord::Gap(GapRecord {
                reason: GapReason::Overloaded,
            }),
            segment(
                3,
                9_000,
                9_100,
                "Hôm nay chúng ta thảo luận về dự án mới 🎉",
            ),
            end(3),
        ];
        HistorySession::from_records(
            SessionId::parse("1-1").unwrap(),
            lines.into_iter().map(Some),
        )
        .unwrap()
    }

    #[test]
    fn session_ids_cannot_name_other_files() {
        assert!(SessionId::parse("1760000000000-3").is_some());
        for bad in ["", "-1", "../etc", "1/2", "12a", "1.jsonl", "1-1\n"] {
            assert!(SessionId::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn translations_join_their_segments_in_speech_order() {
        let session = fixture();
        let texts: Vec<_> = session.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts.len(), 3);
        assert_eq!(
            session.segments[0].translation(Language::English),
            Some("Hello.")
        );
        assert_eq!(session.segments[2].translation(Language::English), None);
        assert!(session.segments[2].after_gap);
        assert_eq!(session.gaps, 1);
        assert_eq!(
            SessionStatus::of(&session.header, session.end.as_ref(), session.gaps, false),
            SessionStatus::Incomplete
        );
    }

    #[test]
    fn a_crash_leaves_an_interrupted_session_whose_text_is_kept() {
        let store = HistoryStore::new(temp_dir("crash"));
        let id = save(&store, "5-1", &[segment(1, 0, 1_000, "first")]);
        // A write cut short by the crash.
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.path(&id))
            .unwrap();
        file.write_all(br#"{"type":"segment","id":2,"start_ms":1000,"en"#)
            .unwrap();
        let session = store.load(&id).unwrap();
        assert_eq!(session.segments.len(), 1);
        assert_eq!(session.damaged_lines, 1);
        let library = store.list().unwrap();
        assert_eq!(library.sessions.len(), 1);
        let summary = &library.sessions[0];
        assert_eq!(summary.segments, 1);
        assert_eq!(
            SessionStatus::of(&summary.header, summary.end.as_ref(), summary.gaps, false),
            SessionStatus::Interrupted
        );
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn a_session_survives_reopening_and_lists_from_its_end_record() {
        let dir = temp_dir("reopen");
        let id = save(
            &HistoryStore::new(&dir),
            "7-2",
            &[segment(1, 0, 900, "hello"), end(1)],
        );
        let store = HistoryStore::new(&dir);
        let library = store.list().unwrap();
        assert_eq!(library.sessions[0].id, id);
        assert_eq!(library.sessions[0].segments, 1);
        assert_eq!(library.sessions[0].duration, Duration::from_secs(60));
        assert_eq!(store.load(&id).unwrap().segments[0].text, "hello");
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn history_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let store = HistoryStore::new(temp_dir("private"));
        let id = save(&store, "8-1", &[]);
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.dir()), 0o700);
        assert_eq!(mode(&store.path(&id)), 0o600);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn clearing_deletes_only_history() {
        let store = HistoryStore::new(temp_dir("clear"));
        save(&store, "9-1", &[end(0)]);
        save(&store, "9-2", &[end(0)]);
        let other = store.dir().join("preferences.json");
        fs::write(&other, "{}").unwrap();
        fs::write(store.dir().join("notes.jsonl"), "{}").unwrap();
        let recording = save(&store, "9-3", &[]);
        assert_eq!(store.clear(Some(&recording)).unwrap(), 2);
        assert!(other.exists());
        assert!(store.dir().join("notes.jsonl").exists());
        let left = store.list().unwrap();
        assert_eq!(left.sessions.len(), 1);
        assert_eq!(left.sessions[0].id, recording);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn an_open_file_knows_when_it_was_deleted() {
        let store = HistoryStore::new(temp_dir("deleted"));
        let file = store
            .create(SessionId::parse("13-1").unwrap(), &header())
            .unwrap();
        assert!(file.exists());
        store.clear(None).unwrap();
        assert!(!file.exists());
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn a_taken_id_gets_a_new_name_instead_of_overwriting() {
        let store = HistoryStore::new(temp_dir("taken"));
        let first = save(&store, "10-1", &[segment(1, 0, 1, "first")]);
        let second = save(&store, "10-1", &[]);
        assert_ne!(first, second);
        assert_eq!(store.load(&first).unwrap().segments.len(), 1);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn a_damaged_header_is_counted_not_listed() {
        let store = HistoryStore::new(temp_dir("damaged"));
        create_private_dir(store.dir()).unwrap();
        fs::write(store.dir().join("11-1.jsonl"), "not json\n").unwrap();
        fs::write(store.dir().join("11-2.jsonl"), "").unwrap();
        let library = store.list().unwrap();
        assert!(library.sessions.is_empty());
        assert_eq!(library.unreadable, 2);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn search_finds_japanese_vietnamese_and_english_ignoring_case() {
        let text = "今日は新しいプロジェクト Hôm nay DỰ ÁN mới Hello World";
        assert_eq!(find_matches(text, "プロジェクト"), vec![6..12]);
        assert_eq!(find_matches(text, "dự án").len(), 1);
        assert_eq!(find_matches(text, "hello world").len(), 1);
        // Diacritics are significant: they change Vietnamese words.
        assert!(find_matches(text, "du an").is_empty());
        assert!(find_matches(text, "").is_empty());
        assert!(find_matches("ab", "abc").is_empty());
        assert_eq!(find_matches("aaaa", "aa"), vec![0..2, 2..4]);
    }

    #[test]
    fn search_lists_sessions_whose_text_matches() {
        let store = HistoryStore::new(temp_dir("search"));
        let japanese = save(&store, "12-1", &[segment(1, 0, 1, "東京で会議"), end(1)]);
        save(&store, "12-2", &[segment(1, 0, 1, "Xin chào"), end(1)]);
        let library = store.list().unwrap();
        let never = AtomicBool::new(false);
        assert_eq!(store.search(&library, "会議", &never), vec![japanese]);
        assert!(store.search(&library, "nothing", &never).is_empty());
        assert_eq!(store.search(&library, "", &never).len(), 2);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn cues_are_ordered_never_overlap_and_are_readable() {
        let mut clock = CueClock::default();
        clock.presented(Duration::from_secs(2));
        let first = clock.cues(Duration::from_secs(5), &[10, 30]);
        assert_eq!(first[0].0, Duration::from_secs(2));
        assert_eq!(first[0].1, first[1].0);
        assert_eq!(first[1].1, Duration::from_secs(5));
        // Accepted with no caption change before it: a short, valid cue
        // that never starts before the previous one ended.
        let second = clock.cues(Duration::from_secs(5), &[4]);
        assert_eq!(second[0].0, Duration::from_secs(5));
        assert!(second[0].1 - second[0].0 >= MIN_CUE);
        let third = clock.cues(Duration::from_millis(5_100), &[4]);
        assert!(third[0].0 >= second[0].1);
    }

    fn assert_valid_subtitles(text: &str, format: ExportFormat) -> usize {
        let separator = if format == ExportFormat::SubRip {
            ','
        } else {
            '.'
        };
        let body = match format {
            ExportFormat::WebVtt => text.strip_prefix("WEBVTT\n\n").expect("WEBVTT header"),
            _ => text,
        };
        let parse = |stamp: &str| -> u64 {
            let (hms, millis) = stamp.split_once(separator).unwrap();
            let parts: Vec<u64> = hms.split(':').map(|p| p.parse().unwrap()).collect();
            assert_eq!(parts.len(), 3);
            assert_eq!(millis.len(), 3);
            ((parts[0] * 60 + parts[1]) * 60 + parts[2]) * 1_000 + millis.parse::<u64>().unwrap()
        };
        let mut previous_end = 0;
        let mut cues = 0;
        for block in body.split("\n\n").filter(|block| !block.trim().is_empty()) {
            let mut lines = block.lines();
            if block.starts_with("NOTE") {
                continue;
            }
            if format == ExportFormat::SubRip {
                assert_eq!(lines.next().unwrap(), (cues + 1).to_string());
            }
            let (start, end) = lines.next().unwrap().split_once(" --> ").unwrap();
            let (start, end) = (parse(start), parse(end));
            assert!(start < end && start >= previous_end, "{block}");
            previous_end = end;
            let text: Vec<_> = lines.collect();
            assert!(!text.is_empty() && text.iter().all(|line| !line.trim().is_empty()));
            assert!(!text.iter().any(|line| line.contains("-->")));
            cues += 1;
        }
        cues
    }

    #[test]
    fn subtitle_exports_are_valid_ordered_and_deterministic() {
        let session = fixture();
        for format in [ExportFormat::SubRip, ExportFormat::WebVtt] {
            for content in session.export_contents() {
                let text = export(&session, format, content, "title");
                assert!(assert_valid_subtitles(&text, format) > 0, "{content:?}");
                assert_eq!(text, export(&session, format, content, "title"));
            }
        }
        let vtt = export(
            &session,
            ExportFormat::WebVtt,
            ExportContent::Translation(Language::English),
            "",
        );
        assert!(vtt.contains("a &lt;new&gt; project &amp; more."));
        let srt = export(&session, ExportFormat::SubRip, ExportContent::Original, "");
        assert!(srt.starts_with("1\n00:00:01,000 --> 00:00:04,000\nこんにちは。\n"));
        // The 100 ms segment still gets a readable cue.
        assert!(srt.contains("00:00:09,000 --> 00:00:09,700"));
    }

    #[test]
    fn long_segments_wrap_and_split_into_short_cues() {
        let long =
            "今日は新しいプロジェクトについて話し合いますそしてこれからの計画を詳しく説明します"
                .repeat(3);
        let lines = [
            HistoryRecord::Session(header()),
            segment(1, 0, 30_000, &long),
            segment(2, 30_000, 40_000, &"word ".repeat(40)),
        ];
        let session = HistorySession::from_records(
            SessionId::parse("2-2").unwrap(),
            lines.into_iter().map(Some),
        )
        .unwrap();
        let srt = export(&session, ExportFormat::SubRip, ExportContent::Original, "");
        assert!(assert_valid_subtitles(&srt, ExportFormat::SubRip) > 2);
        for line in srt.lines().filter(|line| !line.contains("-->")) {
            let width: usize = line.chars().map(columns).sum();
            assert!(width <= SUBTITLE_LINE_WIDTH, "{line}");
        }
        assert!(srt.contains("word word"));
        assert!(!srt.contains("wor\n"));
    }

    #[test]
    fn text_export_marks_gaps_missing_translations_and_a_late_start() {
        let mut session = fixture();
        session.header.started_late = true;
        let text = export(
            &session,
            ExportFormat::Text,
            ExportContent::Bilingual(Language::English),
            "Japanese · System audio",
        );
        assert!(text.starts_with("Japanese · System audio\n"));
        assert!(text.contains("earlier speech is not included"));
        assert!(text.contains("[00:01] こんにちは。\n        Hello.\n"));
        assert!(text.contains("[Some speech here was not saved.]"));
        assert!(text.contains("[No translation saved]"));
    }
}
