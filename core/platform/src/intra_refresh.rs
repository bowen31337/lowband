//! Intra-refresh column-sweep state for Gear B (SVT-AV1) — Feature 126.
//!
//! After the initial keyframe, the encoder performs periodic column sweeps
//! instead of full IDR keyframes.  One column band is intra-coded per frame;
//! over [`IntraRefreshState::columns`] frames the entire image is refreshed
//! without the bitrate spike of a keyframe.
//!
//! # Motivation
//!
//! A full IDR keyframe refreshes every pixel in one frame, which can be 5–10×
//! the average frame size and stalls the pacer token bucket.  Column-sweep
//! intra-refresh spreads the same refresh work across `N` frames at a cost of
//! roughly `1/N` extra bits per frame — invisible to the pacer and to the
//! 64 kbps link budget.
//!
//! # Usage
//!
//! ```
//! use lowband_platform::intra_refresh::{IntraRefreshState, IntraRefreshFrame};
//!
//! // 30 column bands → one-second refresh cycle at 30 fps (Gear B target).
//! let mut ir = IntraRefreshState::new(30);
//!
//! // First frame is always a keyframe (decoder sync).
//! assert_eq!(ir.advance(), IntraRefreshFrame::Keyframe);
//!
//! // Subsequent frames sweep columns 0 → 29 in order.
//! for expected_col in 0..30 {
//!     match ir.advance() {
//!         IntraRefreshFrame::ColumnSweep { col } => assert_eq!(col, expected_col),
//!         IntraRefreshFrame::Keyframe => panic!("no keyframe after stream start"),
//!     }
//! }
//! ```

// ── Public types ──────────────────────────────────────────────────────────────

/// The encoding mode the Gear B encoder should apply to the current frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntraRefreshFrame {
    /// Initial intra frame (IDR) — emitted exactly once at stream start so the
    /// decoder can decode the first frame without prior reference.
    Keyframe,
    /// Intra-refresh column sweep: intra-code column band `col` (0-based index
    /// within the frame's column partition) and inter-code the rest.
    ColumnSweep {
        /// Column band to refresh (0 ≤ col < [`IntraRefreshState::columns`]).
        col: u32,
    },
}

// ── IntraRefreshState ─────────────────────────────────────────────────────────

/// Per-stream intra-refresh sweep state for SVT-AV1 Gear B encoding.
///
/// Tracks which column band to refresh on each frame.  Call [`advance`] once
/// per encoded frame to obtain the [`IntraRefreshFrame`] variant.
///
/// The first call returns [`IntraRefreshFrame::Keyframe`]; every subsequent
/// call returns [`IntraRefreshFrame::ColumnSweep`] with `col` advancing
/// through `0 ..= columns - 1` cyclically — never emitting another keyframe.
///
/// [`advance`]: Self::advance
pub struct IntraRefreshState {
    columns:  u32,
    next_col: u32,
    started:  bool,
}

impl IntraRefreshState {
    /// Create intra-refresh state with `columns` vertical bands per sweep cycle.
    ///
    /// Choose `columns` so the sweep period matches the desired refresh interval:
    ///
    /// | FPS | Target refresh | `columns` |
    /// |-----|----------------|-----------|
    /// | 30  | 1 s            | 30        |
    /// | 30  | 2 s            | 60        |
    /// | 25  | 1 s            | 25        |
    ///
    /// The Gear B target is 30 fps ([`crate::gear_policy::GEAR_B_TARGET_FPS`]),
    /// so `columns = 30` gives a one-second refresh cycle.
    ///
    /// # Panics
    ///
    /// Panics if `columns == 0`.
    pub fn new(columns: u32) -> Self {
        assert!(columns > 0, "intra_refresh: columns must be non-zero");
        Self { columns, next_col: 0, started: false }
    }

    /// Advance by one frame and return the encoding instruction.
    ///
    /// Returns [`IntraRefreshFrame::Keyframe`] on the very first call, then
    /// [`IntraRefreshFrame::ColumnSweep { col }`] for every subsequent call,
    /// cycling `col` through `0 ..= columns - 1`.
    pub fn advance(&mut self) -> IntraRefreshFrame {
        if !self.started {
            self.started = true;
            return IntraRefreshFrame::Keyframe;
        }
        let col = self.next_col;
        self.next_col = (self.next_col + 1) % self.columns;
        IntraRefreshFrame::ColumnSweep { col }
    }

    /// Returns `true` once the initial keyframe has been emitted and
    /// intra-refresh column sweep is active.
    pub fn is_active(&self) -> bool {
        self.started
    }

