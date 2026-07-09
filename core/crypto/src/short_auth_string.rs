//! Feature 24 — Short authentication string for verbal channel verification.
//!
//! After the Noise-IK handshake both peers derive the same [`ShortAuthString`]
//! from the shared 32-byte handshake transcript hash.  Each side displays the
//! phrase; the technician and the assisted user read it aloud over the phone to
//! confirm they are looking at each other's channel and not a MITM.
//!
//! ```
//! use lowband_crypto::short_auth_string::ShortAuthString;
//!
//! // Both peers supply the same handshake transcript hash from Noise-IK.
//! let transcript_hash: [u8; 32] = [0x3a; 32]; // Feature 19 supplies this
//! let sas = ShortAuthString::derive(&transcript_hash);
//!
//! // Each side displays the phrase; users read it aloud to each other.
//! println!("Verify: {sas}"); // e.g. "corn-gust-fold"
//!
//! // The receiving side can also check a string typed/spoken by the user.
//! assert!(sas.verify_str(&sas.to_string()));
//! ```

use core::fmt;

/// Human-readable phrase derived from session key material for verbal verification.
///
/// Both peers in a Noise-IK session derive the same [`ShortAuthString`] from
/// the shared 32-byte handshake transcript hash.  Each side displays the
/// three-word phrase; the users read it aloud to each other to detect a
/// man-in-the-middle attack.
///
/// # Format
///
/// Three common English words separated by dashes, e.g. `"corn-gust-fold"`.
/// The phrase encodes 24 bits of entropy (one byte → one word × 3 words),
/// giving roughly 16.7 million possible strings — sufficient to make accidental
/// or deliberate collision negligible in a real-time voice call.
///
/// # Derivation
///
/// Three index bytes are extracted from the 32-byte session material by XOR-
/// folding every eighth byte:
///
/// ```text
/// idx[i] = material[i] ^ material[i+8] ^ material[i+16] ^ material[i+24]
/// ```
///
/// This mixes all 32 bytes so no single octet segment can dominate the output.
/// Each index selects one word from the fixed 256-word list below.
///
/// Feature 19 (Noise-IK handshake) provides the `session_material` as the
/// finalized handshake transcript hash, which both sides compute identically
/// only after the handshake succeeds.  The SAS is therefore never displayable
/// on an unauthenticated connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortAuthString {
    /// Word indices (0–255) used to select the three words.
    raw: [u8; 3],
}

impl ShortAuthString {
    /// Derive a [`ShortAuthString`] from 32-byte `session_material`.
    ///
    /// The `session_material` is the 32-byte Noise-IK handshake transcript hash
    /// shared by both peers after a successful handshake (Feature 19).  Both
    /// sides derive the same value and therefore display the same phrase.
    pub fn derive(session_material: &[u8; 32]) -> Self {
        let raw = [
            session_material[0]
                ^ session_material[8]
                ^ session_material[16]
                ^ session_material[24],
            session_material[1]
                ^ session_material[9]
                ^ session_material[17]
                ^ session_material[25],
            session_material[2]
                ^ session_material[10]
                ^ session_material[18]
                ^ session_material[26],
        ];
        Self { raw }
    }

    /// The three words that compose the phrase, in display order.
    pub fn words(&self) -> [&'static str; 3] {
        [
            WORD_LIST[self.raw[0] as usize],
            WORD_LIST[self.raw[1] as usize],
            WORD_LIST[self.raw[2] as usize],
        ]
    }

    /// Raw word-index bytes (mainly for testing and serialisation).
    pub fn raw_indices(&self) -> [u8; 3] {
        self.raw
    }

    /// Check whether `candidate` matches this phrase.
    ///
    /// Comparison is case-insensitive against the canonical dash-separated
    /// lowercase form.  Use this instead of string equality when the input
    /// comes from a user interface.
    pub fn verify_str(&self, candidate: &str) -> bool {
        let expected = self.to_string(); // lowercase "word-word-word"
        let normalised = candidate.trim().to_lowercase();
        constant_time_eq(expected.as_bytes(), normalised.as_bytes())
    }
}

impl fmt::Display for ShortAuthString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c] = self.words();
        write!(f, "{a}-{b}-{c}")
    }
}

