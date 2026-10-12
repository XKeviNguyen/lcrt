//! Local, incremental Japanese/Vietnamese speech behind the portable contract.
use std::{
    fs,
    io::{BufReader, Read},
    path::Path,
};

use lcrt_core::{AudioChunk, AudioConverter, Transcriber, TranscriptUpdate, TranscriptionError};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineStream};

const RATE: u32 = 16_000;
const FEED_SAMPLES: usize = 320;
const MAX_TEXT_BYTES: usize = 16_384;
const SEED: &str = "initial_context_token";
const SEED_ID: &str = "initial_context_token_id";
const SEED_ERROR: &str = "initial_context_error";
const PACK_ERROR: &str = "The streaming speech pack is missing or damaged. Reinstall LILOPOP, or choose a custom Whisper model in Advanced.";

/// Only the two languages enabled by this pilot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamingLanguage {
    /// Japanese speech.
    Japanese,
    /// Vietnamese speech.
    Vietnamese,
}

impl StreamingLanguage {
    fn tag(self) -> &'static str {
        match self {
            Self::Japanese => "<JA>",
            Self::Vietnamese => "<VI>",
        }
    }
}

#[derive(Deserialize)]
struct Manifest {
    models: Vec<Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    id: String,
    file: String,
    size: u64,
    sha256: String,
}

fn verify(path: &Path, size: u64, expected: &str) -> Result<(), TranscriptionError> {
    let failed = || TranscriptionError::new(PACK_ERROR);
    let file = fs::File::open(path).map_err(|_| failed())?;
    if file.metadata().map_err(|_| failed())?.len() != size {
        return Err(failed());
    }
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0; 65_536];
    loop {
        let count = reader.read(&mut buffer).map_err(|_| failed())?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    if format!("{:x}", digest.finalize()) != expected {
        return Err(failed());
    }
    Ok(())
}

fn vocabulary_id(tokens: &str, tag: &str) -> Result<i32, TranscriptionError> {
    let mut found = None;
    for line in tokens.lines() {
        if let Some((symbol, id)) = line.rsplit_once(' ')
            && symbol == tag
        {
            if found.is_some() {
                return Err(TranscriptionError::new(PACK_ERROR));
            }
            found = Some(
                id.parse::<i32>()
                    .map_err(|_| TranscriptionError::new(PACK_ERROR))?,
            );
        }
    }
    found
        .filter(|id| *id > 0)
        .ok_or_else(|| TranscriptionError::new(PACK_ERROR))
}

/// Current utterance only. Partials replace the hypothesis; endpoints commit it.
#[derive(Default)]
struct Utterance {
    current: String,
}

impl Utterance {
    fn partial(&mut self, text: &str) -> Result<Option<TranscriptUpdate>, TranscriptionError> {
        if text.len() > MAX_TEXT_BYTES {
            return Err(TranscriptionError::new(
                "The streaming speech result exceeded its limit. Restart captions.",
            ));
        }
        let text = text.trim();
        if text.is_empty() || text == self.current {
            return Ok(None);
        }
        self.current = text.to_owned();
        TranscriptUpdate::partial(text)
            .map(Some)
            .map_err(|error| TranscriptionError::new(error.to_string()))
    }

    fn finalize(&mut self) -> Option<TranscriptUpdate> {
        let text = std::mem::take(&mut self.current);
        if text.is_empty() {
            None
        } else {
            TranscriptUpdate::finalized(text).ok()
        }
    }
}

/// One owner feeds, decodes, reads and resets a persistent native stream.
///
/// Inference runs on the existing pipeline thread. PipeWire retains its bounded
/// non-blocking capture queue; this adapter adds no pending audio queue.
pub struct StreamingTranscriber {
    // Declaration order drops the stream before its recognizer.
    stream: OnlineStream,
    recognizer: OnlineRecognizer,
    converter: Option<AudioConverter>,
    utterance: Utterance,
    finished: bool,
}

