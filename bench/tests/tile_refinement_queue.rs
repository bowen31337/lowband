//! Feature 98 — tile lossless refinement on idle bandwidth with priority_queue saliency.
//!
//! # Purpose
//!
//! Verifies that the refinement pass uses only idle `screen_refinement_bps`
//! budget (the leftover after audio, input, screen-coarse, and camera are
//! funded) and that its [`RefinementQueue`] drains tiles in decreasing saliency
//! order (TEXT first, then FLAT, then PICTURE) with FIFO tiebreaking inside
//! each saliency tier.
//!
//! # Scenario A — constrained tier (64 kbps)
//!
//! At 64 kbps the strict-priority allocation exhausts all bandwidth before
//! reaching the refinement slot:
//!
//! | Stream         | Budget  | Remaining after |
//! |----------------|---------|-----------------|
//! | Voice          | 24 kbps | 40 kbps         |
//! | Input / cursor |  8 kbps | 32 kbps         |
//! | Screen coarse  | 20 kbps | 12 kbps         |
//! | Camera (GearA) | 12 kbps |  0 kbps         |
//! | Screen refine  |  0 kbps |  0 kbps         |
//!
//! Camera takes the entire remainder — the link is fully subscribed and there
//! is no idle bandwidth for refinement.  This is correct strict-priority
//! behaviour; the coarse lane still delivers text-lossless frames.
//!
//! # Scenario B — comfortable tier (400 kbps)
//!
//! At 400 kbps the refinement allocation is 48 kbps.  All 12 PICTURE tiles
//! from a typical damage event drain within 800 ms (< 1 000 ms deadline).
//!
//! # Assertions
//!
//! 1. At 64 kbps, `screen_refinement_bps` is zero — camera consumes all
//!    remaining bandwidth after the coarse lane; no idle slice exists.
//! 1b. Once the link exceeds the camera's 300 kbps cap (e.g. 400 kbps),
//!    `screen_refinement_bps` is positive (idle bandwidth exists).
//! 2. At `Serious` or `Critical` thermal pressure, `screen_refinement_bps`
//!    is zero — refinement is fully suspended, not merely slowed.
//! 3. A mixed queue (TEXT, FLAT, PICTURE tiles inserted in arbitrary order)
//!    drains in strict saliency order: TEXT (3) > FLAT (2) > PICTURE (1).
//! 4. Within the same saliency tier, tiles drain in FIFO order.
//! 5. Throughput model: the number of tiles refined per second at the
//!    comfortable-tier idle budget is consistent with the 400-byte lossless
//!    tile estimate and the 48 kbps allocated bitrate.
//! 6. All tile classes that do not need refinement (Text, Flat, Video) are
//!    not in the queue after a coarse pass — the refinement path never wastes
//!    idle bandwidth on already-lossless tiles.
//! 7. The priority_queue preserves saliency ordering across repeated push/pop
//!    interleaving (streaming damage events, not a one-shot batch).

use lowband_platform::gear_policy::{allocate, GearConstraints};
use lowband_platform::screen_encoder::{
    RefinementQueue, TileClass, TileCoord, LOSSLESS_BYTES_PER_PICTURE_TILE,
};
use lowband_platform::thermal::ThermalPressure;

// ── 1. Idle bandwidth: zero at 64 kbps, positive once camera is capped ───────

#[test]
fn refinement_bps_zero_at_64kbps_camera_takes_all_remaining() {
    // At 64 kbps with camera allowed (Nominal), strict-priority allocation:
    //   audio(24k) + input(8k) + coarse(20k) + camera(12k) = 64k — fully subscribed.
    // No idle bandwidth remains; refinement is correctly zero.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(64_000, &constraints);

    assert_eq!(
        budgets.screen_refinement_bps, 0,
        "at 64 kbps the camera consumes all remaining bandwidth after the coarse lane; \
         screen_refinement_bps must be 0 — no idle bandwidth exists at this link rate"
    );
}

