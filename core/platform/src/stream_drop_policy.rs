//! Governor invariant: every stream is droppable or layerable — Feature 77.
//!
//! Every media stream the governor manages must satisfy at least one of:
//!
//! * **Droppable** — the stream can be paused (rate → 0) and resumed without
//!   any decoder resync packet.  The receiver's decoder context is intact when
//!   the stream restarts.
//!
//! * **Layered** — the stream uses temporal layering (T0/T1/T2 …) so
//!   enhancement layers can be withheld under congestion while the T0 base
//!   layer continues to arrive at a lower-but-decodable rate.
//!
//! Consequence: **no tier transition or gear switch ever requires a keyframe
//! burst.**  An IDR under congestion is counter-productive — it is 5–10× the
//! average inter-frame size and arrives exactly when the link budget is tightest.
//!
//! # How each stream satisfies the invariant
//!
//! | Stream              | Mechanism                          | Policy                   |
//! |---------------------|------------------------------------|--------------------------|
//! | Audio               | Opus DTX (silence ≈ 0 B/frame)    | `Droppable`              |
//! | Input / cursor      | Self-contained delta packets       | `Droppable`              |
//! | Screen coarse       | Per-tile, each tile is independent | `Droppable`              |
//! | Screen refinement   | Work-queue suspend                 | `Droppable`              |
//! | Camera Gear A       | Stop sending latent frames         | `Droppable`              |
//! | Camera Gear B       | L1T2 temporal SVC T-layer drops    | `Layered { base: 0.5 }` |
//! | Camera Gear C       | Pause; intra-refresh resumes sweep | `Droppable`              |
//! | Video sub-stream    | Pause Gear-B AV1 encode            | `Droppable`              |
//! | File transfer       | Governor headroom freeze           | `Droppable`              |

// ── StreamKind ────────────────────────────────────────────────────────────────

/// A media stream managed by the governor's 10 Hz control loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    /// Opus voice with DTX, LBRR, and DRED redundancy.
    Audio,
    /// Keyboard and pointer event channel (reliable-ordered, highest priority).
    Input,
    /// Screen coarse-pass tile updates (TEXT / FLAT / PICTURE first pass).
    ScreenCoarse,
    /// Screen build-to-lossless refinement queue (PICTURE second pass).
    ScreenRefinement,
    /// Neural talking-head Gear A camera codec (latent-vector frames).
    CameraGearA,
    /// SVT-AV1 Gear B camera codec with L1T2 temporal SVC layering.
    CameraGearB,
    /// OpenH264 Gear C legacy fallback (no temporal layering).
    CameraGearC,
    /// VIDEO-tile Gear-B AV1 sub-stream isolated from the tile pipeline.
    VideoSubStream,
    /// Bulk file transfer on governor-granted headroom.
    FileTransfer,
}

// ── DropPolicy ────────────────────────────────────────────────────────────────

/// The mechanism by which a stream absorbs congestion without a keyframe burst.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DropPolicy {
    /// The stream can be fully paused and resumed without any resync packet.
    ///
    /// On resumption the receiver's decoder picks up from its current state —
    /// no IDR or equivalent is emitted.  Examples: audio DTX, screen tiles,
    /// input events, file transfer.
    Droppable,

    /// The stream uses temporal layering with a permanently-forwarded base layer.
    ///
    /// Enhancement layers (T1, T2, …) can be withheld under congestion while
    /// the T0 base layer continues to arrive.  The decoder produces output
    /// from T0 frames alone; no resync is needed when enhancement layers are
    /// dropped or later restored.
    ///
    /// `base_fraction` is the fraction of encoded frames that are T0 and must
    /// always be forwarded.  For L1T2 this is `0.5`; for L1T3 it would be
    /// `0.25`.
    Layered {
        /// Fraction of encoded frames in the always-forwarded T0 base layer.
        base_fraction: f32,
    },
}

impl DropPolicy {
    /// `true` when the stream can be fully paused without decoder resync.
    #[inline]
    pub fn is_droppable(self) -> bool {
        matches!(self, Self::Droppable)
    }

