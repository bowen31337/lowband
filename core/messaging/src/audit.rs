//! Signed, tamper-evident audit log — Feature 171.
//!
//! Each [`AuditEntry`] is protected by a 256-bit keyed signature derived via
//! four independent SipHash-2-4 instances that collectively commit to the
//! entry's content **and** to all preceding entries through a hash chain.
//! Verifying the chain is O(n) via [`AuditLog::verify`] or the static
//! [`AuditLog::verify_entries`] (useful for externally-loaded export slices).
//!
//! The signed payload covers identity keys, capability grants, and timestamps
//! so that any post-write tampering with any of these fields is detectable.
//!
//! # Signing scheme
//!
//! ```text
//! sig[0]   = KH(key, chain_hash[init=0] || len(event_type) || event_type
//!                    || len(capability) || capability
//!                    || identity_key_flag(1B) || [identity_key(32B) if present]
//!                    || occurred_at_ms)
//! chain[0] = KH(key_derived, sig[0])
//! sig[n]   = KH(key, chain[n-1] || ...)
//! chain[n] = KH(key_derived, sig[n])
//! ```
//!
//! where `KH` is the 256-bit keyed hash (`sip256_keyed`).
//!
//! No external crates required — the hasher is a pure-Rust SipHash-2-4
//! construction extended to 256 bits by running four independent instances
//! with derived sub-keys.
//!
//! # Example
//!
//! ```
//! use lowband_messaging::audit::AuditLog;
//!
//! const KEY: [u8; 32] = *b"example-session-key-32-bytes-xyz";
//! const PEER: [u8; 32] = [0x42u8; 32];
//!
//! let mut log = AuditLog::new(KEY);
//! log.append_with_identity("identity_verified", None,            &PEER, 999);
//! log.append("view_granted",    Some("view"),    1000);
//! log.append("control_granted", Some("control"), 1001);
//! log.append("session_ended",   None,            1002);
//!
//! assert!(log.verify());
//! let json = log.export_json();
//! assert!(json.contains("view_granted"));
//! assert!(json.contains("identity_verified"));
//! ```

/// A single signed entry in the audit log.
#[derive(Clone, Debug)]
pub struct AuditEntry {
    /// Category label for the logged event (e.g. `"view_granted"`, `"session_ended"`).
    pub event_type: String,
    /// The capability the event relates to, if any (`"view"`, `"control"`, `"file"`, `"clipboard"`).
    pub capability: Option<String>,
    /// Static public key of the peer identity involved in this event, if applicable.
    ///
    /// Included in the signed payload so that substituting a different identity
    /// key breaks the signature.  Encoded as lowercase hex in the JSON export.
    pub identity_key: Option<[u8; 32]>,
    /// Wall-clock milliseconds since the Unix epoch, supplied by the caller.
    pub occurred_at_ms: u64,
    /// 256-bit keyed-hash signature binding this entry to the key and the chain.
    pub signature: [u8; 32],
}

/// Signed, tamper-evident audit log for one LowBand session.
///
/// The session key is supplied at construction; entries are chained so that
/// no entry can be removed, reordered, or altered without breaking every
/// subsequent signature.
pub struct AuditLog {
    key: [u8; 32],
    chain_hash: [u8; 32],
    entries: Vec<AuditEntry>,
}

