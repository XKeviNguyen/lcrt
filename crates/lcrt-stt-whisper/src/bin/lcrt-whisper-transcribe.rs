use std::{
    env,
    error::Error,
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

use hound::{SampleFormat, WavReader};
use lcrt_core::{AudioChunk, CaptionStatus, Transcriber, TranscriptUpdate};
use lcrt_stt_whisper::{WhisperConfig, WhisperTranscriber};

const OFFLINE_CHUNK_FRAMES: usize = 4_096;
const PACED_CHUNK_DURATION: Duration = Duration::from_millis(20);
const OFFLINE_QUEUE_CAPACITY: usize = 8;
const INPUT_BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(30);
const USAGE: &str = "usage:\n  lcrt-whisper-transcribe <model.bin> <audio.wav> [language]\n  lcrt-whisper-transcribe benchmark <paced|offline> <model.bin> <audio.wav> [language]";

#[derive(Clone, Copy)]
enum BenchmarkMode {
    Paced,
    Offline,
}

impl BenchmarkMode {
    fn name(self) -> &'static str {
        match self {
            Self::Paced => "paced",
            Self::Offline => "offline",
        }
    }
}

struct Input {
    model_path: PathBuf,
    wav_path: PathBuf,
    language: Option<String>,
}

struct WavAudio {
    samples: Vec<f32>,
    sample_rate: u32,
    channels: u16,
}

impl WavAudio {
    fn duration(&self) -> Duration {
        duration_for_frames(
            self.samples.len() / usize::from(self.channels),
            self.sample_rate,
        )
    }
}

#[derive(Default)]
struct TranscriptMetrics {
    first_partial: Option<Duration>,
    word_milestones: [Option<(Duration, String)>; 3],
    first_final: Option<Duration>,
    latest_text: String,
    update_count: usize,
}

impl TranscriptMetrics {
    fn observe(&mut self, updates: Vec<TranscriptUpdate>, elapsed: Duration) {
        for update in updates {
            // Match the JFK benchmark normalization: remove ASCII punctuation,
            // then count whitespace-separated tokens. Revisions never reset a milestone.
            let normalized: String = update
                .text()
                .chars()
                .filter(|c| !c.is_ascii_punctuation())
                .collect();
            let words = normalized.split_whitespace().count();
            for (threshold, milestone) in [1, 3, 5].into_iter().zip(&mut self.word_milestones) {
                if words >= threshold && milestone.is_none() {
                    *milestone = Some((elapsed, update.text().to_owned()));
                }
            }
            match update.status() {
                CaptionStatus::Partial if self.first_partial.is_none() => {
                    self.first_partial = Some(elapsed);
                }
                CaptionStatus::Final if self.first_final.is_none() => {
                    self.first_final = Some(elapsed);
                }
                CaptionStatus::Partial | CaptionStatus::Final => {}
            }
            self.latest_text = update.text().to_owned();
            self.update_count += 1;
        }
    }
}

struct BenchmarkResult {
    mode: BenchmarkMode,
    model_startup: Duration,
    audio_duration: Duration,
    first_partial: Option<Duration>,
    word_milestones: [Option<(Duration, String)>; 3],
    first_final: Option<Duration>,
    completion: Duration,
    real_time_factor: Option<f64>,
    inference_count: u64,
    update_count: usize,
    transcript: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let first = arguments.next().ok_or(USAGE)?;
    if first == "benchmark" {
        let mode = parse_benchmark_mode(arguments.next())?;
        let input = parse_input(arguments)?;
        return run_benchmark(mode, input);
    }

    run_transcription(parse_input_with_model(first, arguments)?)
}

fn parse_benchmark_mode(value: Option<OsString>) -> Result<BenchmarkMode, Box<dyn Error>> {
    match value.as_deref().and_then(|value| value.to_str()) {
        Some("paced") => Ok(BenchmarkMode::Paced),
        Some("offline") => Ok(BenchmarkMode::Offline),
        _ => Err(USAGE.into()),
    }
}

fn parse_input(mut arguments: impl Iterator<Item = OsString>) -> Result<Input, Box<dyn Error>> {
    let model_path = arguments.next().ok_or(USAGE)?;
    parse_input_with_model(model_path, arguments)
}

