//! LowBand messaging plugin (`lowband-messaging`).
//!
//! | # | Feature |
//! |---|---------|
//! | 112 | System syncs clipboard text only with capability_token held live |
//! | 115 | System frames clipboard and chat payloads with reliable_channel zstd on the wire |
//! | 116 | System rejects remote clipboard content without an active clipboard_grant |

pub mod clipboard;

pub use clipboard::{ClipboardError, ClipboardGrant, ClipboardSession};
