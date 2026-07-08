//! Signed, tamper-evident audit log — Feature 171.
//!
//! Each [`AuditEntry`] is protected by a 256-bit keyed signature derived via
//! four independent SipHash-2-4 instances that collectively commit to the
//! entry's content **and** to all preceding entries through a hash chain.
//! Verifying the chain is O(n) via [`AuditLog::verify`] or the static
//! [`AuditLog::verify_entries`] (useful for externally-loaded export slices).
//!
//! # Signing scheme
//!
//! ```text
//! sig[0]   = KH(key, chain_hash[init=0] || len(event_type) || event_type
//!                    || len(capability) || capability || occurred_at_ms)
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
//!
//! let mut log = AuditLog::new(KEY);
//! log.append("view_granted",    Some("view"),    1000);
//! log.append("control_granted", Some("control"), 1001);
//! log.append("session_ended",   None,            1002);
//!
//! assert!(log.verify());
//! let json = log.export_json();
//! assert!(json.contains("view_granted"));
//! ```

/// A single signed entry in the audit log.
#[derive(Clone, Debug)]
pub struct AuditEntry {
    /// Category label for the logged event (e.g. `"view_granted"`, `"session_ended"`).
    pub event_type: String,
    /// The capability the event relates to, if any (`"view"`, `"control"`, `"file"`, `"clipboard"`).
    pub capability: Option<String>,
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
    pub fn append(
        &mut self,
        event_type: &str,
        capability: Option<&str>,
        occurred_at_ms: u64,
    ) {
        let sig = sign_entry(
            &self.key,
            &self.chain_hash,
            event_type,
            capability,
            occurred_at_ms,
        );
        self.chain_hash = advance_chain(&self.key, &sig);
        self.entries.push(AuditEntry {
            event_type: event_type.to_string(),
            capability: capability.map(str::to_string),
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
    occurred_at_ms: u64,
) -> [u8; 32] {
    let cap = capability.unwrap_or("");
    let mut buf: Vec<u8> = Vec::with_capacity(
        32 + 4 + event_type.len() + 4 + cap.len() + 8,
    );
    buf.extend_from_slice(chain_hash);
    buf.extend_from_slice(&(event_type.len() as u32).to_le_bytes());
    buf.extend_from_slice(event_type.as_bytes());
    buf.extend_from_slice(&(cap.len() as u32).to_le_bytes());
    buf.extend_from_slice(cap.as_bytes());
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
    let sig_hex: String = e.signature.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{{\"event_type\":\"{}\",\"capability\":{},\"occurred_at_ms\":{},\"signature\":\"{}\"}}",
        e.event_type, cap_field, e.occurred_at_ms, sig_hex
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
}