#[test]
fn refinement_bps_positive_when_camera_reaches_its_cap() {
    // Refinement gets idle budget only after camera hits its 300 kbps ceiling.
    // Minimum link where refinement > 0:
    //   audio(24k) + input(8k) + coarse(20k) + camera_cap(300k) + 1 = 353 kbps.
    // At 400 kbps: remaining after camera = 400k−352k = 48k → refinement = 48k.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(400_000, &constraints);

    assert!(
        budgets.screen_refinement_bps > 0,
        "screen_refinement_bps must be > 0 at 400 kbps; \
         camera is capped at 300 kbps, leaving idle bandwidth for refinement — \
         got screen_refinement_bps={}",
        budgets.screen_refinement_bps
    );

    // Expected: 400k − 24k − 8k − 20k − 300k = 48k (capped at 50k → 48k)
    assert_eq!(
        budgets.screen_refinement_bps, 48_000,
        "at 400 kbps the idle refinement budget must be exactly 48 kbps; \
         strict-priority: audio(24k)+input(8k)+coarse(20k)+camera(300k) = 352k, \
         leaving 48k for refinement (below the 50k cap)"
    );
}

// ── 2. Refinement suspended under thermal pressure ───────────────────────────

#[test]
fn refinement_suspended_at_serious_thermal() {
    let constraints = GearConstraints::from_thermal(ThermalPressure::Serious);
    let budgets = allocate(400_000, &constraints);

    assert_eq!(
        budgets.screen_refinement_bps, 0,
        "screen_refinement_bps must be zero at Serious thermal (refinement suspended); \
         coarse lane still runs but the lossless rebuild is deferred until CPU cools"
    );
}

#[test]
fn refinement_suspended_at_critical_thermal() {
    let constraints = GearConstraints::from_thermal(ThermalPressure::Critical);
    let budgets = allocate(400_000, &constraints);

    assert_eq!(
        budgets.screen_refinement_bps, 0,
        "screen_refinement_bps must be zero at Critical thermal (camera off, refine off)"
    );
}

// ── 3. Mixed-saliency queue drains in TEXT > FLAT > PICTURE order ────────────

#[test]
fn mixed_saliency_queue_drains_text_flat_picture_in_order() {
    let mut q = RefinementQueue::new();

    // Insert in an order that would violate saliency if the heap is wrong.
    let picture = TileCoord { col: 0, row: 0 };
    let flat    = TileCoord { col: 1, row: 0 };
    let text    = TileCoord { col: 2, row: 0 };

    q.push(picture, TileClass::Picture); // saliency 1 — inserted first
    q.push(flat,    TileClass::Flat);    // saliency 2
    q.push(text,    TileClass::Text);    // saliency 3 — inserted last

    let (c0, cls0) = q.pop().expect("entry 1");
    let (c1, cls1) = q.pop().expect("entry 2");
    let (c2, cls2) = q.pop().expect("entry 3");

    assert_eq!(cls0, TileClass::Text,    "TEXT (saliency=3) must come first");
    assert_eq!(c0,   text);
    assert_eq!(cls1, TileClass::Flat,    "FLAT (saliency=2) must come second");
    assert_eq!(c1,   flat);
    assert_eq!(cls2, TileClass::Picture, "PICTURE (saliency=1) must come last");
    assert_eq!(c2,   picture);
    assert!(q.is_empty());
}

// ── 4. FIFO order within same saliency tier ──────────────────────────────────

#[test]
fn fifo_order_within_flat_saliency_tier() {
    let mut q = RefinementQueue::new();
    let coords = [
        TileCoord { col: 0, row: 0 },
        TileCoord { col: 1, row: 0 },
        TileCoord { col: 2, row: 0 },
        TileCoord { col: 3, row: 0 },
    ];
    for &c in &coords {
        q.push(c, TileClass::Flat);
    }
    for &expected in &coords {
        let (got, _) = q.pop().expect("queue non-empty");
        assert_eq!(
            got, expected,
            "FLAT tiles must drain FIFO: expected {expected:?}, got {got:?}"
        );
    }
    assert!(q.is_empty());
}

