//! LowBand UI shell utilities (`lowband-shells`).
//!
//! | Feature | Description |
//! |---------|-------------|
//! | 33  | User can export the audit_log, which saves to a tamper-evident json file |
//! | 147 | UI displays an AI-reconstructed badge while any neural gear is live |
//! | 149 | UI displays zero networking_questions to the assisted user during join |
//! | 150 | App survives a crash with ui_shell isolation and never drops the underlying call |
//! | 151 | UI displays a session summary with capabilities used and total data consumed |

pub mod audit_export;
pub mod gear_badge;
pub mod join_screen;
pub mod session_summary;
pub mod ui_shell;

pub use audit_export::{AuditExportError, AuditExporter};
pub use gear_badge::{BadgeState, GearBadge, BADGE_COLOR, BADGE_LABEL};
pub use join_screen::{CodeError, ConnectError, JoinScreen, JoinState};
pub use session_summary::{CapabilitiesUsed, SessionSummary, SessionTracker};
