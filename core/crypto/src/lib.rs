//! LowBand cryptography layer.
//!
//! Implements the Security & Consent encryption features:
//!
//! | # | Feature |
//! |---|---------|
//! | 25 | relay_guard — type-level invariant that only ciphertext flows through the TURN relay |

pub mod relay_guard;

pub use relay_guard::{
    DatagramCipher, E2eeRelayBridge, RelayPayload, RELAY_GUARD_OVERHEAD_BYTES,
};
