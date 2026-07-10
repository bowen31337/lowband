//! LowBand neural runtime (`lowband-nn`).
//!
//! | Feature | Description |
//! |---------|-------------|
//! | 79 | Gear availability: which neural gears exist is decided by [`capability_probe`] results at startup |
//! | 81 | Transport guard: [`model_watchdog`] ensures a stalled model never stalls the transport loop |
//! | 82 | Head-gear gate: Gear A is rejected unless an NPU or spare CPU is available ([`head_gear_gate`]) |
//! | 83 | Neural vocoder gate: the vocoder activates only when [`capability_probe::probe`] reports a hardware accelerator |
//! | 84 | Model versioning: each ONNX model carries an [`EvalCard`] in the `models/` directory |
//! | 85 | Warm pool: [`warm_pool`] keeps models pre-loaded so a gear switch adds no cold-start latency |

pub mod capability_probe;
pub mod eval_card;
pub mod head_gear_gate;
pub mod model_watchdog;
pub mod warm_pool;

pub use capability_probe::{CapabilityProbeResult, ExecutionProvider};
pub use eval_card::{eval_card, EvalCard, ModelId, MODEL_REGISTRY};
pub use head_gear_gate::{head_gear_available, HeadGearCapability, CPU_HEADROOM_THRESHOLD_PCT};
pub use model_watchdog::{InferenceTimeout, ModelWatchdog, INFERENCE_DEADLINE_MS};
pub use warm_pool::{WarmEntry, WarmPool, WarmState, GEAR_A_MODELS};