    /// `true` when the stream uses temporal enhancement-layer shedding.
    #[inline]
    pub fn is_layered(self) -> bool {
        matches!(self, Self::Layered { .. })
    }

    /// Fraction of frames that are always forwarded.
    ///
    /// `0.0` for `Droppable` (stream can be fully silenced); the T0 fraction
    /// for `Layered` streams.
    #[inline]
    pub fn base_fraction(self) -> f32 {
        match self {
            Self::Droppable => 0.0,
            Self::Layered { base_fraction } => base_fraction,
        }
    }

    /// The invariant core: no tier transition requires a keyframe.
    ///
    /// Both `Droppable` and `Layered` policies absorb congestion without
    /// emitting a keyframe burst.  This method always returns `false`.
    #[inline]
    pub fn needs_keyframe_on_transition(self) -> bool {
        false
    }
}

// ── StreamDropPolicy ──────────────────────────────────────────────────────────

/// Static drop-policy table for all governor-managed streams.
///
/// The governor calls [`StreamDropPolicy::for_kind`] to retrieve the
/// [`DropPolicy`] for a stream, and calls
/// [`StreamDropPolicy::all_streams_keyframe_free`] as a startup assertion.
#[derive(Debug, Clone, Copy)]
pub struct StreamDropPolicy;

impl StreamDropPolicy {
    /// Return the [`DropPolicy`] for `kind`.
    ///
    /// Every entry satisfies `policy.needs_keyframe_on_transition() == false`.
    pub fn for_kind(kind: StreamKind) -> DropPolicy {
        match kind {
            // Opus with DTX: silence encodes to < 1 B/frame.  Resume from
            // the next active packet; no resync.
            StreamKind::Audio => DropPolicy::Droppable,

            // Delta-coded events are self-contained; pausing leaves the
            // remote input state frozen, not broken.
            StreamKind::Input => DropPolicy::Droppable,

            // Each tile is an independent encode unit.  Pausing means no
            // updates reach the remote; its framebuffer stays at last state.
            StreamKind::ScreenCoarse => DropPolicy::Droppable,

            // The refinement work-list is suspended; the remote stays at
            // coarse-pass quality until the queue drains later.
            StreamKind::ScreenRefinement => DropPolicy::Droppable,

            // Gear A latent frames can stop at any time; the synthesis
            // network freezes on the last reconstructed head pose.
            StreamKind::CameraGearA => DropPolicy::Droppable,

            // Gear B uses L1T2 temporal SVC.  T1 enhancement frames (50 %)
            // are withheld under congestion; T0 base frames (50 %) continue.
            // [`TemporalSvcController`] drives the drop level; no IDR is forced.
            StreamKind::CameraGearB => DropPolicy::Layered { base_fraction: 0.5 },

            // Gear C (OpenH264 baseline) has no temporal layering.  Under
            // congestion it is paused entirely; the intra-refresh column sweep
            // in [`IntraRefreshState`] continues from the current column on
            // resumption without an additional IDR.
            StreamKind::CameraGearC => DropPolicy::Droppable,

            // The VIDEO tile sub-stream Gear-B encode can be paused when no
            // VIDEO tiles are dirty.  The remote framebuffer is frozen at the
            // last complete frame; no resync packet is sent.
            StreamKind::VideoSubStream => DropPolicy::Droppable,

            // RaptorQ delivers file data reliably on governor headroom.  A
            // headroom freeze pauses transmission without losing progress;
            // repair symbols continue from where they stopped on resumption.
            StreamKind::FileTransfer => DropPolicy::Droppable,
        }
    }

