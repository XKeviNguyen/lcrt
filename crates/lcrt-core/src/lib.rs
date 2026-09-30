//! Portable application core for LCRT.
//!
//! This crate owns platform-independent audio and transcription ports, caption
//! state, runtime configuration, and pipeline orchestration. Platform adapters
//! (PipeWire, whisper.cpp, and native UI implementations) live outside it.

pub mod audio;
pub mod caption;
pub mod config;
pub mod convert;
pub mod pipeline;
pub mod preferences;
pub mod session;
pub mod transcription;
pub mod ui;

pub use audio::{
    AudioCapture, AudioCaptureError, AudioChunk, AudioChunkError, AudioInputEvent,
    AudioSourceDescriptor, AudioSourceKind,
};
pub use caption::{Caption, CaptionSnapshot, CaptionState, CaptionStateError, CaptionStatus};
pub use config::{RuntimeConfig, RuntimeConfigError};
pub use convert::{AudioConversionError, AudioConverter};
pub use pipeline::{CaptionPipeline, PipelineError, RunSummary};
pub use preferences::{
    AppearancePreferences, GeneralPreferences, Preferences, Rgb, VocabularyPreferences,
};
pub use session::{Language, LanguageSelection, ProcessingMode, SessionGeneration, SessionOptions};
pub use transcription::{Transcriber, TranscriptUpdate, TranscriptUpdateError, TranscriptionError};
pub use ui::{CaptionSink, CaptionSinkError};