#[test]
fn fifo_order_within_picture_saliency_tier() {
    let mut q = RefinementQueue::new();
    let coords: Vec<TileCoord> = (0..8)
        .map(|i| TileCoord { col: i, row: 0 })
        .collect();
    for &c in &coords {
        q.push(c, TileClass::Picture);
    }
    for &expected in &coords {
        let (got, _) = q.pop().expect("queue non-empty");
        assert_eq!(
            got, expected,
            "PICTURE tiles must drain FIFO: expected {expected:?}, got {got:?}"
        );
    }
}

// ── 5. Throughput model at comfortable-tier idle budget ──────────────────────

#[test]
fn tiles_refined_per_second_at_comfortable_tier_is_consistent() {
    // At 400 kbps the idle refinement budget is 48 kbps.
    // Each lossless PICTURE tile costs 400 B × 8 = 3 200 bits.
    // Tiles per second: 48 000 / 3 200 = 15.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(400_000, &constraints);
    let refine_bps = budgets.screen_refinement_bps as u64;

    assert!(refine_bps > 0, "precondition: refinement budget must be positive at 400 kbps");

    let bits_per_tile = LOSSLESS_BYTES_PER_PICTURE_TILE * 8;
    let tiles_per_sec = refine_bps / bits_per_tile;

    eprintln!(
        "comfortable_throughput  refine_bps={refine_bps}  \
         bits_per_tile={bits_per_tile}  tiles_per_sec={tiles_per_sec}"
    );

    // At 48 kbps: 48 000 / (400 × 8) = 15 tiles per second.
    assert!(
        tiles_per_sec >= 15,
        "at comfortable-tier idle budget ({refine_bps} bps) the system must \
         refine ≥ 15 PICTURE tiles per second; got {tiles_per_sec}"
    );
}

// ── 6. Already-lossless classes are not enqueued ─────────────────────────────

#[test]
fn lossless_classes_never_enter_refinement_queue() {
    let mut q = RefinementQueue::new();

    // Only PICTURE tiles require refinement and would be pushed.
    for class in [TileClass::Text, TileClass::Flat, TileClass::Video] {
        assert!(
            !class.needs_refinement(),
            "{class:?} must not need refinement — it is already lossless \
             or uses a separate sub-stream"
        );
    }

    // Simulating a coarse pass: only PICTURE tiles enter the queue.
    let picture_tile = TileCoord { col: 5, row: 3 };
    q.push(picture_tile, TileClass::Picture);

    assert_eq!(q.len(), 1, "only the PICTURE tile should be in the queue");
    let (coord, class) = q.pop().unwrap();
    assert_eq!(coord, picture_tile);
    assert_eq!(class, TileClass::Picture);
    assert!(q.is_empty());
}

// ── 7. Saliency order preserved under interleaved push/pop ───────────────────

#[test]
fn saliency_order_preserved_under_streaming_damage_events() {
    // Simulates two successive damage events interleaved with partial drains,
    // mimicking real-time refinement while new damage arrives.

    let mut q = RefinementQueue::new();

    // First damage event: two PICTURE tiles arrive.
    let p0 = TileCoord { col: 0, row: 0 };
    let p1 = TileCoord { col: 1, row: 0 };
    q.push(p0, TileClass::Picture);
    q.push(p1, TileClass::Picture);

    // Partial drain: consume one tile (p0 must come out first, FIFO).
    let (drained, cls) = q.pop().unwrap();
    assert_eq!(cls, TileClass::Picture);
    assert_eq!(drained, p0, "FIFO: first PICTURE tile must drain before the second");

    // Second damage event arrives while queue is not empty: a TEXT tile.
    let t0 = TileCoord { col: 2, row: 0 };
    q.push(t0, TileClass::Text); // saliency 3 — should jump ahead of p1

    // Next pop must yield the TEXT tile (higher saliency than remaining PICTURE).
    let (drained, cls) = q.pop().unwrap();
    assert_eq!(cls, TileClass::Text, "TEXT must preempt remaining PICTURE in the queue");
    assert_eq!(drained, t0);

    // Final pop drains the remaining PICTURE tile.
    let (drained, cls) = q.pop().unwrap();
    assert_eq!(cls, TileClass::Picture);
    assert_eq!(drained, p1);

    assert!(q.is_empty(), "all tiles must be consumed");
}