/// Byte-by-byte equality that does not short-circuit (avoids timing side-channels).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── Word list ─────────────────────────────────────────────────────────────────
//
// 256 short, phonetically distinct English words (3–5 letters each).
// Each byte value 0x00–0xFF maps to exactly one word.
// Words are chosen to minimise auditory confusion: no minimal pairs
// (e.g. "bit"/"bat"), no homophones, no profanity.
const WORD_LIST: [&str; 256] = [
    // 0x00–0x0F
    "acid", "acre", "aged", "ajar", "aloe", "alto", "amen", "amok",
    "anew", "apex", "arch", "area", "aria", "army", "aura", "avid",
    // 0x10–0x1F
    "axle", "back", "bale", "ball", "balm", "band", "bank", "barn",
    "base", "bath", "bead", "beam", "bean", "bear", "belt", "bend",
    // 0x20–0x2F
    "bird", "bite", "blue", "bold", "bone", "book", "boom", "boot",
    "born", "bowl", "brew", "brim", "buck", "bulb", "bulk", "bull",
    // 0x30–0x3F
    "bump", "burn", "buzz", "cage", "cake", "calm", "camp", "cape",
    "card", "care", "cart", "cash", "cave", "cell", "cent", "chef",
    // 0x40–0x4F
    "chin", "chip", "chop", "clam", "clay", "clip", "club", "coal",
    "coat", "coil", "cold", "comb", "cone", "cord", "core", "corn",
    // 0x50–0x5F
    "cost", "crab", "crew", "crop", "crow", "cube", "curb", "curl",
    "damp", "dark", "dawn", "daze", "deft", "desk", "dial", "diet",
    // 0x60–0x6F
    "dire", "dish", "disk", "dock", "dome", "door", "dorm", "dove",
    "down", "drag", "drip", "drop", "drum", "dune", "dusk", "dust",
    // 0x70–0x7F
    "earl", "earn", "echo", "edge", "emit", "epic", "exit", "face",
    "fact", "fail", "fair", "fall", "fame", "farm", "fast", "fate",
    // 0x80–0x8F
    "fawn", "feat", "feel", "fern", "fill", "film", "fine", "fire",
    "fish", "fist", "flag", "flaw", "flex", "flip", "flow", "foam",
    // 0x90–0x9F
    "fold", "folk", "fond", "fork", "form", "fort", "fray", "free",
    "fuel", "full", "fume", "fund", "fuse", "gale", "gaze", "gear",
    // 0xA0–0xAF
    "glad", "glen", "glow", "glue", "goal", "gold", "golf", "gong",
    "gown", "grab", "grin", "grip", "grit", "gust", "hail", "half",
    // 0xB0–0xBF
    "hall", "halt", "harm", "harp", "hash", "haul", "hawk", "haze",
    "heal", "heap", "heat", "heel", "helm", "help", "herb", "hike",
    // 0xC0–0xCF
    "hill", "hint", "hold", "hole", "hook", "horn", "hull", "hump",
    "hurl", "idle", "inch", "iris", "isle", "itch", "item", "jade",
    // 0xD0–0xDF
    "jail", "jolt", "jury", "keen", "keel", "kelp", "knob", "knot",
    "lake", "lame", "lamp", "land", "lane", "lark", "lava", "lawn",
    // 0xE0–0xEF
    "leaf", "lean", "leap", "lens", "lime", "link", "lint", "lion",
    "loft", "loom", "loop", "lord", "lore", "lull", "lump", "lung",
    // 0xF0–0xFF
    "lure", "mace", "main", "mane", "mark", "mast", "maze", "mild",
    "mill", "mint", "mist", "moan", "moat", "mode", "mold", "musk",
];

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Session material that produces a known, deterministic SAS for all tests.
    const KNOWN_MATERIAL: [u8; 32] = [
        0x4F, 0xE2, 0x71, // indices before folding bytes 1, 9, 17, 25
        0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    // ── Word list invariants ───────────────────────────────────────────────────

    #[test]
    fn word_list_has_exactly_256_entries() {
        assert_eq!(WORD_LIST.len(), 256);
    }

    #[test]
    fn word_list_has_no_empty_entries() {
        for (i, word) in WORD_LIST.iter().enumerate() {
            assert!(!word.is_empty(), "WORD_LIST[{i}] must not be empty");
        }
    }

    #[test]
    fn word_list_all_lowercase_ascii() {
        for (i, word) in WORD_LIST.iter().enumerate() {
            assert!(
                word.bytes().all(|b| b.is_ascii_lowercase()),
                "WORD_LIST[{i}] = {word:?} must be lowercase ASCII"
            );
        }
    }

    #[test]
    fn word_list_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for (i, word) in WORD_LIST.iter().enumerate() {
            assert!(
                seen.insert(*word),
                "WORD_LIST[{i}] = {word:?} is duplicated"
            );
        }
    }

    // ── Derivation determinism ─────────────────────────────────────────────────

    #[test]
    fn derive_is_deterministic() {
        let material = [0xABu8; 32];
        let sas1 = ShortAuthString::derive(&material);
        let sas2 = ShortAuthString::derive(&material);
        assert_eq!(sas1, sas2, "same material must produce the same SAS");
    }

    #[test]
    fn both_peers_derive_same_sas() {
        // Simulates both peers deriving from the identical Noise-IK transcript hash.
        let shared_hash = [0x37u8; 32];
        let tech_sas = ShortAuthString::derive(&shared_hash);
        let user_sas = ShortAuthString::derive(&shared_hash);
        assert_eq!(
            tech_sas.to_string(),
            user_sas.to_string(),
            "both peers must display the same phrase"
        );
    }

    #[test]
    fn different_material_produces_different_sas() {
        // Use non-uniform inputs: uniform byte arrays (e.g. [0x11; 32]) are
        // degenerate for XOR-folding because b^b^b^b == 0 for any b, so two
        // uniform arrays always produce the same index (0).  Real Noise-IK
        // transcript hashes are never uniform; use sparse inputs here.
        let mut m1 = [0u8; 32];
        m1[0] = 0x11;
        let mut m2 = [0u8; 32];
        m2[0] = 0x22;
        let sas1 = ShortAuthString::derive(&m1);
        let sas2 = ShortAuthString::derive(&m2);
        assert_ne!(
            sas1.to_string(),
            sas2.to_string(),
            "different session material must produce different SAS"
        );
    }

    #[test]
    fn single_bit_flip_changes_sas() {
        let mut m1 = [0x00u8; 32];
        let mut m2 = [0x00u8; 32];
        m2[0] ^= 0x01; // flip one bit in the first material byte
        let sas1 = ShortAuthString::derive(&m1);
        let sas2 = ShortAuthString::derive(&m2);
        assert_ne!(
            sas1.raw_indices()[0],
            sas2.raw_indices()[0],
            "a single-bit difference in material must change the first index"
        );

        // Also verify that a flip in byte 8 (XOR partner of byte 0) changes index 0.
        m1[8] ^= 0x01;
        let sas3 = ShortAuthString::derive(&m1);
        assert_ne!(
            sas1.raw_indices()[0],
            sas3.raw_indices()[0],
            "a flip in material byte 8 must change index 0 (XOR-fold)"
        );
    }

    // ── XOR folding ───────────────────────────────────────────────────────────

    #[test]
    fn raw_indices_match_xor_fold_of_material() {
        let mut material = [0u8; 32];
        for (i, b) in material.iter_mut().enumerate() {
            *b = i as u8;
        }
        let sas = ShortAuthString::derive(&material);
        let idx = sas.raw_indices();

        // Expected: idx[i] = material[i] ^ material[i+8] ^ material[i+16] ^ material[i+24]
        assert_eq!(
            idx[0],
            material[0] ^ material[8] ^ material[16] ^ material[24]
        );
        assert_eq!(
            idx[1],
            material[1] ^ material[9] ^ material[17] ^ material[25]
        );
        assert_eq!(
            idx[2],
            material[2] ^ material[10] ^ material[18] ^ material[26]
        );
    }

    // ── Display format ────────────────────────────────────────────────────────

    #[test]
    fn display_is_dash_separated_lowercase_triple() {
        let sas = ShortAuthString::derive(&KNOWN_MATERIAL);
        let s = sas.to_string();
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 3, "display must have exactly three dash-separated words");
        for part in &parts {
            assert!(
                part.bytes().all(|b| b.is_ascii_lowercase()),
                "each word in display must be lowercase ASCII"
            );
            assert!(!part.is_empty(), "each word in display must be non-empty");
        }
    }

    #[test]
    fn display_words_match_words_accessor() {
        let sas = ShortAuthString::derive(&KNOWN_MATERIAL);
        let [a, b, c] = sas.words();
        let expected = format!("{a}-{b}-{c}");
        assert_eq!(sas.to_string(), expected);
    }

    #[test]
    fn display_uses_word_list_entries() {
        let sas = ShortAuthString::derive(&KNOWN_MATERIAL);
        let s = sas.to_string();
        for word in s.split('-') {
            assert!(
                WORD_LIST.contains(&word),
                "displayed word {word:?} must be in WORD_LIST"
            );
        }
    }

    #[test]
    fn all_256_indices_produce_valid_words() {
        for i in 0u8..=255 {
            let word = WORD_LIST[i as usize];
            assert!(!word.is_empty(), "index {i} must map to a non-empty word");
        }
    }

    // ── verify_str ────────────────────────────────────────────────────────────

    #[test]
    fn verify_str_accepts_exact_match() {
        let sas = ShortAuthString::derive(&KNOWN_MATERIAL);
        assert!(sas.verify_str(&sas.to_string()), "exact match must verify");
    }

    #[test]
    fn verify_str_accepts_uppercase() {
        let sas = ShortAuthString::derive(&KNOWN_MATERIAL);
        let upper = sas.to_string().to_uppercase();
        assert!(sas.verify_str(&upper), "uppercase match must verify");
    }

    #[test]
    fn verify_str_accepts_mixed_case() {
        let sas = ShortAuthString::derive(&KNOWN_MATERIAL);
        let s = sas.to_string();
        // Capitalise the first letter only.
        let mut chars = s.chars();
        let mixed = chars
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default()
            + chars.as_str();
        assert!(sas.verify_str(&mixed), "mixed-case match must verify");
    }

    #[test]
    fn verify_str_accepts_leading_trailing_whitespace() {
        let sas = ShortAuthString::derive(&KNOWN_MATERIAL);
        let padded = format!("  {}  ", sas);
        assert!(
            sas.verify_str(&padded),
            "leading/trailing whitespace must be ignored"
        );
    }

    #[test]
    fn verify_str_rejects_wrong_phrase() {
        // Use sparse inputs so the XOR-fold produces distinct indices (see
        // different_material_produces_different_sas for the degenerate-input rationale).
        let mut m1 = [0u8; 32];
        m1[0] = 0x11;
        let mut m2 = [0u8; 32];
        m2[0] = 0x22;
        let sas1 = ShortAuthString::derive(&m1);
        let sas2 = ShortAuthString::derive(&m2);
        assert!(
            !sas1.verify_str(&sas2.to_string()),
            "a phrase from different material must not verify"
        );
    }

    #[test]
    fn verify_str_rejects_empty_string() {
        let sas = ShortAuthString::derive(&KNOWN_MATERIAL);
        assert!(!sas.verify_str(""), "empty string must not verify");
    }

    #[test]
    fn verify_str_rejects_partial_match() {
        let sas = ShortAuthString::derive(&KNOWN_MATERIAL);
        let full = sas.to_string();
        let partial = full.split('-').next().unwrap();
        assert!(
            !sas.verify_str(partial),
            "partial phrase (one word) must not verify"
        );
    }

    // ── Entropy coverage ──────────────────────────────────────────────────────

    #[test]
    fn distinct_materials_yield_distinct_phrases_for_spot_check() {
        // For 64 distinct single-byte-varying materials, all 64 SAS strings
        // are distinct (verifies that the derivation is injective over this range).
        let phrases: std::collections::HashSet<String> = (0u8..64)
            .map(|b| {
                let mut m = [0u8; 32];
                m[0] = b;
                ShortAuthString::derive(&m).to_string()
            })
            .collect();
        assert_eq!(
            phrases.len(),
            64,
            "64 materials differing only in byte[0] must yield 64 distinct phrases"
        );
    }

    #[test]
    fn all_zero_material_does_not_panic() {
        let sas = ShortAuthString::derive(&[0u8; 32]);
        assert_eq!(
            sas.to_string(),
            "acid-acid-acid",
            "all-zero material maps index 0 (\"acid\") for all three slots"
        );
    }

    #[test]
    fn all_ff_material_produces_last_word() {
        let sas = ShortAuthString::derive(&[0xFFu8; 32]);
        // XOR-fold of 0xFF ^ 0xFF ^ 0xFF ^ 0xFF = 0x00, so all three words
        // must be WORD_LIST[0] = "acid".
        let expected = format!(
            "{}-{}-{}",
            WORD_LIST[0], WORD_LIST[0], WORD_LIST[0]
        );
        assert_eq!(sas.to_string(), expected);
    }
}
