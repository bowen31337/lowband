//! LowBand UI shell utilities (`lowband-shells`).
//!
//! | Feature | Description |
//! |---------|-------------|
//! | 33  | User can export the audit_log, which saves to a tamper-evident json file |
//! | 139 | UI shows a TCP-443 relay warning with an honest latency penalty label |
//! | 145 | UI displays a panic control that severs injection on both sides |
//! | 146 | UI shows the current tier, bitrate, RTT, and loss in an honest quality bar |
//! | 147 | UI displays an AI-reconstructed badge while any neural gear is live |
//! | 149 | UI displays zero networking_questions to the assisted user during join |
//! | 150 | App survives a crash with ui_shell isolation and never drops the underlying call |
//! | 151 | UI displays a session summary with capabilities used and total data consumed |

pub mod audit_export;
pub mod gear_badge;
pub mod join_screen;
pub mod panic_control;
pub mod quality_bar;
pub mod relay_warning;
pub mod session_summary;
pub mod ui_shell;

pub use audit_export::{AuditExportError, AuditExporter};
pub use gear_badge::{BadgeState, GearBadge, BADGE_COLOR, BADGE_LABEL};
pub use join_screen::{CodeError, ConnectError, JoinScreen, JoinState};
pub use panic_control::{
    PanicControl, PanicControlState,
    PANIC_BUTTON_COLOR, PANIC_BUTTON_LABEL,
    PANIC_SEVERED_COLOR, PANIC_SEVERED_LABEL,
};
pub use quality_bar::{QualityBar, QualitySnapshot, MIN_REFRESH_INTERVAL};
pub use relay_warning::{
    RelayWarning, RelayWarningSnapshot, RELAY_WARNING_COLOR,
    RELAY_PENALTY_LABEL_PREFIX, RELAY_PENALTY_LABEL_SUFFIX,
};
pub use session_summary::{CapabilitiesUsed, SessionSummary, SessionTracker};