    /// The number of column bands in one full sweep cycle.
    pub fn columns(&self) -> u32 {
        self.columns
    }

    /// The column band that will be refreshed by the *next* call to [`advance`].
    ///
    /// Returns `0` before the stream has started (before the initial keyframe).
    pub fn next_column(&self) -> u32 {
        self.next_col
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_is_keyframe() {
        let mut ir = IntraRefreshState::new(30);
        assert_eq!(ir.advance(), IntraRefreshFrame::Keyframe);
    }

    #[test]
    fn not_active_before_first_frame() {
        let ir = IntraRefreshState::new(30);
        assert!(!ir.is_active());
    }

    #[test]
    fn active_after_first_frame() {
        let mut ir = IntraRefreshState::new(30);
        ir.advance();
        assert!(ir.is_active());
    }

    #[test]
    fn no_keyframe_after_stream_start() {
        let mut ir = IntraRefreshState::new(30);
        ir.advance(); // keyframe
        for _ in 0..300 {
            assert_ne!(
                ir.advance(),
                IntraRefreshFrame::Keyframe,
                "keyframe must never be emitted after stream start"
            );
        }
    }

    #[test]
    fn columns_advance_in_order_within_one_cycle() {
        let columns = 30u32;
        let mut ir = IntraRefreshState::new(columns);
        ir.advance(); // keyframe
        for expected in 0..columns {
            match ir.advance() {
                IntraRefreshFrame::ColumnSweep { col } => {
                    assert_eq!(col, expected, "column order mismatch at position {expected}");
                }
                IntraRefreshFrame::Keyframe => panic!("unexpected keyframe at column {expected}"),
            }
        }
    }

    #[test]
    fn cycle_wraps_back_to_column_zero() {
        let columns = 10u32;
        let mut ir = IntraRefreshState::new(columns);
        ir.advance(); // keyframe
        // Exhaust one full cycle.
        for _ in 0..columns {
            ir.advance();
        }
        // The next advance must start a fresh cycle at column 0.
        assert_eq!(
            ir.advance(),
            IntraRefreshFrame::ColumnSweep { col: 0 },
            "sweep must wrap to column 0 after a full cycle"
        );
    }

    #[test]
    fn each_column_covered_exactly_once_per_cycle() {
        let columns = 30u32;
        let mut ir = IntraRefreshState::new(columns);
        ir.advance(); // keyframe

        let mut seen = vec![0u32; columns as usize];
        for _ in 0..columns {
            match ir.advance() {
                IntraRefreshFrame::ColumnSweep { col } => seen[col as usize] += 1,
                IntraRefreshFrame::Keyframe => panic!("unexpected keyframe"),
            }
        }
        for (col, &count) in seen.iter().enumerate() {
            assert_eq!(count, 1, "column {col} covered {count} times in one sweep cycle");
        }
    }

    #[test]
    fn multiple_cycles_are_consistent() {
        let columns = 8u32;
        let cycles = 5u32;
        let mut ir = IntraRefreshState::new(columns);
        ir.advance(); // keyframe

        for cycle in 0..cycles {
            for expected_col in 0..columns {
                match ir.advance() {
                    IntraRefreshFrame::ColumnSweep { col } => assert_eq!(
                        col, expected_col,
                        "cycle {cycle}: expected column {expected_col}, got {col}"
                    ),
                    IntraRefreshFrame::Keyframe => {
                        panic!("unexpected keyframe in cycle {cycle}")
                    }
                }
            }
        }
    }

    #[test]
    fn columns_accessor_returns_configured_value() {
        for n in [1u32, 5, 30, 60, 128] {
            assert_eq!(IntraRefreshState::new(n).columns(), n);
        }
    }

    #[test]
    fn next_column_starts_at_zero() {
        assert_eq!(IntraRefreshState::new(30).next_column(), 0);
    }

    #[test]
    fn next_column_advances_after_each_sweep_frame() {
        let mut ir = IntraRefreshState::new(4);
        ir.advance(); // keyframe — next_col stays at 0
        assert_eq!(ir.next_column(), 0);

        ir.advance(); // sweep col 0 → next becomes 1
        assert_eq!(ir.next_column(), 1);

        ir.advance(); // sweep col 1 → next becomes 2
        assert_eq!(ir.next_column(), 2);
    }

    #[test]
    #[should_panic(expected = "columns must be non-zero")]
    fn zero_columns_panics() {
        IntraRefreshState::new(0);
    }
}
