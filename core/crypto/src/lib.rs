//! LowBand cryptography layer.
//!
//! Implements the Security & Consent encryption features:
//!
//! | # | Feature |
//! |---|---------|
//! | 19 | noise_ik — Noise-IK handshake keyed with `session_code` |
//! | 20 | key_exchange — X25519 ephemeral DH + HKDF-SHA-256 traffic-key derivation |
//! | 23 | known_peers — persist each peer's static public key to the known_peers store |
//! | 24 | short_auth_string — verbal channel verification phrase |
//! | 25 | relay_guard — type-level invariant that only ciphertext flows through the TURN relay |

pub mod key_exchange;
pub mod known_peers;
pub mod noise_ik;
pub mod relay_guard;
pub mod short_auth_string;
pub mod udp_session;

pub use key_exchange::{EphemeralKeypair, SessionState, TrafficKeys};
pub use known_peers::{KnownPeer, KnownPeerStore, PeerId};
pub use noise_ik::{
    HandshakeError, HandshakeResult, NoiseIkInitiator, NoiseIkResponder, StaticKeypair,
    MSG1_LEN, MSG2_LEN,
};
pub use relay_guard::{
    DatagramCipher, E2eeRelayBridge, RelayPayload, RELAY_GUARD_OVERHEAD_BYTES,
};
pub use short_auth_string::ShortAuthString;
pub use udp_session::{SecureReceiver, SecureSender, SecureSession, SessionError};
