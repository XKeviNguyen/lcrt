//! Isolated, single-threaded driver for the official online recognizer.
use serde_json::{Value, json};
use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineStream};
use std::{
    error::Error,
    path::Path,
    thread,
    time::{Duration, Instant},
};

const RATE: usize = 16_000;
const CHUNK: usize = 320;

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
fn deadline(samples: usize) -> Duration {
    Duration::from_secs_f64(samples as f64 / RATE as f64)
}

#[derive(Default)]
struct Observations {
    first_partial: Option<f64>,
    first_text: String,
    first_final: Option<f64>,
    final_kind: Option<&'static str>,
    committed: Vec<String>,
    current: String,
    changes: Vec<Value>,
    decode_calls: usize,
    decode_time: Duration,
    endpoints: usize,
}

impl Observations {
    fn observe(&mut self, text: String, elapsed: Duration, trace: bool) {
        if !text.trim().is_empty() && self.first_partial.is_none() {
            self.first_partial = Some(milliseconds(elapsed));
            self.first_text.clone_from(&text);
        }
        if text != self.current {
            if trace {
                self.changes
                    .push(json!({"ms": milliseconds(elapsed), "text": text}));
            }
            self.current = text;
        }
    }

    fn finalize(&mut self, elapsed: Duration, kind: &'static str) {
        if !self.current.trim().is_empty() {
            if self.first_final.is_none() {
                self.first_final = Some(milliseconds(elapsed));
                self.final_kind = Some(kind);
            }
            self.committed.push(std::mem::take(&mut self.current));
        }
    }

