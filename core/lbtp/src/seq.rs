//! Wire-sequence-number expansion — Feature 11.
//!
//! # Purpose
//!
//! The LBTP datagram header transmits only the lower [`WIRE_SEQ_BITS`] (16)
//! bits of the 47-bit packet sequence number to cut per-datagram overhead.
//! The receiver uses [`SeqExpander`] to rebuild the full [`SEQ_BITS`]-bit
//! value required to construct the ChaCha20-Poly1305 AEAD nonce
//! (`direction_bit ‖ seq47`).
//!
//! # Algorithm
//!
//! The expander tracks `next_expected` — the full sequence of the next
//! anticipated in-order packet (highest seen + 1, starting at 0).  On each
//! call to [`SeqExpander::expand`]:
//!
//! 1. Extract `low = next_expected & 0xFFFF`.
//! 2. Compute `Δ = (wire_seq.wrapping_sub(low as u16)) as i16`, which maps
//!    any 16-bit wire value to the nearest full-sequence candidate in the
//!    window `[next_expected − 32 768, next_expected + 32 767]`.
//! 3. `full_seq = clamp(next_expected as i64 + Δ as i64, 0, SEQ_MAX)`.
//! 4. When `full_seq ≥ next_expected`, advance `next_expected = full_seq + 1`.
//!
//! The ±32 768 packet window comfortably covers any realistic reordering
//! depth or burst-loss recovery without misclassifying future packets as
//! delayed duplicates.
//!
//! # Integration
//!
//! ```rust
//! use lowband_lbtp::seq::SeqExpander;
//!
//! let mut exp = SeqExpander::new();
//!
//! // In-order stream: the wire carries only 16 bits; expand returns 47-bit values.
//! assert_eq!(exp.expand(0), 0);
//! assert_eq!(exp.expand(1), 1);
//! assert_eq!(exp.expand(2), 2);
//! ```

/// Bits transmitted on the wire for the sequence number field.
pub const WIRE_SEQ_BITS: u32 = 16;

/// Full sequence-number width in bits.
///
/// The AEAD nonce is `direction_bit ‖ seq47`; this constant fixes the
/// space used by the sequence component of that nonce.
pub const SEQ_BITS: u32 = 47;

/// Largest valid full sequence number (2 ^ [`SEQ_BITS`] − 1).
pub const SEQ_MAX: u64 = (1u64 << SEQ_BITS) - 1;

const WIRE_MASK: u64 = (1u64 << WIRE_SEQ_BITS) - 1; // 0x0000_FFFF

/// Receiver-side expander that maps 16-bit wire sequence numbers to the full
/// 47-bit sequence space.
///
/// One instance per active session (one per direction).  Zero heap allocation.
///
/// ## State
///
/// The expander records `next_expected` — the full sequence number of the
/// next in-order packet.  It advances automatically as packets arrive, and
/// handles reordering within a ±32 768 packet window without external
/// coordination.
///
/// ## Example
///
/// ```rust
/// use lowband_lbtp::seq::SeqExpander;
///
/// let mut exp = SeqExpander::new();
///
/// // 65 536 in-order packets: the 16-bit field wraps, expansion is seamless.
/// for i in 0u64..65_536 {
///     assert_eq!(exp.expand(i as u16), i);
/// }
/// // Wire wraps to 0; full sequence advances to 65 536.
/// assert_eq!(exp.expand(0), 65_536);
/// ```
#[derive(Debug, Clone)]
pub struct SeqExpander {
    /// Full sequence number of the next expected in-order packet.
    /// Starts at 0; advances each time a packet with `full_seq ≥ next_expected`
    /// is received.
    next_expected: u64,
}

impl Default for SeqExpander {
    fn default() -> Self {
        Self::new()
    }
}

impl SeqExpander {
    /// Create a new expander at the start of a session (next expected = 0).
    pub fn new() -> Self {
        Self { next_expected: 0 }
    }

    /// Expand a 16-bit wire sequence number to the full 47-bit sequence.
    ///
    /// Uses `next_expected` to resolve the ambiguity in the truncated wire
    /// value.  The returned value is the 47-bit sequence number closest to
    /// `next_expected` whose lower 16 bits equal `wire_seq`.
    ///
    /// Advances the internal state when the returned sequence number is ≥
    /// `next_expected` (i.e., in-order or out-of-order-ahead packets).
    ///
    /// # Out-of-order handling
    ///
    /// Late packets (full_seq < next_expected) are correctly expanded without
    /// advancing state, provided the delay is within 32 768 packets of the
    /// current `next_expected`.
    pub fn expand(&mut self, wire_seq: u16) -> u64 {
        let low = (self.next_expected & WIRE_MASK) as u16;

        // Signed 16-bit delta: maps wire_seq to the nearest value around next_expected.
        let delta = wire_seq.wrapping_sub(low) as i16;

        let full = (self.next_expected as i64 + delta as i64).clamp(0, SEQ_MAX as i64) as u64;

        if full >= self.next_expected {
            self.next_expected = full + 1;
        }

        full
    }

