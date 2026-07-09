//! LowBand UI shell utilities (`lowband-shells`).
//!
//! | Feature | Description |
//! |---------|-------------|
//! | 33  | User can export the audit_log, which saves to a tamper-evident json file |
//! | 150 | App survives a crash with ui_shell isolation and never drops the underlying call |
//! | 151 | UI displays a session summary with capabilities used and total data consumed |

pub mod audit_export;
pub mod session_summary;
pub mod ui_shell;

pub use audit_export::{AuditExportError, AuditExporter};
pub use session_summary::{CapabilitiesUsed, SessionSummary, SessionTracker};