impl AuditLog {
    /// Create an empty log protected by `key`.
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key,
            chain_hash: [0u8; 32],
            entries: Vec::new(),
        }
    }

    /// Append a new signed entry and advance the chain hash.
    ///
    /// Use [`AuditLog::append_with_identity`] when the event involves a known
    /// peer whose static public key should be committed into the signature.
    pub fn append(
        &mut self,
        event_type: &str,
        capability: Option<&str>,
        occurred_at_ms: u64,
    ) {
        self.append_inner(event_type, capability, None, occurred_at_ms);
    }

    /// Append a signed entry that commits to a peer identity key.
    ///
    /// The 32-byte `identity_key` is folded into the signing payload so that
    /// any post-write substitution of the key breaks the chain signature.  Use
    /// this for events like `"identity_verified"` or `"peer_connected"` where
    /// the remote peer's static public key is material to the audit record.
    pub fn append_with_identity(
        &mut self,
        event_type: &str,
        capability: Option<&str>,
        identity_key: &[u8; 32],
        occurred_at_ms: u64,
    ) {
        self.append_inner(event_type, capability, Some(identity_key), occurred_at_ms);
    }

    fn append_inner(
        &mut self,
        event_type: &str,
        capability: Option<&str>,
        identity_key: Option<&[u8; 32]>,
        occurred_at_ms: u64,
    ) {
        let sig = sign_entry(
            &self.key,
            &self.chain_hash,
            event_type,
            capability,
            identity_key,
            occurred_at_ms,
        );
        self.chain_hash = advance_chain(&self.key, &sig);
        self.entries.push(AuditEntry {
            event_type: event_type.to_string(),
            capability: capability.map(str::to_string),
            identity_key: identity_key.copied(),
            occurred_at_ms,
            signature: sig,
        });
    }

    /// All entries in append order.
    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    /// Verify the entire log using the stored session key.
    pub fn verify(&self) -> bool {
        Self::verify_entries(&self.entries, &self.key)
    }

    /// Verify a (possibly external) slice of entries against `key`.
    ///
    /// Returns `false` on the first entry whose signature does not match.
    /// An empty slice always returns `true`.
    pub fn verify_entries(entries: &[AuditEntry], key: &[u8; 32]) -> bool {
        let mut chain_hash = [0u8; 32];
        for entry in entries {
            let expected = sign_entry(
                key,
                &chain_hash,
                &entry.event_type,
                entry.capability.as_deref(),
                entry.identity_key.as_ref(),
                entry.occurred_at_ms,
            );
            if entry.signature != expected {
                return false;
            }
            chain_hash = advance_chain(key, &entry.signature);
        }
        true
    }

    /// Serialise the log as a JSON object for tamper-evident archival.
    ///
    /// Signatures are encoded as lowercase hex.
    pub fn export_json(&self) -> String {
        let mut buf = String::from("{\"entries\":[");
        for (i, e) in self.entries.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            buf.push_str(&entry_to_json(e));
        }
        buf.push_str("]}");
        buf
    }

    /// Write the tamper-evident JSON export to `path`.
    ///
    /// The file contains the same content as [`AuditLog::export_json`] with
    /// each entry's 256-bit chain signature encoded as lowercase hex.  Any
    /// post-write tampering (field edits, entry removal, reordering) is
    /// detectable by re-running [`AuditLog::verify_entries`] against the
    /// session key used at construction.
    pub fn export_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        let json = self.export_json();
        std::fs::write(path, json.as_bytes())
    }
}

// ── Keyed hash (pure-Rust SipHash-2-4 → 256-bit) ─────────────────────────────

/// SipHash-2-4 core.  Returns a 64-bit MAC.
fn siphash24(k0: u64, k1: u64, msg: &[u8]) -> u64 {
    let mut v0 = k0 ^ 0x736f6d6570736575u64;
    let mut v1 = k1 ^ 0x646f72616e646f6du64;
    let mut v2 = k0 ^ 0x6c7967656e657261u64;
    let mut v3 = k1 ^ 0x7465646279746573u64;

    macro_rules! sipr {
        () => {
            v0 = v0.wrapping_add(v1); v1 = v1.rotate_left(13); v1 ^= v0; v0 = v0.rotate_left(32);
            v2 = v2.wrapping_add(v3); v3 = v3.rotate_left(16); v3 ^= v2;
            v0 = v0.wrapping_add(v3); v3 = v3.rotate_left(21); v3 ^= v0;
            v2 = v2.wrapping_add(v1); v1 = v1.rotate_left(17); v1 ^= v2; v2 = v2.rotate_left(32);
        };
    }

    let blocks = msg.len() / 8;
    for i in 0..blocks {
        let m = u64::from_le_bytes(msg[i * 8..(i + 1) * 8].try_into().unwrap());
        v3 ^= m;
        sipr!();
        sipr!();
        v0 ^= m;
    }

    let rem = msg.len() - blocks * 8;
    let mut last = (msg.len() as u64) << 56;
    for i in 0..rem {
        last |= (msg[blocks * 8 + i] as u64) << (i * 8);
    }
    v3 ^= last;
    sipr!();
    sipr!();
    v0 ^= last;

    v2 ^= 0xff;
    sipr!();
    sipr!();
    sipr!();
    sipr!();
    v0 ^ v1 ^ v2 ^ v3
}