    /// Verify that every known stream satisfies the keyframe-free invariant.
    ///
    /// Returns `true` iff no stream requires a keyframe on tier transition.
    ///
    /// Intended as a `debug_assert!(StreamDropPolicy::all_streams_keyframe_free())`
    /// in the governor startup path.
    pub fn all_streams_keyframe_free() -> bool {
        use StreamKind::*;
        [
            Audio,
            Input,
            ScreenCoarse,
            ScreenRefinement,
            CameraGearA,
            CameraGearB,
            CameraGearC,
            VideoSubStream,
            FileTransfer,
        ]
        .iter()
        .all(|&kind| !Self::for_kind(kind).needs_keyframe_on_transition())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── DropPolicy invariant ──────────────────────────────────────────────────

    #[test]
    fn droppable_never_needs_keyframe() {
        assert!(!DropPolicy::Droppable.needs_keyframe_on_transition());
    }

    #[test]
    fn layered_never_needs_keyframe() {
        assert!(!DropPolicy::Layered { base_fraction: 0.5 }.needs_keyframe_on_transition());
    }

    #[test]
    fn droppable_base_fraction_is_zero() {
        assert_eq!(DropPolicy::Droppable.base_fraction(), 0.0);
    }

    #[test]
    fn layered_base_fraction_matches_constructor() {
        let policy = DropPolicy::Layered { base_fraction: 0.25 };
        assert!((policy.base_fraction() - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn is_droppable_and_is_layered_are_exclusive() {
        assert!(DropPolicy::Droppable.is_droppable());
        assert!(!DropPolicy::Droppable.is_layered());
        let l = DropPolicy::Layered { base_fraction: 0.5 };
        assert!(!l.is_droppable());
        assert!(l.is_layered());
    }

    // ── StreamDropPolicy table ────────────────────────────────────────────────

    #[test]
    fn every_stream_is_keyframe_free() {
        assert!(
            StreamDropPolicy::all_streams_keyframe_free(),
            "all_streams_keyframe_free must hold: every stream must satisfy the invariant"
        );
    }

    #[test]
    fn camera_gear_b_is_layered_with_half_base_fraction() {
        let policy = StreamDropPolicy::for_kind(StreamKind::CameraGearB);
        assert!(
            policy.is_layered(),
            "Gear B must be Layered (L1T2 temporal SVC); got {policy:?}"
        );
        assert!(
            (policy.base_fraction() - 0.5).abs() < f32::EPSILON,
            "Gear B L1T2 base fraction must be 0.5 (T0 = every other frame)"
        );
    }

    #[test]
    fn audio_is_droppable() {
        assert!(StreamDropPolicy::for_kind(StreamKind::Audio).is_droppable());
    }

    #[test]
    fn input_is_droppable() {
        assert!(StreamDropPolicy::for_kind(StreamKind::Input).is_droppable());
    }

    #[test]
    fn screen_coarse_is_droppable() {
        assert!(StreamDropPolicy::for_kind(StreamKind::ScreenCoarse).is_droppable());
    }

    #[test]
    fn screen_refinement_is_droppable() {
        assert!(StreamDropPolicy::for_kind(StreamKind::ScreenRefinement).is_droppable());
    }

    #[test]
    fn camera_gear_a_is_droppable() {
        assert!(StreamDropPolicy::for_kind(StreamKind::CameraGearA).is_droppable());
    }

    #[test]
    fn camera_gear_c_is_droppable() {
        assert!(StreamDropPolicy::for_kind(StreamKind::CameraGearC).is_droppable());
    }

    #[test]
    fn video_sub_stream_is_droppable() {
        assert!(StreamDropPolicy::for_kind(StreamKind::VideoSubStream).is_droppable());
    }

    #[test]
    fn file_transfer_is_droppable() {
        assert!(StreamDropPolicy::for_kind(StreamKind::FileTransfer).is_droppable());
    }

    // ── No keyframe for each individual stream ────────────────────────────────

    #[test]
    fn no_stream_requires_keyframe_on_transition() {
        use StreamKind::*;
        for kind in [
            Audio, Input, ScreenCoarse, ScreenRefinement,
            CameraGearA, CameraGearB, CameraGearC, VideoSubStream, FileTransfer,
        ] {
            let policy = StreamDropPolicy::for_kind(kind);
            assert!(
                !policy.needs_keyframe_on_transition(),
                "{kind:?}: needs_keyframe_on_transition must be false"
            );
        }
    }
}
