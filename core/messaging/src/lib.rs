//! LowBand messaging plugin (`lowband-messaging`).
//!
//! | # | Feature |
//! |---|---------|
//! | 112 | System syncs clipboard text only with capability_token held live |
//! | 115 | System frames clipboard and chat payloads with reliable_channel zstd on the wire |
//! | 116 | System rejects remote clipboard content without an active clipboard_grant |
//! | 143 | System creates separate view, control, file, and clipboard capability_token grants on explicit consent |
//! | 153 | System validates every injected input event with capability_token checks before delivery |
//! | 171 | System persists signed entries to the audit_log covering identity keys, grants, and timestamps |

pub mod audit;
pub mod clipboard;
pub mod grants;

pub use audit::{AuditEntry, AuditLog};
pub use clipboard::{ClipboardError, ClipboardGrant, ClipboardSession};
pub use grants::{
    CapabilityError,
    ControlGrant, ControlSession,
    FileGrant, FileSession,
    ViewGrant, ViewSession,
};
