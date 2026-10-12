//! Short compatibility and paced speech checks through the unchanged Rust/C API.
use serde_json::json;
use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineStream};
use std::{
    error::Error,
    path::Path,
    thread,
    time::{Duration, Instant},
};

const RATE: i32 = 16_000;
const TOKEN: &str = "initial_context_token";
const TOKEN_ID: &str = "initial_context_token_id";
const TOKEN_ERROR: &str = "initial_context_error";

fn check_error(stream: &OnlineStream) -> Result<(), Box<dyn Error>> {
    let error = stream.get_option(TOKEN_ERROR);
    if error.is_empty() {
        Ok(())
    } else {
        Err(error.into())
    }
}

fn ready(recognizer: &OnlineRecognizer, stream: &OnlineStream) -> Result<bool, Box<dyn Error>> {
    let ready = recognizer.is_ready(stream);
    check_error(stream)?;
    Ok(ready)
}

fn configure(
    recognizer: &OnlineRecognizer,
    stream: &OnlineStream,
    token: &str,
    id: i32,
) -> Result<(), Box<dyn Error>> {
    if !token.is_empty() {
        stream.set_option(TOKEN, token);
    }
    ready(recognizer, stream)?;
    let actual = stream.get_option(TOKEN_ID);
    if actual != id.to_string() {
        return Err(format!(
            "native decoder did not acknowledge seed {id}: got {actual:?}; patched runtime required"
        )
        .into());
    }
    Ok(())
}

