//! LowBand messaging plugin (`lowband-messaging`).
//!
//! | # | Feature |
//! |---|---------|
//! | 112 | System syncs clipboard text only with capability_token held live |
//! | 113 | System syncs clipboard text with round_trip under one second at the constrained tier |
//! | 114 | User can send an in-session chat_message delivered even at survival tier |
//! | 115 | System frames clipboard and chat payloads with reliable_channel zstd on the wire |
//! | 116 | System rejects remote clipboard content without an active clipboard_grant |
//! | 143 | System creates separate view, control, file, and clipboard capability_token grants on explicit consent |
//! | 153 | System validates every injected input event with capability_token checks before delivery |
//! | 171 | System persists signed entries to the audit_log covering identity keys, grants, and timestamps |
//! | 32  | System persists session metadata to the session_records store for later export |

pub mod audit;
pub mod channel;
pub mod chat;
pub mod clipboard;
pub mod grants;
pub mod panic_key;
pub mod qos_observer;
pub mod session_records;

pub use audit::{AuditEntry, AuditLog};
pub use channel::{ChannelError, Message, ReliableChannel};
pub use chat::{ChatError, ChatMessage, ChatSession, CHAT_MAX_TEXT_BYTES, SURVIVAL_TIER_BPS};
pub use clipboard::{
    ClipboardError, ClipboardGrant, ClipboardSession,
    CLIPBOARD_MAX_TEXT_BYTES, CONSTRAINED_TIER_BPS,
};
pub use grants::{
    CapabilityError,
    ConsentGrant,
    ConsentRevocationHandle,
    ControlGrant, ControlSession,
    FileGrant, FileSession,
    ViewGrant, ViewSession,
};
pub use panic_key::{PanicController, PanicEffect, PANIC_INJECTION_BLOCK_DEADLINE_MS};
pub use qos_observer::QosSessionObserver;
pub use session_records::{SessionRecord, SessionRecordStore, Tier};
