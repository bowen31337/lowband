//! UC-4 compliance integration test — Feature 176.
//!
//! Verifies that view, control, file, and clipboard grants are enforced on
//! every event and that the signed audit_log persists a tamper-evident record
//! of each grant, revoke, and rejection event.

use lowband_messaging::{
    audit::AuditLog,
    clipboard::{ClipboardError, ClipboardGrant, ClipboardSession},
    grants::{
        CapabilityError, ConsentGrant,
        ControlGrant, ControlSession, FileGrant, FileSession, ViewGrant, ViewSession,
    },
};

// Deterministic 32-byte session key shared across UC-4 tests.
const SESSION_KEY: [u8; 32] = *b"lowband-uc4-test-session-key-xyz";

fn test_log() -> AuditLog {
    AuditLog::new(SESSION_KEY)
}

// ── View grant ────────────────────────────────────────────────────────────────

#[test]
fn uc4_view_grant_enforced_per_frame() {
    let mut session = ViewSession::new();
    let mut log = test_log();

    // Without grant: frame rejected.
    assert_eq!(
        session.apply_frame(),
        Err(CapabilityError::NoActiveGrant),
        "view frame must be rejected before a grant is issued"
    );
    log.append("view_rejected_pregrant", Some("view"), 1);

    // Grant issued: frame accepted.
    session.set_grant(Some(ViewGrant::new()));
    log.append("view_granted", Some("view"), 2);
    assert!(
        session.apply_frame().is_ok(),
        "view frame must be accepted with an active grant"
    );
    log.append("view_frame_accepted", Some("view"), 3);

    // Grant revoked: frame rejected again.
    session.set_grant(None);
    log.append("view_revoked", Some("view"), 4);
    assert_eq!(
        session.apply_frame(),
        Err(CapabilityError::NoActiveGrant),
        "view frame must be rejected after grant is revoked"
    );
    log.append("view_rejected_postrevoke", Some("view"), 5);

    assert!(log.verify(), "audit_log must verify after view grant lifecycle");
    assert_eq!(log.entries().len(), 5);
}

// ── Control grant ─────────────────────────────────────────────────────────────

#[test]
fn uc4_control_grant_enforced_per_event() {
    let mut session = ControlSession::new();
    let mut log = test_log();

    assert_eq!(
        session.apply_event(),
        Err(CapabilityError::NoActiveGrant),
        "control event must be rejected before a grant is issued"
    );
    log.append("control_rejected_pregrant", Some("control"), 1);

    session.set_grant(Some(ControlGrant::new()));
    log.append("control_granted", Some("control"), 2);
    assert!(
        session.apply_event().is_ok(),
        "control event must be accepted with an active grant"
    );

    session.set_grant(None);
    log.append("control_revoked", Some("control"), 3);
    assert_eq!(
        session.apply_event(),
        Err(CapabilityError::NoActiveGrant),
        "control event must be rejected after revocation"
    );

    assert!(log.verify(), "audit_log must verify after control grant lifecycle");
    assert_eq!(log.entries().len(), 3);
}

// ── File grant ────────────────────────────────────────────────────────────────

#[test]
fn uc4_file_grant_enforced_per_chunk() {
    let mut session = FileSession::new();
    let mut log = test_log();

    assert_eq!(
        session.apply_chunk(b"sensitive-payload"),
        Err(CapabilityError::NoActiveGrant),
        "file chunk must be rejected before a grant is issued"
    );
    log.append("file_rejected_pregrant", Some("file"), 1);

    session.set_grant(Some(FileGrant::new()));
    log.append("file_granted", Some("file"), 2);
    assert!(
        session.apply_chunk(b"chunk-data").is_ok(),
        "file chunk must be accepted with an active grant"
    );

    session.set_grant(None);
    log.append("file_revoked", Some("file"), 3);
    assert_eq!(
        session.apply_chunk(b"after-revoke"),
        Err(CapabilityError::NoActiveGrant),
        "file chunk must be rejected after grant revocation"
    );

    assert!(log.verify(), "audit_log must verify after file grant lifecycle");
    assert_eq!(log.entries().len(), 3);
}