fn parse_input_with_model(
    model_path: OsString,
    mut arguments: impl Iterator<Item = OsString>,
) -> Result<Input, Box<dyn Error>> {
    let wav_path = arguments.next().ok_or(USAGE)?;
    let language = arguments
        .next()
        .map(|value| value.into_string().map_err(|_| "language must be UTF-8"))
        .transpose()?;
    if arguments.next().is_some() {
        return Err(USAGE.into());
    }
    Ok(Input {
        model_path: model_path.into(),
        wav_path: wav_path.into(),
        language,
    })
}

fn run_transcription(input: Input) -> Result<(), Box<dyn Error>> {
    let audio = read_wav(&input.wav_path)?;
    let mut config = config_for(&input);
    // Keep finite-file input close enough to completed inference that shutdown
    // never inherits a live-capture-sized backlog.
    config.input_queue_capacity = OFFLINE_QUEUE_CAPACITY;
    let mut transcriber = WhisperTranscriber::new(config)?;
    let samples_per_chunk = OFFLINE_CHUNK_FRAMES * usize::from(audio.channels);
    let mut update_count = 0_usize;
    let mut chunk_count = 0_usize;
    for samples in audio.samples.chunks(samples_per_chunk) {
        let chunk = AudioChunk::new(samples.to_vec(), audio.sample_rate, audio.channels)?;
        update_count +=
            print_updates(transcriber.push_audio_with_timeout(chunk, INPUT_BACKPRESSURE_TIMEOUT)?);
        chunk_count += 1;
    }
    update_count += print_updates(transcriber.finish()?);
    let inference_count = transcriber.inference_count();
    eprintln!(
        "processed {chunk_count} bounded audio chunks with {inference_count} inference passes and emitted {update_count} transcript updates"
    );
    Ok(())
}

fn run_benchmark(mode: BenchmarkMode, input: Input) -> Result<(), Box<dyn Error>> {
    let audio = read_wav(&input.wav_path)?;
    let mut config = config_for(&input);
    if matches!(mode, BenchmarkMode::Offline) {
        config.input_queue_capacity = OFFLINE_QUEUE_CAPACITY;
    }

    let startup_started = Instant::now();
    let mut transcriber = WhisperTranscriber::new(config)?;
    let model_startup = startup_started.elapsed();
    let replay_started = Instant::now();
    let mut transcript = TranscriptMetrics::default();

    match mode {
        BenchmarkMode::Paced => {
            replay_paced(&audio, &mut transcriber, replay_started, &mut transcript)?;
        }
        BenchmarkMode::Offline => {
            replay_offline(&audio, &mut transcriber, replay_started, &mut transcript)?;
        }
    }

    transcript.observe(transcriber.finish()?, replay_started.elapsed());
    let completion = replay_started.elapsed();
    let inference_count = transcriber.inference_count();
    let real_time_factor =
        matches!(mode, BenchmarkMode::Offline).then(|| calculate_rtf(completion, audio.duration()));
    let result = BenchmarkResult {
        mode,
        model_startup,
        audio_duration: audio.duration(),
        first_partial: transcript.first_partial,
        word_milestones: transcript.word_milestones,
        first_final: transcript.first_final,
        completion,
        real_time_factor,
        inference_count,
        update_count: transcript.update_count,
        transcript: transcript.latest_text,
    };
    println!("{}", benchmark_json(&result));
    Ok(())
}

fn config_for(input: &Input) -> WhisperConfig {
    let mut config = WhisperConfig::new(&input.model_path);
    config.language.clone_from(&input.language);
    config
}

fn replay_paced(
    audio: &WavAudio,
    transcriber: &mut WhisperTranscriber,
    replay_started: Instant,
    transcript: &mut TranscriptMetrics,
) -> Result<(), Box<dyn Error>> {
    let frames_per_chunk = paced_chunk_frames(audio.sample_rate);
    let samples_per_chunk = frames_per_chunk * usize::from(audio.channels);
    let mut delivered_frames = 0_usize;
    for samples in audio.samples.chunks(samples_per_chunk) {
        delivered_frames += samples.len() / usize::from(audio.channels);
        sleep_until(replay_started + duration_for_frames(delivered_frames, audio.sample_rate));
        let chunk = AudioChunk::new(samples.to_vec(), audio.sample_rate, audio.channels)?;
        transcript.observe(transcriber.push_audio(chunk)?, replay_started.elapsed());
    }
    Ok(())
}

