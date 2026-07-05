//! LowBand Transport Protocol (`lbtp`) core.
//!
//! Implements the LBTP transport features:
//!
//! | # | Feature |
//! |---|---------|
//! | 16 | adaptive Reed-Solomon FEC via Gilbert-Elliott burst model |
//! | 17 | channel_priority pacer — input beats media in every queue |
//!
//! # Channel map
//!
//! | Channel | Purpose | Delivery class |
//! |---------|---------|----------------|
//! | 0 | ctrl / ACK | reliable-ordered |
//! | 1 | audio | realtime (no retx) |
//! | 2 | cursor | reliable-ordered |
//! | 3 | input events | reliable-ordered |
//! | 4 | screen-rt | realtime (no retx) |
//! | 5 | video-rt | realtime (no retx) |
//! | 6 | reliable bulk (screen lossless, video ref) | reliable-unordered |
//! | 7 | xfer / file transfer | reliable-unordered |
//! | 8 | probes (padding) | realtime, first-to-drop |
//!
//! # Pacer priority invariant (Feature 17)
//!
//! The canonical priority order is `0 > 3 > 2 > 1 > 4 > 5 > 6 > 7 > 8`.
//! Input frames (channel 3) always beat media frames (channels 1, 4, 5) at
//! every dequeue decision, regardless of arrival order or frame size.

pub mod fec;
pub mod pacer;

pub use fec::{
    GilbertElliottEstimator, GilbertElliottParams, MAX_FEC_RATIO, MIN_FEC_RATIO,
    MIN_OBS_FOR_ESTIMATE,
};
pub use pacer::{ChannelId, Pacer, PacerFrame, PRIORITY_ORDER};
