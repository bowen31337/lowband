//! LowBand neural runtime (`lowband-nn`).
//!
//! | Feature | Description |
//! |---------|-------------|
//! | 79 | Gear availability: which neural gears exist is decided by [`capability_probe`] results at startup |
//! | 83 | Neural vocoder gate: the vocoder activates only when [`capability_probe::probe`] reports a hardware accelerator |
//! | 84 | Model versioning: each ONNX model carries an [`EvalCard`] in the `models/` directory |

pub mod capability_probe;
pub mod eval_card;

pub use capability_probe::{CapabilityProbeResult, ExecutionProvider};
pub use eval_card::{eval_card, EvalCard, ModelId, MODEL_REGISTRY};