    fn drain(
        &mut self,
        recognizer: &OnlineRecognizer,
        stream: &OnlineStream,
        started: Instant,
        trace: bool,
        endpoint: bool,
    ) -> Result<(), Box<dyn Error>> {
        while recognizer.is_ready(stream) {
            let decode_started = Instant::now();
            recognizer.decode(stream);
            self.decode_time += decode_started.elapsed();
            self.decode_calls += 1;
            let result = recognizer
                .get_result(stream)
                .ok_or("cannot read recognizer result")?;
            self.observe(result.text, started.elapsed(), trace);
            if endpoint && recognizer.is_endpoint(stream) {
                self.finalize(started.elapsed(), "endpoint");
                self.endpoints += 1;
                recognizer.reset(stream);
            }
        }
        // Also observe after every input chunk, even when no decode is ready.
        let result = recognizer
            .get_result(stream)
            .ok_or("cannot read recognizer result")?;
        self.observe(result.text, started.elapsed(), trace);
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    if !(args.len() == 4 || args.len() == 5) || !matches!(args[1].as_str(), "paced" | "offline") {
        return Err(
            "usage: lcrt-sherpa-spike <paced|offline> <model-dir> <jfk.wav> [repeats:1..60]".into(),
        );
    }
    let paced = args[1] == "paced";
    let repeats: usize = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(1);
    if !(1..=60).contains(&repeats) {
        return Err("repeats must be 1..60".into());
    }
    let mut reader = hound::WavReader::open(&args[3])?;
    let spec = reader.spec();
    if spec.channels != 1
        || spec.sample_rate != RATE as u32
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
        || reader.duration() != 176_000
    {
        return Err(
            "expected 11-second mono 16 kHz signed 16-bit JFK WAV; verify SHA externally".into(),
        );
    }
    let samples = reader
        .samples::<i16>()
        .map(|s| s.map(|s| f32::from(s) / 32768.0))
        .collect::<Result<Vec<_>, _>>()?;
    let model = Path::new(&args[2]);
    let model_file = |name: &str| -> Result<String, Box<dyn Error>> {
        let path = model.join(name);
        if !path.is_file() {
            return Err(format!("missing model file: {}", path.display()).into());
        }
        let text = path.to_str().ok_or("model path must be UTF-8")?;
        if text.contains('\0') {
            return Err("model path contains NUL".into());
        }
        Ok(text.to_owned())
    };
    let mut config = OnlineRecognizerConfig {
        enable_endpoint: true,
        rule1_min_trailing_silence: 2.4,
        rule2_min_trailing_silence: 1.2,
        rule3_min_utterance_length: 20.0,
        decoding_method: Some("greedy_search".into()),
        ..Default::default()
    };
    config.model_config.transducer.encoder = Some(model_file("encoder-epoch-99-avg-1.int8.onnx")?);
    config.model_config.transducer.decoder = Some(model_file("decoder-epoch-99-avg-1.onnx")?);
    config.model_config.transducer.joiner = Some(model_file("joiner-epoch-99-avg-1.int8.onnx")?);
    config.model_config.tokens = Some(model_file("tokens.txt")?);
    config.model_config.num_threads = 4;
    config.model_config.provider = Some("cpu".into());
    let startup = Instant::now();
    let recognizer = OnlineRecognizer::create(&config).ok_or("recognizer construction failed")?;
    // Stream is dropped before the recognizer; one driver owns all calls.
    let stream = recognizer.create_stream();
    let startup_ms = milliseconds(startup.elapsed());
    let mut observations = Observations::default();
    let mut delivered = 0;
    let started = Instant::now();
    for _ in 0..repeats {
        for chunk in samples.chunks(CHUNK) {
            delivered += chunk.len();
            if paced
                && let Some(wait) =
                    (started + deadline(delivered)).checked_duration_since(Instant::now())
            {
                thread::sleep(wait);
            }
            stream.accept_waveform(RATE as i32, chunk);
            observations.drain(&recognizer, &stream, started, repeats == 1, true)?;
        }
    }
    // Official Rust example flush: 0.3 s synthetic tail, accepted without pacing.
    // This is end-of-input context, not additional fixture audio or latency input.
    stream.accept_waveform(RATE as i32, &[0.0; 4800]);
    stream.input_finished();
    observations.drain(&recognizer, &stream, started, repeats == 1, false)?;
    observations.finalize(started.elapsed(), "end_of_input");
    let completion = started.elapsed();
    println!(
        "{}",
        json!({
            "mode": args[1], "repeats": repeats, "audio_duration_ms": milliseconds(deadline(delivered)),
            "model_startup_ms": startup_ms, "first_partial_ms": observations.first_partial,
            "first_partial_text": observations.first_text,
            "first_final_ms": observations.first_final, "first_final_kind": observations.final_kind,
            "completion_ms": milliseconds(completion), "decode_calls": observations.decode_calls,
            "decode_wall_ms": milliseconds(observations.decode_time), "endpoints": observations.endpoints,
            "real_time_factor": (!paced).then(|| completion.as_secs_f64() / deadline(delivered).as_secs_f64()),
            "transcript": observations.committed.join(" "), "hypothesis_changes": observations.changes,
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cumulative_pacing_is_twenty_ms_and_includes_last_chunk() {
        assert_eq!(deadline(CHUNK), Duration::from_millis(20));
        assert_eq!(deadline(176_000), Duration::from_secs(11));
    }
    #[test]
    fn first_nonempty_and_endpoint_segments_survive_resets() {
        let mut o = Observations::default();
        o.observe(" ".into(), Duration::ZERO, true);
        assert!(o.first_partial.is_none());
        o.observe("AND".into(), Duration::from_millis(100), true);
        o.observe("AND SO".into(), Duration::from_millis(200), true);
        o.finalize(Duration::from_millis(300), "endpoint");
        o.observe(String::new(), Duration::from_millis(310), true);
        o.observe("MY FELLOW".into(), Duration::from_millis(400), true);
        o.finalize(Duration::from_millis(500), "end_of_input");
        assert_eq!(o.first_partial, Some(100.0));
        assert_eq!(o.first_final, Some(300.0));
        assert_eq!(o.final_kind, Some("endpoint"));
        assert_eq!(o.committed.join(" "), "AND SO MY FELLOW");
    }
}
