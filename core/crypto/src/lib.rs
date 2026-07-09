//! LowBand cryptography layer.
//!
//! Implements the Security & Consent encryption features:
//!
//! | # | Feature |
//! |---|---------|
//! | 23 | known_peers — persist each peer's static public key to the known_peers store |
//! | 24 | short_auth_string — verbal channel verification phrase |
//! | 25 | relay_guard — type-level invariant that only ciphertext flows through the TURN relay |

pub mod known_peers;
pub mod relay_guard;
pub mod short_auth_string;

pub use known_peers::{KnownPeer, KnownPeerStore, PeerId};
pub use relay_guard::{
    DatagramCipher, E2eeRelayBridge, RelayPayload, RELAY_GUARD_OVERHEAD_BYTES,
};
pub use short_auth_string::ShortAuthString;
