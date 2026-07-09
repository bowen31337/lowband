//! Audit-log export screen — Feature 33.
//!
//! The UI shell constructs an [`AuditExporter`] for the audit-export screen
//! and calls [`AuditExporter::save`] with the tamper-evident JSON string
//! produced by `AuditLog::export_json()` and the file path chosen by the user
//! in the native file-save dialog.
//!
//! The file produced is a plain JSON object whose entries each carry a
//! 256-bit chain signature; any post-write alteration (field edits, entry
//! removal, or reordering) is detectable by re-running
//! `AuditLog::verify_entries` against the original session key.
//!
//! # Example
//!
//! ```
//! use lowband_shells::audit_export::AuditExporter;
//! use std::path::Path;
//!
//! // `json` comes from `AuditLog::export_json()` in the daemon.
//! let json = r#"{"entries":[]}"#;
//! let path = std::env::temp_dir().join("session_audit.json");
//! let bytes = AuditExporter::save(json, &path).expect("save must succeed");
//! assert_eq!(bytes, json.len());
//! # let _ = std::fs::remove_file(&path);
//! ```

use std::fmt;
use std::io;
use std::path::Path;

/// Errors returned by [`AuditExporter::save`].
#[derive(Debug)]
pub enum AuditExportError {
    /// The supplied path is empty or its parent directory does not exist.
    InvalidPath,
    /// An I/O error occurred while writing the export file.
    Io(io::Error),
}

impl fmt::Display for AuditExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuditExportError::InvalidPath => write!(f, "invalid export path"),
            AuditExportError::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl From<io::Error> for AuditExportError {
    fn from(e: io::Error) -> Self {
        AuditExportError::Io(e)
    }
}

/// Drives the audit-export screen: saves a tamper-evident JSON export to disk.
pub struct AuditExporter;

impl AuditExporter {
    /// Write `json` to `path` and return the number of bytes written.
    ///
    /// `json` must be the output of `AuditLog::export_json()`.  The file is
    /// written in a single `write_all` call; on success the bytes on disk are
    /// identical to `json` so callers can verify the file independently.
    ///
    /// Returns [`AuditExportError::InvalidPath`] when `path` is empty or its
    /// parent directory does not exist.
    pub fn save(json: &str, path: &Path) -> Result<usize, AuditExportError> {
        if path.as_os_str().is_empty() {
            return Err(AuditExportError::InvalidPath);
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(AuditExportError::InvalidPath);
            }
        }
        std::fs::write(path, json.as_bytes())?;
        Ok(json.len())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    const SAMPLE_JSON: &str =
        r#"{"entries":[{"event_type":"view_granted","capability":"view","occurred_at_ms":1,"signature":"abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"}]}"#;

    #[test]
    fn save_writes_json_to_file() {
        let path = env::temp_dir().join("audit_export_screen_test_write.json");
        let bytes = AuditExporter::save(SAMPLE_JSON, &path).expect("save must succeed");
        assert_eq!(bytes, SAMPLE_JSON.len());

        let content = std::fs::read_to_string(&path).expect("file must be readable");
        assert_eq!(content, SAMPLE_JSON, "file content must match input json");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_returns_byte_count() {
        let path = env::temp_dir().join("audit_export_screen_test_bytes.json");
        let json = r#"{"entries":[]}"#;
        let bytes = AuditExporter::save(json, &path).expect("save must succeed");
        assert_eq!(bytes, json.len());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_empty_entries_json_succeeds() {
        let path = env::temp_dir().join("audit_export_screen_test_empty.json");
        let json = r#"{"entries":[]}"#;
        AuditExporter::save(json, &path).expect("empty entries must save successfully");

        let content = std::fs::read_to_string(&path).expect("file must be readable");
        assert_eq!(content, json);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_empty_path_returns_invalid_path() {
        let path = Path::new("");
        let result = AuditExporter::save(SAMPLE_JSON, path);
        assert!(
            matches!(result, Err(AuditExportError::InvalidPath)),
            "empty path must return InvalidPath"
        );
    }

    #[test]
    fn save_nonexistent_parent_returns_invalid_path() {
        let path = Path::new("/nonexistent_lowband_dir_xyz/audit.json");
        let result = AuditExporter::save(SAMPLE_JSON, path);
        assert!(
            matches!(result, Err(AuditExportError::InvalidPath)),
            "nonexistent parent directory must return InvalidPath"
        );
    }

    #[test]
    fn save_overwrites_existing_file() {
        let path = env::temp_dir().join("audit_export_screen_test_overwrite.json");
        AuditExporter::save(r#"{"entries":[]}"#, &path).expect("first write must succeed");
        AuditExporter::save(SAMPLE_JSON, &path).expect("overwrite must succeed");

        let content = std::fs::read_to_string(&path).expect("file must be readable");
        assert_eq!(content, SAMPLE_JSON, "overwrite must replace prior content");

        let _ = std::fs::remove_file(&path);
    }
}