impl StreamingTranscriber {
    /// Loads the installed, hash-verified speech pack and checks native seeding.
    pub fn new(root: &Path, language: StreamingLanguage) -> Result<Self, TranscriptionError> {
        let manifest: Manifest =
            serde_json::from_str(include_str!("../../../packaging/models.json"))
                .map_err(|_| TranscriptionError::new(PACK_ERROR))?;
        let file = |part: &str| -> Result<String, TranscriptionError> {
            let artifact = manifest
                .models
                .iter()
                .find(|m| m.id == format!("streaming-ja-vi-{part}"))
                .ok_or_else(|| TranscriptionError::new(PACK_ERROR))?;
            let path = root.join(&artifact.file);
            verify(&path, artifact.size, &artifact.sha256)?;
            path.to_str()
                .filter(|path| !path.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(|| TranscriptionError::new(PACK_ERROR))
        };
        let mut config = OnlineRecognizerConfig {
            enable_endpoint: true,
            rule1_min_trailing_silence: 2.4,
            rule2_min_trailing_silence: 1.2,
            rule3_min_utterance_length: 20.0,
            decoding_method: Some("greedy_search".into()),
            ..Default::default()
        };
        config.model_config.transducer.encoder = Some(file("encoder")?);
        config.model_config.transducer.decoder = Some(file("decoder")?);
        config.model_config.transducer.joiner = Some(file("joiner")?);
        let tokens = file("tokens")?;
        let id = vocabulary_id(
            &fs::read_to_string(&tokens).map_err(|_| TranscriptionError::new(PACK_ERROR))?,
            language.tag(),
        )?;
        config.model_config.tokens = Some(tokens);
        config.model_config.num_threads = 1;
        config.model_config.provider = Some("cpu".into());
        let recognizer =
            OnlineRecognizer::create(&config).ok_or_else(|| TranscriptionError::new(PACK_ERROR))?;
        let stream = recognizer.create_stream();
        stream.set_option(SEED, language.tag());
        recognizer.is_ready(&stream);
        if !stream.get_option(SEED_ERROR).is_empty() || stream.get_option(SEED_ID) != id.to_string()
        {
            return Err(TranscriptionError::new(
                "The streaming speech engine could not initialize its language. Reinstall LILOPOP.",
            ));
        }
        Ok(Self {
            stream,
            recognizer,
            converter: None,
            utterance: Utterance::default(),
            finished: false,
        })
    }

    fn native_error(&self) -> Result<(), TranscriptionError> {
        if self.stream.get_option(SEED_ERROR).is_empty() {
            Ok(())
        } else {
            Err(TranscriptionError::new(
                "The streaming speech engine lost its language configuration. Restart captions.",
            ))
        }
    }

    fn drain(
        &mut self,
        endpoint: bool,
        updates: &mut Vec<TranscriptUpdate>,
    ) -> Result<(), TranscriptionError> {
        for _ in 0..256 {
            let ready = self.recognizer.is_ready(&self.stream);
            self.native_error()?;
            if !ready {
                return Ok(());
            }
            self.recognizer.decode(&self.stream);
            self.native_error()?;
            let result = self.recognizer.get_result(&self.stream).ok_or_else(|| {
                TranscriptionError::new(
                    "The streaming speech engine could not read a result. Restart captions.",
                )
            })?;
            if result.tokens.iter().any(|token| {
                matches!(
                    token.as_str(),
                    "<JA>" | "<VI>" | "<EN>" | "<ZH>" | "<AR>" | "<RU>" | "<TH>" | "<ID>"
                )
            }) {
                return Err(TranscriptionError::new(
                    "The streaming speech engine returned an invalid caption. Restart captions.",
                ));
            }
            if let Some(update) = self.utterance.partial(&result.text)? {
                updates.push(update);
            }
            if endpoint && self.recognizer.is_endpoint(&self.stream) {
                updates.extend(self.utterance.finalize());
                self.recognizer.reset(&self.stream);
                self.native_error()?;
            }
        }
        Err(TranscriptionError::new(
            "The streaming speech engine exceeded its decoding limit. Restart captions.",
        ))
    }

    fn feed(
        &mut self,
        samples: &[f32],
        updates: &mut Vec<TranscriptUpdate>,
    ) -> Result<(), TranscriptionError> {
        for chunk in samples.chunks(FEED_SAMPLES) {
            self.stream.accept_waveform(RATE as i32, chunk);
            self.drain(true, updates)?;
        }
        Ok(())
    }
}

impl Transcriber for StreamingTranscriber {
    fn push_audio(
        &mut self,
        chunk: AudioChunk,
    ) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        if self.finished {
            return Err(TranscriptionError::new(
                "The streaming speech session has stopped. Start captions again.",
            ));
        }
        if self.converter.is_none() {
            self.converter = Some(
                AudioConverter::new(&chunk, RATE)
                    .map_err(|error| TranscriptionError::new(error.to_string()))?,
            );
        }
        let samples = self
            .converter
            .as_mut()
            .expect("converter initialized")
            .push(&chunk)
            .map_err(|error| TranscriptionError::new(error.to_string()))?;
        let mut updates = Vec::new();
        self.feed(&samples, &mut updates)?;
        Ok(updates)
    }