fn replay_offline(
    audio: &WavAudio,
    transcriber: &mut WhisperTranscriber,
    replay_started: Instant,
    transcript: &mut TranscriptMetrics,
) -> Result<(), Box<dyn Error>> {
    let samples_per_chunk = OFFLINE_CHUNK_FRAMES * usize::from(audio.channels);
    for samples in audio.samples.chunks(samples_per_chunk) {
        let chunk = AudioChunk::new(samples.to_vec(), audio.sample_rate, audio.channels)?;
        transcript.observe(
            transcriber.push_audio_with_timeout(chunk, INPUT_BACKPRESSURE_TIMEOUT)?,
            replay_started.elapsed(),
        );
    }
    Ok(())
}

fn sleep_until(deadline: Instant) {
    if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        thread::sleep(remaining);
    }
}

fn paced_chunk_frames(sample_rate: u32) -> usize {
    let frames = u64::from(sample_rate) * PACED_CHUNK_DURATION.as_millis() as u64 / 1_000;
    usize::try_from(frames.max(1)).expect("sample rate and chunk duration fit in usize")
}

fn duration_for_frames(frames: usize, sample_rate: u32) -> Duration {
    Duration::from_secs_f64(frames as f64 / f64::from(sample_rate))
}

fn calculate_rtf(processing: Duration, audio: Duration) -> f64 {
    processing.as_secs_f64() / audio.as_secs_f64()
}

fn benchmark_json(result: &BenchmarkResult) -> String {
    let milestones = [1, 3, 5]
        .into_iter()
        .zip(&result.word_milestones)
        .map(|(words, milestone)| {
            let (ms, text) = milestone.as_ref().map_or_else(
                || ("null".to_owned(), "null".to_owned()),
                |(elapsed, text)| {
                    (
                        optional_milliseconds(Some(*elapsed)),
                        format!("\"{}\"", escape_json(text)),
                    )
                },
            );
            format!("{{\"words\":{words},\"ms\":{ms},\"text\":{text}}}")
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        concat!(
            "{{\"schema_version\":1,\"word_milestones\":[{}],\"mode\":\"{}\",",
            "\"model_startup_ms\":{:.3},\"audio_duration_ms\":{:.3},",
            "\"first_partial_ms\":{},\"first_final_ms\":{},",
            "\"completion_ms\":{:.3},\"real_time_factor\":{},",
            "\"inference_count\":{},\"update_count\":{},\"transcript\":\"{}\"}}"
        ),
        milestones,
        result.mode.name(),
        milliseconds(result.model_startup),
        milliseconds(result.audio_duration),
        optional_milliseconds(result.first_partial),
        optional_milliseconds(result.first_final),
        milliseconds(result.completion),
        result
            .real_time_factor
            .map_or_else(|| "null".to_owned(), |value| format!("{value:.6}")),
        result.inference_count,
        result.update_count,
        escape_json(&result.transcript),
    )
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn optional_milliseconds(duration: Option<Duration>) -> String {
    duration.map_or_else(
        || "null".to_owned(),
        |value| format!("{:.3}", milliseconds(value)),
    )
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0c}' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character <= '\u{1f}' => {
                use std::fmt::Write as _;
                write!(escaped, "\\u{:04x}", u32::from(character))
                    .expect("writing to a String cannot fail");
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn read_wav(path: &Path) -> Result<WavAudio, Box<dyn Error>> {
    let mut reader = WavReader::open(path)?;
    let spec = reader.spec();
    if spec.sample_rate == 0 || spec.channels == 0 {
        return Err("WAV sample rate and channel count must be greater than zero".into());
    }
    let samples = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|sample| sample.map(|value| f32::from(value) / f32::from(i16::MAX)))
            .collect::<Result<Vec<_>, _>>()?,
        (SampleFormat::Float, 32) => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(format!(
                "unsupported WAV encoding: {:?}/{}-bit; use signed 16-bit PCM or 32-bit float",
                spec.sample_format, spec.bits_per_sample
            )
            .into());
        }
    };
    if samples.is_empty() {
        return Err("WAV contains no audio samples".into());
    }
    if samples.len() % usize::from(spec.channels) != 0 {
        return Err("WAV contains an incomplete interleaved audio frame".into());
    }
    Ok(WavAudio {
        samples,
        sample_rate: spec.sample_rate,
        channels: spec.channels,
    })
}

