//! AI-media labeling (FR-8 / NFR-6, v1.1).
//!
//! The PRD requires neural-reconstructed media to be **always UI-labeled**
//! ("AI-reconstructed") and never silently synthetic (NFR-6 trust model). This
//! makes the label a property carried with any neural-gear output, so a frame
//! produced by the neural voice codec or the AI head-video gear cannot reach a
//! shell without its provenance attached — the shell renders the badge from
//! [`MediaProvenance`], and it cannot be suppressed for AI media.
//!
//! The neural gears use this under `--features onnx` and shells render it; a
//! build without an active AI-media path leaves parts of the contract unused.
#![allow(dead_code)]

/// How a media frame was produced — travels with the frame to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaProvenance {
    /// Captured/coded from the real source (camera, mic, screen).
    Real,
    /// Reconstructed by a neural gear (vocoder / talking-head). MUST be labeled.
    AiReconstructed,
}

/// The exact UI label text for AI-reconstructed media (Design §5 guardrail).
pub const AI_LABEL: &str = "AI-reconstructed";

impl MediaProvenance {
    /// `true` when this media must carry the AI label.
    pub fn requires_ai_label(self) -> bool {
        matches!(self, MediaProvenance::AiReconstructed)
    }

    /// The label a shell must display for this media, if any.
    pub fn label(self) -> Option<&'static str> {
        match self {
            MediaProvenance::AiReconstructed => Some(AI_LABEL),
            MediaProvenance::Real => None,
        }
    }
}

/// A media frame tagged with its provenance so the label can never be dropped
/// in transit from the neural gear to the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct LabeledFrame<T> {
    pub provenance: MediaProvenance,
    pub frame: T,
}

impl<T> LabeledFrame<T> {
    /// Tag a neural-gear output; the AI label is mandatory and attached here.
    pub fn ai(frame: T) -> Self {
        Self { provenance: MediaProvenance::AiReconstructed, frame }
    }

    /// Tag real captured/coded media.
    pub fn real(frame: T) -> Self {
        Self { provenance: MediaProvenance::Real, frame }
    }

    pub fn label(&self) -> Option<&'static str> {
        self.provenance.label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_media_is_always_labeled() {
        let f = LabeledFrame::ai(vec![0u8; 4]);
        assert!(f.provenance.requires_ai_label());
        assert_eq!(f.label(), Some(AI_LABEL));
    }

    #[test]
    fn real_media_carries_no_ai_label() {
        let f = LabeledFrame::real(vec![0u8; 4]);
        assert!(!f.provenance.requires_ai_label());
        assert_eq!(f.label(), None);
    }
}