    fn finish(&mut self) -> Result<Vec<TranscriptUpdate>, TranscriptionError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut updates = Vec::new();
        if let Some(mut converter) = self.converter.take() {
            let tail = converter
                .finish()
                .map_err(|error| TranscriptionError::new(error.to_string()))?;
            self.feed(&tail, &mut updates)?;
        }
        self.feed(&vec![0.0; RATE as usize / 2], &mut updates)?;
        self.stream.input_finished();
        self.drain(false, &mut updates)?;
        updates.extend(self.utterance.finalize());
        Ok(updates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lcrt_core::CaptionStatus;

    #[test]
    #[ignore = "requires the pinned speech pack and rebuilt native runtime"]
    fn real_native_stop_is_idempotent_and_restart_has_fresh_language_state() {
        let root = std::env::var_os("LCRT_TEST_STREAMING_MODEL_DIR")
            .expect("set LCRT_TEST_STREAMING_MODEL_DIR for the native gate");
        for language in [StreamingLanguage::Japanese, StreamingLanguage::Vietnamese] {
            let mut speech = StreamingTranscriber::new(Path::new(&root), language).unwrap();
            let chunk = || AudioChunk::new(vec![0.0; 3_200], RATE, 1).unwrap();
            speech.push_audio(chunk()).unwrap();
            speech.finish().unwrap();
            assert!(speech.finish().unwrap().is_empty());
            assert!(
                speech
                    .push_audio(chunk())
                    .unwrap_err()
                    .to_string()
                    .contains("session has stopped")
            );
            drop(speech);
            let mut restarted = StreamingTranscriber::new(Path::new(&root), language).unwrap();
            assert_eq!(restarted.stream.get_option(SEED), language.tag());
            restarted.push_audio(chunk()).unwrap();
            restarted.finish().unwrap();
        }
    }

    #[test]
    fn unicode_partials_replace_without_duplication_then_finalize_once() {
        let mut utterance = Utterance::default();
        assert_eq!(
            utterance.partial("今日は").unwrap().unwrap().text(),
            "今日は"
        );
        assert!(utterance.partial("今日は").unwrap().is_none());
        assert_eq!(
            utterance.partial("今日は新しい").unwrap().unwrap().text(),
            "今日は新しい"
        );
        assert_eq!(
            utterance
                .partial("今日は新しいプロジェクト")
                .unwrap()
                .unwrap()
                .text(),
            "今日は新しいプロジェクト"
        );
        let final_update = utterance.finalize().unwrap();
        assert_eq!(final_update.text(), "今日は新しいプロジェクト");
        assert_eq!(final_update.status(), CaptionStatus::Final);
        assert!(utterance.finalize().is_none());
        assert_eq!(
            utterance.partial("một dự án mới").unwrap().unwrap().text(),
            "một dự án mới"
        );
    }

    #[test]
    fn revised_partial_and_endpoint_do_not_append_previous_hypotheses() {
        let mut utterance = Utterance::default();
        utterance.partial("một dự án cũ").unwrap();
        assert_eq!(
            utterance.partial("một dự án mới").unwrap().unwrap().text(),
            "một dự án mới"
        );
        assert_eq!(utterance.finalize().unwrap().text(), "một dự án mới");
        assert!(utterance.partial("").unwrap().is_none());
        assert!(utterance.partial(&"a".repeat(MAX_TEXT_BYTES + 1)).is_err());
    }

    #[test]
    fn language_ids_come_from_vocabulary_and_invalid_tags_fail() {
        assert_eq!(
            vocabulary_id("<blk> 0\n<JA> 37\n<VI> 23\n", "<JA>").unwrap(),
            37
        );
        assert_eq!(vocabulary_id("<VI> 23\n", "<VI>").unwrap(), 23);
        for tokens in ["<EN> 1\n", "<JA> 0\n", "<JA> nope\n", "<JA> 2\n<JA> 3\n"] {
            assert!(vocabulary_id(tokens, "<JA>").is_err());
        }
    }

    #[test]
    fn missing_and_same_size_corrupt_model_fail_actionably() {
        let directory =
            std::env::temp_dir().join(format!("lcrt-speech-integrity-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("model");
        let expected = format!("{:x}", Sha256::digest(b"good"));
        assert!(
            verify(&path, 4, &expected)
                .unwrap_err()
                .to_string()
                .contains("Reinstall LILOPOP")
        );
        fs::write(&path, b"good").unwrap();
        verify(&path, 4, &expected).unwrap();
        fs::write(&path, b"evil").unwrap();
        assert!(verify(&path, 4, &expected).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