// ── Clipboard grant ───────────────────────────────────────────────────────────

#[test]
fn uc4_clipboard_grant_enforced_per_event() {
    let mut session = ClipboardSession::new();
    let mut log = test_log();

    assert_eq!(
        session.apply_remote("remote text before consent"),
        Err(ClipboardError::NoActiveGrant),
        "clipboard must be rejected before a grant is issued"
    );
    log.append("clipboard_rejected_pregrant", Some("clipboard"), 1);

    session.set_grant(Some(ClipboardGrant::new()));
    log.append("clipboard_granted", Some("clipboard"), 2);
    assert!(
        session.apply_remote("hello from peer").is_ok(),
        "clipboard must be accepted with an active grant"
    );

    session.set_grant(None);
    log.append("clipboard_revoked", Some("clipboard"), 3);
    assert_eq!(
        session.apply_remote("post-revoke text"),
        Err(ClipboardError::NoActiveGrant),
        "clipboard must be rejected after grant revocation"
    );

    assert!(log.verify(), "audit_log must verify after clipboard grant lifecycle");
    assert_eq!(log.entries().len(), 3);
}

// ── Audit log tamper evidence ─────────────────────────────────────────────────

#[test]
fn uc4_audit_log_detects_tampering() {
    let mut log = AuditLog::new(SESSION_KEY);
    log.append("view_granted", Some("view"), 100);
    log.append("control_granted", Some("control"), 101);
    log.append("session_ended", None, 102);

    assert!(log.verify(), "clean log must verify");

    // Tamper with the second entry's event_type.
    let mut entries = log.entries().to_vec();
    entries[1].event_type = "privilege_escalation".to_string();
    assert!(
        !AuditLog::verify_entries(&entries, &SESSION_KEY),
        "tampered event_type must break signature verification"
    );

    // Tamper with a timestamp.
    let mut entries2 = log.entries().to_vec();
    entries2[0].occurred_at_ms = 0;
    assert!(
        !AuditLog::verify_entries(&entries2, &SESSION_KEY),
        "tampered timestamp must break signature verification"
    );

    // Reorder entries.
    let mut entries3 = log.entries().to_vec();
    entries3.swap(0, 2);
    assert!(
        !AuditLog::verify_entries(&entries3, &SESSION_KEY),
        "reordered entries must break chain verification"
    );
}

// ── All four grants in one compliance scenario ────────────────────────────────

