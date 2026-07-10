//! LowBand neural runtime (`lowband-nn`).
//!
//! | Feature | Description |
//! |---------|-------------|
//! | 84 | Model versioning: each ONNX model carries an [`EvalCard`] in the `models/` directory |

pub mod eval_card;

pub use eval_card::{eval_card, EvalCard, ModelId, MODEL_REGISTRY};
