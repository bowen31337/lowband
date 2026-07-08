//! LowBand Transport Protocol (`lbtp`) core.
//!
//! Implements the LBTP transport features:
//!
//! | # | Feature |
//! |---|---------|
//! | 10 | token_bucket pacing — every outbound packet passes through the pacer; no encoder burst floods the uplink queue |
//! | 11 | wire_seq expansion — 16-bit wire field → full 47-bit sequence at the receiver |
//! | 12 | path_challenge / path_response — migrates the session to a new path without renegotiation |
//! | 13 | delay-gradient trendline estimator — OWD variation slope drives congestion control |
//! | 14 | cellular_mode guard — widens γ, caps decrease frequency, and gates increases on OWD trend when bimodal spikes appear |
//! | 16 | adaptive Reed-Solomon FEC via Gilbert-Elliott burst model |
//! | 17 | channel_priority pacer — input beats media in every queue |
//! | 18 | per-tick frame coalescing — concurrent frames from all channels into one aggregated_datagram |
//! | 20 | loss backstop — multiplicative send_rate reduction when sustained loss > 10 % |
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

pub mod cellular;
pub mod congestion;
pub mod connection;
pub mod delay;
pub mod fec;
pub mod pacer;
pub mod path;
pub mod seq;

pub use delay::{
    BandwidthUsage, DelayGradientEstimator, MIN_WINDOW_FOR_SLOPE,
    OVERUSE_THRESHOLD_GAMMA_US_PER_MS, TRENDLINE_SMOOTHING_ALPHA, TRENDLINE_WINDOW_SIZE,
};
pub use cellular::{
    BimodalDetector, CellularModeController, CELLULAR_ENTRY_TICKS, CELLULAR_EXIT_TICKS,
    CELLULAR_GAMMA_MULTIPLIER, CELLULAR_MIN_DECREASE_TICKS, MIN_BIMODAL_FRACTION,
    MAX_BIMODAL_FRACTION, MIN_BIMODAL_SPREAD_US, OWD_WINDOW_SIZE, SPIKE_THRESHOLD_FACTOR,
};
pub use congestion::{
    LossBackstop, BACKSTOP_COOLDOWN_TICKS, BACKSTOP_MIN_RATE_BPS, LOSS_BACKSTOP_REDUCTION,
    LOSS_BACKSTOP_THRESHOLD,
};
pub use fec::{
    BurstStatsFitter, GilbertElliottEstimator, GilbertElliottParams, MAX_FEC_RATIO,
    MIN_FEC_RATIO, MIN_OBS_FOR_ESTIMATE, MIN_RUNS_FOR_ESTIMATE,
};
pub use connection::Connection;
pub use pacer::{
    ChannelId, Pacer, PacerAggregatedDatagram, PacerFrame, PRIORITY_ORDER,
    MAX_DATAGRAM_PAYLOAD_BYTES,
};
pub use path::{
    ChallengeToken, MigrationEvent, PathChallengeFrame, PathMigrationController,
    PathResponseFrame, CHALLENGE_TIMEOUT_TICKS, MAX_CHALLENGE_RETRIES,
};
pub use seq::{SeqExpander, SEQ_BITS, SEQ_MAX, WIRE_SEQ_BITS};