#[test]
fn uc4_all_grants_enforced_and_audit_log_persisted() {
    let mut view = ViewSession::new();
    let mut control = ControlSession::new();
    let mut file = FileSession::new();
    let mut clipboard = ClipboardSession::new();
    let mut log = AuditLog::new(SESSION_KEY);

    let mut ts: u64 = 0;
    let mut tick = || {
        ts += 1;
        ts
    };

    // -- Before any grants: all capabilities rejected. -------------------------

    assert_eq!(view.apply_frame(),             Err(CapabilityError::NoActiveGrant));
    assert_eq!(control.apply_event(),          Err(CapabilityError::NoActiveGrant));
    assert_eq!(file.apply_chunk(b"payload"),   Err(CapabilityError::NoActiveGrant));
    assert_eq!(clipboard.apply_remote("text"), Err(ClipboardError::NoActiveGrant));

    // -- Issue all four grants. ------------------------------------------------

    view.set_grant(Some(ViewGrant::new()));
    log.append("view_granted", Some("view"), tick());

    control.set_grant(Some(ControlGrant::new()));
    log.append("control_granted", Some("control"), tick());

    file.set_grant(Some(FileGrant::new()));
    log.append("file_granted", Some("file"), tick());

    clipboard.set_grant(Some(ClipboardGrant::new()));
    log.append("clipboard_granted", Some("clipboard"), tick());

    // -- All four accepted. ----------------------------------------------------

    assert!(view.apply_frame().is_ok(),              "view must be accepted with grant");
    assert!(control.apply_event().is_ok(),           "control must be accepted with grant");
    assert!(file.apply_chunk(b"data").is_ok(),       "file must be accepted with grant");
    assert!(clipboard.apply_remote("hello").is_ok(), "clipboard must be accepted with grant");

    // -- Revoke all grants. ----------------------------------------------------

    view.set_grant(None);
    log.append("view_revoked", Some("view"), tick());

    control.set_grant(None);
    log.append("control_revoked", Some("control"), tick());

    file.set_grant(None);
    log.append("file_revoked", Some("file"), tick());

    clipboard.set_grant(None);
    log.append("clipboard_revoked", Some("clipboard"), tick());

    // -- All four rejected again. ----------------------------------------------

    assert_eq!(view.apply_frame(),             Err(CapabilityError::NoActiveGrant));
    assert_eq!(control.apply_event(),          Err(CapabilityError::NoActiveGrant));
    assert_eq!(file.apply_chunk(b"payload"),   Err(CapabilityError::NoActiveGrant));
    assert_eq!(clipboard.apply_remote("text"), Err(ClipboardError::NoActiveGrant));

    // -- Audit log: 8 signed entries, all verifiable. -------------------------

    assert_eq!(log.entries().len(), 8, "expected 8 grant/revoke audit entries");
    assert!(log.verify(), "audit_log must verify across the full compliance scenario");

    // -- JSON export contains key event labels and signature field. -----------

    let json = log.export_json();
    for label in &[
        "view_granted", "control_granted", "file_granted", "clipboard_granted",
        "view_revoked", "control_revoked", "file_revoked", "clipboard_revoked",
    ] {
        assert!(json.contains(label), "export_json must contain '{label}';\njson={json}");
    }
    assert!(json.contains("\"signature\""), "export_json must include signature field");
}

// ── Consent withdrawal — all four tokens invalidated by a single call ─────────

#[test]
fn uc4_all_tokens_invalidated_instantly_on_consent_withdrawal() {
    // Issue all three non-clipboard grants from ConsentGrant and bind clipboard
    // to the same handle.
    let (view_grant, ctrl_grant, file_grant, handle) = ConsentGrant::new().issue_all();
    let clipboard_grant = ClipboardGrant::with_consent(handle.clone());

    let mut view      = ViewSession::new();
    let mut control   = ControlSession::new();
    let mut file      = FileSession::new();
    let mut clipboard = ClipboardSession::new();

    view.set_grant(Some(view_grant));
    control.set_grant(Some(ctrl_grant));
    file.set_grant(Some(file_grant));
    clipboard.set_grant(Some(clipboard_grant));

    // All four accept while consent is active.
    assert!(view.apply_frame().is_ok(),              "view must accept before withdrawal");
    assert!(control.apply_event().is_ok(),           "control must accept before withdrawal");
    assert!(file.apply_chunk(b"data").is_ok(),       "file must accept before withdrawal");
    assert!(clipboard.apply_remote("hello").is_ok(), "clipboard must accept before withdrawal");

    // Single withdrawal — all four invalidated atomically.
    handle.withdraw();

    assert_eq!(
        view.apply_frame(),
        Err(CapabilityError::ConsentWithdrawn),
        "view must be rejected instantly after consent withdrawal",
    );
    assert_eq!(
        control.apply_event(),
        Err(CapabilityError::ConsentWithdrawn),
        "control must be rejected instantly after consent withdrawal",
    );
    assert_eq!(
        file.apply_chunk(b"data"),
        Err(CapabilityError::ConsentWithdrawn),
        "file must be rejected instantly after consent withdrawal",
    );
    assert_eq!(
        clipboard.apply_remote("hello"),
        Err(ClipboardError::ConsentWithdrawn),
        "clipboard must be rejected instantly after consent withdrawal",
    );
}