    /// The next expected full sequence number (highest received + 1).
    pub fn next_expected(&self) -> u64 {
        self.next_expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ──────────────────────────────────────────────────────

    #[test]
    fn new_starts_at_zero() {
        let exp = SeqExpander::new();
        assert_eq!(exp.next_expected(), 0);
    }

    #[test]
    fn default_equals_new() {
        let a = SeqExpander::new();
        let b = SeqExpander::default();
        assert_eq!(a.next_expected(), b.next_expected());
    }

    // ── In-order delivery ─────────────────────────────────────────────────

    #[test]
    fn expand_first_packet_zero() {
        let mut exp = SeqExpander::new();
        assert_eq!(exp.expand(0), 0);
    }

    #[test]
    fn expand_sequential_packets() {
        let mut exp = SeqExpander::new();
        for i in 0u64..16 {
            assert_eq!(exp.expand(i as u16), i, "seq {i}");
        }
    }

    #[test]
    fn next_expected_advances_in_order() {
        let mut exp = SeqExpander::new();
        exp.expand(0); // full = 0 → next_expected = 1
        assert_eq!(exp.next_expected(), 1);
        exp.expand(1); // full = 1 → next_expected = 2
        assert_eq!(exp.next_expected(), 2);
    }

    // ── 16-bit wire rollover ──────────────────────────────────────────────

    #[test]
    fn expansion_survives_wire_rollover() {
        let mut exp = SeqExpander::new();
        for i in 0u64..65_536 {
            assert_eq!(exp.expand(i as u16), i);
        }
        // wire wraps to 0; full sequence must be 65_536, not 0
        assert_eq!(exp.expand(0), 65_536);
        assert_eq!(exp.expand(1), 65_537);
    }

    #[test]
    fn expansion_survives_multiple_wire_rollovers() {
        let mut exp = SeqExpander::new();
        let n = 200_000u64;
        for i in 0..n {
            assert_eq!(exp.expand(i as u16), i, "seq {i}");
        }
    }

    // ── Reordering (late packets) ─────────────────────────────────────────

    #[test]
    fn late_packet_one_behind() {
        let mut exp = SeqExpander::new();
        exp.expand(0); // advance next_expected to 1
        exp.expand(1); // advance next_expected to 2

        // Receive seq 0 again (late / reordered)
        let full = exp.expand(0);
        assert_eq!(full, 0, "seq 0 redelivered must still expand to 0");
        assert_eq!(exp.next_expected(), 2, "late packet must not regress next_expected");
    }

    #[test]
    fn late_packet_within_window() {
        let mut exp = SeqExpander::new();
        // Advance to seq 100
        for i in 0u16..100 {
            exp.expand(i);
        }
        assert_eq!(exp.next_expected(), 100);

        // Late arrival: seq 50 (50 behind the current next_expected)
        assert_eq!(exp.expand(50), 50);
        assert_eq!(exp.next_expected(), 100, "late packet must not change next_expected");
    }

    #[test]
    fn late_packet_at_max_window_boundary() {
        let mut exp = SeqExpander::new();
        // Advance to 32_768
        for i in 0u64..32_768 {
            exp.expand(i as u16);
        }
        assert_eq!(exp.next_expected(), 32_768);

        // Wire seq 0 is exactly 32_768 steps behind.
        // i16::MIN (-32_768) signed delta → full = 0
        assert_eq!(exp.expand(0), 0);
    }

    // ── Out-of-order ahead (gap then fill) ───────────────────────────────

    #[test]
    fn packet_one_ahead() {
        let mut exp = SeqExpander::new();
        // Skip seq 0; receive seq 1 first.
        assert_eq!(exp.expand(1), 1);
        assert_eq!(exp.next_expected(), 2);
    }

    #[test]
    fn packet_ahead_then_fill_gap() {
        let mut exp = SeqExpander::new();
        // Receive seq 5 before 0..4
        assert_eq!(exp.expand(5), 5);
        assert_eq!(exp.next_expected(), 6);

        // Fill in the gap: 0..4 expand correctly even though next_expected is 6
        assert_eq!(exp.expand(0), 0);
        assert_eq!(exp.expand(1), 1);
        assert_eq!(exp.expand(4), 4);
        assert_eq!(exp.next_expected(), 6, "gap fills must not regress state");
    }

    // ── Wire rollover with late packets ───────────────────────────────────

    #[test]
    fn late_packet_across_wire_rollover() {
        let mut exp = SeqExpander::new();
        // Drive past the rollover point: advance to seq 65_537
        for i in 0u64..65_537 {
            exp.expand(i as u16);
        }
        assert_eq!(exp.next_expected(), 65_537);

        // A reordered packet from just before the rollover (seq 65_534 = wire 0xFFFE)
        assert_eq!(exp.expand(0xFFFE), 65_534);
        assert_eq!(exp.next_expected(), 65_537, "late packet must not regress state");
    }

    // ── SEQ_MAX boundary ──────────────────────────────────────────────────

    #[test]
    fn expansion_clamped_at_seq_max() {
        let mut exp = SeqExpander::new();
        // Artificially place next_expected at SEQ_MAX
        // Drive there by expanding at SEQ_MAX - 1
        // Use internal knowledge: wire_seq = low16(SEQ_MAX - 1)
        let target = SEQ_MAX - 1;
        let wire = (target & WIRE_MASK) as u16;
        // Manually set next_expected to just below SEQ_MAX by repeatedly advancing.
        // For efficiency, use a second expander driven by one packet at SEQ_MAX - 1.
        // We cannot set internal state directly, so we test the clamp behavior by
        // constructing an expander whose next_expected = SEQ_MAX.
        //
        // Build an expander with next_expected = SEQ_MAX by expanding the preceding seq.
        let mut exp2 = SeqExpander { next_expected: SEQ_MAX };

        // wire_seq for SEQ_MAX itself
        let wire_max = (SEQ_MAX & WIRE_MASK) as u16;
        let full = exp2.expand(wire_max);
        assert_eq!(full, SEQ_MAX);

        // A wire_seq one ahead of SEQ_MAX would be (wire_max + 1) & 0xFFFF,
        // but the full value SEQ_MAX + 1 is clamped to SEQ_MAX.
        let wire_over = wire_max.wrapping_add(1);
        let clamped = exp2.expand(wire_over);
        assert_eq!(clamped, SEQ_MAX, "expansion must not exceed SEQ_MAX");

        // Suppress unused-variable warning for `exp` and `wire`
        let _ = exp.expand(wire);
        let _ = target;
    }

    #[test]
    fn expansion_never_exceeds_seq_max_fuzz() {
        // Drive through several wire rollovers; ensure no value exceeds SEQ_MAX.
        let mut exp = SeqExpander::new();
        for i in 0u64..200_000 {
            let full = exp.expand(i as u16);
            assert!(
                full <= SEQ_MAX,
                "expanded seq {full} at wire {i} exceeds SEQ_MAX"
            );
        }
    }

    // ── Nonce width: verifies SEQ_BITS and SEQ_MAX constants ─────────────

    #[test]
    fn seq_max_is_47_bit_maximum() {
        assert_eq!(SEQ_MAX, 0x7FFF_FFFF_FFFF);
    }

    #[test]
    fn wire_seq_bits_is_16() {
        assert_eq!(WIRE_SEQ_BITS, 16);
    }

    #[test]
    fn seq_bits_is_47() {
        assert_eq!(SEQ_BITS, 47);
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Allow direct state construction in boundary tests.
    impl SeqExpander {
        fn with_next_expected(next_expected: u64) -> Self {
            Self { next_expected }
        }
    }

    #[test]
    fn with_next_expected_helper() {
        let exp = SeqExpander::with_next_expected(1000);
        assert_eq!(exp.next_expected(), 1000);
    }

    #[test]
    fn late_packet_exactly_one_below_next_expected() {
        let mut exp = SeqExpander::with_next_expected(5);
        // wire_seq 4 → delta = 4 - 5 = -1 as i16 = -1 → full = 5 + (-1) = 4
        assert_eq!(exp.expand(4), 4);
        assert_eq!(exp.next_expected(), 5);
    }

    #[test]
    fn in_order_packet_exactly_at_next_expected() {
        let mut exp = SeqExpander::with_next_expected(300);
        // wire_seq 300 & 0xFFFF = 300 → delta = 0 → full = 300
        assert_eq!(exp.expand(300), 300);
        assert_eq!(exp.next_expected(), 301);
    }

    #[test]
    fn wire_rollover_from_boundary() {
        // next_expected is at 65536; low = 0; wire 0 → full = 65536 (not 0)
        let mut exp = SeqExpander::with_next_expected(65_536);
        assert_eq!(exp.expand(0), 65_536);
        assert_eq!(exp.next_expected(), 65_537);
    }

    #[test]
    fn late_packet_just_before_wire_rollover() {
        // next_expected = 65537; low = 1; wire 65535 → delta = -2 → full = 65535
        let mut exp = SeqExpander::with_next_expected(65_537);
        assert_eq!(exp.expand(65_535), 65_535);
        assert_eq!(exp.next_expected(), 65_537);
    }

    #[test]
    fn clone_is_independent() {
        let mut exp = SeqExpander::new();
        exp.expand(0);
        exp.expand(1);

        let mut clone = exp.clone();
        exp.expand(2);
        // Clone should still see next_expected = 2 (before the third expansion)
        assert_eq!(clone.expand(2), 2);
        assert_eq!(exp.next_expected(), 3);
        assert_eq!(clone.next_expected(), 3);
    }
}