fn token_id(path: &Path, token: &str) -> Result<i32, Box<dyn Error>> {
    if token.is_empty() {
        return Ok(0);
    }
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .find_map(|line| {
            let (symbol, id) = line.rsplit_once(' ')?;
            (symbol == token).then(|| id.parse::<i32>())
        })
        .ok_or("language tag missing from vocabulary")?
        .map_err(Into::into)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 || !matches!(args[1].as_str(), "check" | "offline" | "paced") {
        return Err(
            "usage: lcrt-sherpa-ja-vi <check|offline|paced> <model-dir> <wav|-> <ja|vi|default>"
                .into(),
        );
    }
    let token = match args[4].as_str() {
        "ja" => "<JA>",
        "vi" => "<VI>",
        "default" => "",
        _ => return Err("expected ja, vi or default".into()),
    };
    let root = Path::new(&args[2]);
    let file = |name: &str| -> Result<String, Box<dyn Error>> {
        let path = root.join(name);
        if !path.is_file() {
            return Err(format!("missing speech model file: {}", path.display()).into());
        }
        let path = path.to_str().ok_or("model path must be UTF-8")?;
        if path.contains('\0') {
            return Err("model path contains NUL".into());
        }
        Ok(path.to_owned())
    };
    let mut config = OnlineRecognizerConfig {
        enable_endpoint: true,
        rule1_min_trailing_silence: 2.4,
        rule2_min_trailing_silence: 1.2,
        rule3_min_utterance_length: 20.0,
        decoding_method: Some("greedy_search".into()),
        ..Default::default()
    };
    config.model_config.transducer.encoder =
        Some(file("encoder-epoch-75-avg-11-chunk-16-left-128.int8.onnx")?);
    config.model_config.transducer.decoder =
        Some(file("decoder-epoch-75-avg-11-chunk-16-left-128.onnx")?);
    config.model_config.transducer.joiner =
        Some(file("joiner-epoch-75-avg-11-chunk-16-left-128.int8.onnx")?);
    config.model_config.tokens = Some(file("tokens.txt")?);
    config.model_config.num_threads = 1;
    config.model_config.provider = Some("cpu".into());
    let startup = Instant::now();
    let recognizer = OnlineRecognizer::create(&config).ok_or("streaming model cannot be loaded")?;
    let stream = recognizer.create_stream();
    let id = token_id(&root.join("tokens.txt"), token)?;
    configure(&recognizer, &stream, token, id)?;
    let startup_ms = startup.elapsed().as_secs_f64() * 1000.0;
    if args[1] == "check" {
        recognizer.reset(&stream);
        ready(&recognizer, &stream)?;
        assert_eq!(stream.get_option(TOKEN_ID), id.to_string());
        let invalid = recognizer.create_stream();
        invalid.set_option(TOKEN, "<MISSING>");
        assert!(
            ready(&recognizer, &invalid).is_err(),
            "missing seed was silently accepted"
        );
        let blank = recognizer.create_stream();
        blank.set_option(TOKEN, "<blk>");
        assert!(
            ready(&recognizer, &blank).is_err(),
            "blank is not a language seed"
        );
        stream.set_option(TOKEN, "<MISSING>");
        assert!(
            ready(&recognizer, &stream).is_err(),
            "seed changed after initialization"
        );
        let restarted = recognizer.create_stream();
        configure(&recognizer, &restarted, token, id)?;
        println!(
            "{}",
            json!({"compatibility_checks": "passed", "token": token, "resolved_id": id, "startup_ms": startup_ms})
        );
        return Ok(());
    }
    let mut wav = hound::WavReader::open(&args[3])?;
    let spec = wav.spec();
    if spec.channels != 1
        || spec.sample_rate != RATE as u32
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
    {
        return Err("expected mono 16 kHz signed 16-bit WAV".into());
    }
    let samples = wav
        .samples::<i16>()
        .map(|s| s.map(|s| f32::from(s) / 32768.0))
        .collect::<Result<Vec<_>, _>>()?;
    if samples.len() > RATE as usize * 120 {
        return Err("speech check limited to 120 seconds".into());
    }
    let started = Instant::now();
    let mut delivered = 0;
    let mut current = String::new();
    let mut committed = Vec::new();
    let mut changes = Vec::new();
    let mut endpoints = 0;
    let mut calls = 0;
    let mut first = None;
    let mut drain = |endpoint: bool| -> Result<(), Box<dyn Error>> {
        while ready(&recognizer, &stream)? {
            recognizer.decode(&stream);
            check_error(&stream)?;
            calls += 1;
            let result = recognizer
                .get_result(&stream)
                .ok_or("cannot read speech result")?;
            if result.tokens.iter().any(|t| t == "<JA>" || t == "<VI>") {
                return Err("language-control token emitted as caption".into());
            }
            if result.text != current {
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                if first.is_none() && !result.text.trim().is_empty() {
                    first = Some(ms);
                }
                changes.push(json!({"ms": ms, "text": result.text}));
                current = result.text;
            }
            if endpoint && recognizer.is_endpoint(&stream) {
                if !current.trim().is_empty() {
                    committed.push(std::mem::take(&mut current));
                }
                endpoints += 1;
                recognizer.reset(&stream);
                check_error(&stream)?;
            }
        }
        Ok(())
    };
    for chunk in samples.chunks(320) {
        delivered += chunk.len();
        if args[1] == "paced"
            && let Some(wait) = (started
                + Duration::from_secs_f64(delivered as f64 / f64::from(RATE)))
            .checked_duration_since(Instant::now())
        {
            thread::sleep(wait);
        }
        stream.accept_waveform(RATE, chunk);
        drain(true)?;
    }
    stream.accept_waveform(RATE, &vec![0.0; RATE as usize / 2]);
    stream.input_finished();
    drain(false)?;
    if !current.trim().is_empty() {
        committed.push(current);
    }
    println!(
        "{}",
        json!({"language": args[4], "token_id": id, "startup_ms": startup_ms, "audio_seconds": samples.len() as f64 / f64::from(RATE), "completion_ms": started.elapsed().as_secs_f64() * 1000.0, "first_nonempty_ms": first, "endpoints": endpoints, "decode_calls": calls, "final_segments": committed, "changes": changes})
    );
    Ok(())
}
