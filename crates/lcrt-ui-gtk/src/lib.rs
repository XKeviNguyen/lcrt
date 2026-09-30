//! Native GTK4/libadwaita presentation and its bounded core-to-UI bridge.

mod bridge;
mod captions;
mod preferences;
mod presentation;
mod window;

pub use bridge::{
    CaptionUiAction, CredentialTone, CredentialView, EnteredApiKey, GtkCaptionReceiver,
    GtkCaptionSink, VocabularyCard, VocabularyOutcome, VocabularyProblem,
};
pub use window::{CaptionUiMode, CaptionUiOptions, run_caption_ui};