fn print_updates(updates: Vec<TranscriptUpdate>) -> usize {
    let count = updates.len();
    for update in updates {
        println!("{:?}: {}", update.status(), update.text());
    }
    count
}

#[cfg(test)]
mod tests {
    use super::{
        BenchmarkMode, BenchmarkResult, TranscriptMetrics, benchmark_json, calculate_rtf,
        duration_for_frames, escape_json, paced_chunk_frames,
    };
    use lcrt_core::TranscriptUpdate;
    use std::time::Duration;

    #[test]
    fn calculates_audio_duration_and_paced_chunk_frames() {
        assert_eq!(paced_chunk_frames(16_000), 320);
        assert_eq!(paced_chunk_frames(48_000), 960);
        assert_eq!(
            duration_for_frames(176_000, 16_000),
            Duration::from_secs(11)
        );
    }

    #[test]
    fn calculates_offline_real_time_factor() {
        let rtf = calculate_rtf(Duration::from_millis(2_750), Duration::from_secs(11));
        assert!((rtf - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn records_only_first_partial_and_final_timestamps() {
        let mut metrics = TranscriptMetrics::default();
        metrics.observe(
            vec![TranscriptUpdate::partial("first").unwrap()],
            Duration::from_millis(800),
        );
        metrics.observe(
            vec![
                TranscriptUpdate::partial("second").unwrap(),
                TranscriptUpdate::finalized("done").unwrap(),
            ],
            Duration::from_millis(1_900),
        );

        assert_eq!(metrics.first_partial, Some(Duration::from_millis(800)));
        assert_eq!(metrics.first_final, Some(Duration::from_millis(1_900)));
        assert_eq!(metrics.latest_text, "done");
        assert_eq!(metrics.update_count, 3);
    }

    #[test]
    fn word_milestones_ignore_punctuation_and_survive_revisions() {
        let mut metrics = TranscriptMetrics::default();
        for (ms, text) in [(10, "..."), (20, "And,"), (30, "And so, my"), (40, "And")] {
            metrics.observe(
                vec![TranscriptUpdate::partial(text).unwrap()],
                Duration::from_millis(ms),
            );
        }
        assert!(metrics.word_milestones[2].is_none());
        metrics.observe(
            vec![TranscriptUpdate::finalized("And so, my fellow Americans.").unwrap()],
            Duration::from_millis(50),
        );
        for (milestone, ms) in metrics.word_milestones.iter().zip([20, 30, 50]) {
            assert_eq!(milestone.as_ref().unwrap().0, Duration::from_millis(ms));
        }
        assert_eq!(metrics.word_milestones[1].as_ref().unwrap().1, "And so, my");
        let json = benchmark_json(&BenchmarkResult {
            mode: BenchmarkMode::Paced,
            model_startup: Duration::ZERO,
            audio_duration: Duration::from_secs(11),
            first_partial: metrics.first_partial,
            word_milestones: metrics.word_milestones,
            first_final: metrics.first_final,
            completion: Duration::from_millis(50),
            real_time_factor: None,
            inference_count: 1,
            update_count: metrics.update_count,
            transcript: metrics.latest_text,
        });
        assert!(json.contains("\"words\":3,\"ms\":30.000,\"text\":\"And so, my\""));
    }

    #[test]
    fn escapes_transcript_as_json_string_content() {
        assert_eq!(
            escape_json("say \"hi\"\\next\n"),
            "say \\\"hi\\\"\\\\next\\n"
        );
    }
}