/// 256-bit keyed hash over `msg` using four independent SipHash-2-4 instances
/// derived from the 32-byte `key`.
fn kh256(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let k0a = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let k0b = u64::from_le_bytes(key[8..16].try_into().unwrap());
    let k1a = u64::from_le_bytes(key[16..24].try_into().unwrap());
    let k1b = u64::from_le_bytes(key[24..32].try_into().unwrap());

    let h0 = siphash24(k0a, k0b, msg);
    let h1 = siphash24(k1a, k1b, msg);
    let h2 = siphash24(k0a ^ k1b, k0b ^ k1a, msg);
    let h3 = siphash24(
        k0a.wrapping_add(k1a),
        k0b.wrapping_add(k1b),
        msg,
    );

    let mut out = [0u8; 32];
    out[0..8].copy_from_slice(&h0.to_le_bytes());
    out[8..16].copy_from_slice(&h1.to_le_bytes());
    out[16..24].copy_from_slice(&h2.to_le_bytes());
    out[24..32].copy_from_slice(&h3.to_le_bytes());
    out
}

// ── Entry signing and chain advancement ───────────────────────────────────────

fn sign_entry(
    key: &[u8; 32],
    chain_hash: &[u8; 32],
    event_type: &str,
    capability: Option<&str>,
    identity_key: Option<&[u8; 32]>,
    occurred_at_ms: u64,
) -> [u8; 32] {
    let cap = capability.unwrap_or("");
    let mut buf: Vec<u8> = Vec::with_capacity(
        32 + 4 + event_type.len() + 4 + cap.len() + 1 + 32 + 8,
    );
    buf.extend_from_slice(chain_hash);
    buf.extend_from_slice(&(event_type.len() as u32).to_le_bytes());
    buf.extend_from_slice(event_type.as_bytes());
    buf.extend_from_slice(&(cap.len() as u32).to_le_bytes());
    buf.extend_from_slice(cap.as_bytes());
    // Commit to the identity key with a presence flag so that swapping
    // None ↔ Some or substituting a different key breaks the signature.
    match identity_key {
        None => buf.push(0u8),
        Some(k) => {
            buf.push(1u8);
            buf.extend_from_slice(k);
        }
    }
    buf.extend_from_slice(&occurred_at_ms.to_le_bytes());
    kh256(key, &buf)
}

fn advance_chain(key: &[u8; 32], sig: &[u8; 32]) -> [u8; 32] {
    // Derive a distinct sub-key so the chain hash and the entry sig use
    // different key schedules.
    let mut derived = *key;
    for (i, b) in derived.iter_mut().enumerate() {
        *b ^= 0x5c_u8.wrapping_add(i as u8);
    }
    kh256(&derived, sig)
}

// ── JSON export ───────────────────────────────────────────────────────────────

fn entry_to_json(e: &AuditEntry) -> String {
    let cap_field = match &e.capability {
        Some(c) => format!("\"{}\"", c),
        None => "null".to_string(),
    };
    let id_key_field = match &e.identity_key {
        Some(k) => {
            let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
            format!("\"{}\"", hex)
        }
        None => "null".to_string(),
    };
    let sig_hex: String = e.signature.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{{\"event_type\":\"{}\",\"capability\":{},\"identity_key\":{},\
         \"occurred_at_ms\":{},\"signature\":\"{}\"}}",
        e.event_type, cap_field, id_key_field, e.occurred_at_ms, sig_hex
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: [u8; 32] = *b"audit-test-key-for-unit-tests-xx";

    #[test]
    fn empty_log_verifies() {
        let log = AuditLog::new(TEST_KEY);
        assert!(log.verify());
        assert_eq!(log.entries().len(), 0);
    }

    #[test]
    fn single_entry_verifies() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("session_start", None, 1000);
        assert!(log.verify());
        assert_eq!(log.entries().len(), 1);
    }

    #[test]
    fn multi_entry_chain_verifies() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("view_granted", Some("view"), 1);
        log.append("control_granted", Some("control"), 2);
        log.append("session_ended", None, 3);
        assert!(log.verify());
        assert_eq!(log.entries().len(), 3);
    }

    #[test]
    fn tampered_event_type_fails_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("view_granted", Some("view"), 1);
        log.append("control_granted", Some("control"), 2);

        let mut entries = log.entries().to_vec();
        entries[0].event_type = "injected_event".to_string();
        assert!(!AuditLog::verify_entries(&entries, &TEST_KEY));
    }

    #[test]
    fn tampered_capability_fails_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("grant_issued", Some("view"), 1);

        let mut entries = log.entries().to_vec();
        entries[0].capability = Some("control".to_string());
        assert!(!AuditLog::verify_entries(&entries, &TEST_KEY));
    }

    #[test]
    fn tampered_timestamp_fails_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("session_start", None, 1000);

        let mut entries = log.entries().to_vec();
        entries[0].occurred_at_ms = 9999;
        assert!(!AuditLog::verify_entries(&entries, &TEST_KEY));
    }

    #[test]
    fn reordered_entries_fail_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("first", None, 1);
        log.append("second", None, 2);

        let mut entries = log.entries().to_vec();
        entries.swap(0, 1);
        assert!(!AuditLog::verify_entries(&entries, &TEST_KEY));
    }

    #[test]
    fn wrong_key_fails_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("view_granted", Some("view"), 1);

        let wrong_key = [0u8; 32];
        assert!(!AuditLog::verify_entries(log.entries(), &wrong_key));
    }

    #[test]
    fn removed_entry_fails_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("first", None, 1);
        log.append("second", None, 2);
        log.append("third", None, 3);

        let mut entries = log.entries().to_vec();
        entries.remove(1);
        assert!(!AuditLog::verify_entries(&entries, &TEST_KEY));
    }

    #[test]
    fn export_json_contains_required_fields() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("view_granted", Some("view"), 1);
        log.append("control_revoked", Some("control"), 2);
        log.append("session_ended", None, 3);

        let json = log.export_json();
        assert!(json.contains("view_granted"), "json: {json}");
        assert!(json.contains("control_revoked"), "json: {json}");
        assert!(json.contains("session_ended"), "json: {json}");
        assert!(json.contains("\"signature\""), "json: {json}");
        assert!(json.contains("\"capability\":null"), "json: {json}");
    }

    #[test]
    fn signatures_differ_between_entries() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("event_a", Some("view"), 1);
        log.append("event_a", Some("view"), 1); // same content, different chain position

        let sigs: Vec<_> = log.entries().iter().map(|e| e.signature).collect();
        assert_ne!(
            sigs[0], sigs[1],
            "two entries with identical content but different chain positions must have different sigs"
        );
    }

    #[test]
    fn export_to_file_writes_readable_json() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("view_granted", Some("view"), 1);
        log.append("control_granted", Some("control"), 2);
        log.append("session_ended", None, 3);

        let path = std::env::temp_dir().join("lowband_audit_export_test_write.json");
        log.export_to_file(&path).expect("export_to_file must succeed");

        let content = std::fs::read_to_string(&path).expect("exported file must be readable");
        assert!(content.contains("view_granted"), "json: {content}");
        assert!(content.contains("control_granted"), "json: {content}");
        assert!(content.contains("session_ended"), "json: {content}");
        assert!(content.contains("\"signature\""), "json: {content}");
        assert!(content.contains("\"capability\":null"), "json: {content}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_to_file_content_matches_export_json() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("grant_issued", Some("file"), 100);
        log.append("session_ended", None, 200);

        let path = std::env::temp_dir().join("lowband_audit_export_test_match.json");
        log.export_to_file(&path).expect("export_to_file must succeed");

        let from_file = std::fs::read_to_string(&path).expect("must be readable");
        let from_method = log.export_json();
        assert_eq!(from_file, from_method, "file content must equal export_json() output");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_to_file_empty_log_writes_valid_json() {
        let log = AuditLog::new(TEST_KEY);
        let path = std::env::temp_dir().join("lowband_audit_export_test_empty.json");
        log.export_to_file(&path).expect("empty log export must succeed");

        let content = std::fs::read_to_string(&path).expect("must be readable");
        assert_eq!(content, "{\"entries\":[]}", "empty log must produce empty entries array");

        let _ = std::fs::remove_file(&path);
    }

    // ── Identity key coverage (Feature 171) ──────────────────────────────────

    const PEER_KEY_A: [u8; 32] = [0x11u8; 32];
    const PEER_KEY_B: [u8; 32] = [0x22u8; 32];

    #[test]
    fn identity_key_entry_verifies() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append_with_identity("identity_verified", None, &PEER_KEY_A, 1000);
        assert!(log.verify());
        assert_eq!(log.entries()[0].identity_key, Some(PEER_KEY_A));
    }

    #[test]
    fn mixed_identity_and_plain_entries_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append_with_identity("identity_verified", None, &PEER_KEY_A, 1);
        log.append("view_granted", Some("view"), 2);
        log.append_with_identity("peer_connected", None, &PEER_KEY_B, 3);
        log.append("session_ended", None, 4);
        assert!(log.verify());
        assert_eq!(log.entries().len(), 4);
        assert_eq!(log.entries()[0].identity_key, Some(PEER_KEY_A));
        assert_eq!(log.entries()[1].identity_key, None);
        assert_eq!(log.entries()[2].identity_key, Some(PEER_KEY_B));
        assert_eq!(log.entries()[3].identity_key, None);
    }

    #[test]
    fn tampered_identity_key_fails_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append_with_identity("identity_verified", None, &PEER_KEY_A, 1000);

        let mut entries = log.entries().to_vec();
        entries[0].identity_key = Some(PEER_KEY_B);
        assert!(
            !AuditLog::verify_entries(&entries, &TEST_KEY),
            "substituted identity_key must break signature"
        );
    }

    #[test]
    fn identity_key_none_to_some_fails_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("view_granted", Some("view"), 1);

        let mut entries = log.entries().to_vec();
        entries[0].identity_key = Some(PEER_KEY_A);
        assert!(
            !AuditLog::verify_entries(&entries, &TEST_KEY),
            "adding an identity_key to an unsigned-identity entry must break signature"
        );
    }

    #[test]
    fn identity_key_some_to_none_fails_verify() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append_with_identity("identity_verified", None, &PEER_KEY_A, 1);

        let mut entries = log.entries().to_vec();
        entries[0].identity_key = None;
        assert!(
            !AuditLog::verify_entries(&entries, &TEST_KEY),
            "removing the identity_key from an identity entry must break signature"
        );
    }

    #[test]
    fn export_json_includes_identity_key_as_hex() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append_with_identity("identity_verified", None, &PEER_KEY_A, 1);
        let json = log.export_json();
        let expected_hex = "11".repeat(32);
        assert!(
            json.contains(&format!("\"identity_key\":\"{}\"", expected_hex)),
            "json must contain identity_key as lowercase hex; json={json}"
        );
    }

    #[test]
    fn export_json_null_identity_key_for_plain_entry() {
        let mut log = AuditLog::new(TEST_KEY);
        log.append("view_granted", Some("view"), 1);
        let json = log.export_json();
        assert!(
            json.contains("\"identity_key\":null"),
            "plain entry must export identity_key as null; json={json}"
        );
    }

    #[test]
    fn identity_key_entries_differ_from_plain_entries_with_same_fields() {
        let mut log_plain    = AuditLog::new(TEST_KEY);
        let mut log_identity = AuditLog::new(TEST_KEY);
        log_plain.append("identity_verified", None, 1);
        log_identity.append_with_identity("identity_verified", None, &PEER_KEY_A, 1);
        assert_ne!(
            log_plain.entries()[0].signature,
            log_identity.entries()[0].signature,
            "entry with identity_key must have a different signature than the same entry without"
        );
    }

    #[test]
    fn two_different_identity_keys_produce_different_signatures() {
        let mut log_a = AuditLog::new(TEST_KEY);
        let mut log_b = AuditLog::new(TEST_KEY);
        log_a.append_with_identity("identity_verified", None, &PEER_KEY_A, 1);
        log_b.append_with_identity("identity_verified", None, &PEER_KEY_B, 1);
        assert_ne!(
            log_a.entries()[0].signature,
            log_b.entries()[0].signature,
            "different identity keys must produce different signatures"
        );
    }
}
