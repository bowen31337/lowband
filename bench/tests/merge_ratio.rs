//! Feature 88 — System merges damage rectangles with merge_ratio under 1.3.
//!
//! # What this test suite verifies
//!
//! 1. **Identical rects merge**: two identical rects collapse into one.
//! 2. **Ratio strictly below 1.3 → merged**: rects whose bounding-box area is
//!    12.95 % larger than their combined area are merged.
//! 3. **Ratio exactly 1.3 → not merged**: the threshold is strict (< 1.3, not ≤).
//! 4. **Ratio above 1.3 → not merged**: distant rects are left unchanged.
//! 5. **Zero-area rects are discarded** before merging.
//! 6. **Empty input → empty output**.
//! 7. **Single rect → returned unchanged** (no merge possible).
//! 8. **Chain merging**: A merges with B; the result merges with C in the
//!    same pass loop.
//! 9. **Touching rects merge**: adjacent (gap = 0) rects are always merged
//!    because the bounding box equals the sum (ratio = 1.0 < 1.3).
//! 10. **One rect contained in another**: always merged (ratio < 1.0).
//! 11. **Overlapping rects**: merged when the bounding box is small enough.
//! 12. **DAMAGE_MERGE_RATIO constant is 1.3**.

use lowband_platform::screen_capture::DirtyRect;
use lowband_platform::screen_encoder::{merge_damage_rects, DAMAGE_MERGE_RATIO};

// ── Helper ────────────────────────────────────────────────────────────────────

fn rect(x: i32, y: i32, w: u32, h: u32) -> DirtyRect {
    DirtyRect { x, y, width: w, height: h }
}

fn bbox(rects: &[DirtyRect]) -> DirtyRect {
    let x0 = rects.iter().map(|r| r.x).min().unwrap();
    let y0 = rects.iter().map(|r| r.y).min().unwrap();
    let x1 = rects.iter().map(|r| r.x + r.width  as i32).max().unwrap();
    let y1 = rects.iter().map(|r| r.y + r.height as i32).max().unwrap();
    rect(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
}

// ── 12. Constant value ────────────────────────────────────────────────────────

#[test]
fn damage_merge_ratio_constant_is_1_3() {
    assert!(
        (DAMAGE_MERGE_RATIO - 1.3_f32).abs() < 1e-6,
        "DAMAGE_MERGE_RATIO must be 1.3, got {DAMAGE_MERGE_RATIO}"
    );
}

// ── 6. Empty input ────────────────────────────────────────────────────────────

#[test]
fn empty_input_returns_empty_output() {
    let result = merge_damage_rects(vec![]);
    assert!(result.is_empty(), "empty input must produce empty output");
}

// ── 7. Single rect ────────────────────────────────────────────────────────────

#[test]
fn single_rect_returned_unchanged() {
    let r = rect(10, 20, 100, 50);
    let result = merge_damage_rects(vec![r]);
    assert_eq!(result.len(), 1, "single rect must be returned as-is");
    assert_eq!(result[0], r);
}

// ── 5. Zero-area rects discarded ──────────────────────────────────────────────

#[test]
fn zero_width_rect_is_discarded() {
    let result = merge_damage_rects(vec![rect(0, 0, 0, 100)]);
    assert!(result.is_empty(), "zero-width rect must be discarded");
}

#[test]
fn zero_height_rect_is_discarded() {
    let result = merge_damage_rects(vec![rect(0, 0, 100, 0)]);
    assert!(result.is_empty(), "zero-height rect must be discarded");
}

#[test]
fn zero_area_rects_discarded_before_merge() {
    // One real rect and one zero-area rect.  Only the real rect survives.
    let result = merge_damage_rects(vec![rect(0, 0, 100, 100), rect(0, 0, 0, 50)]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], rect(0, 0, 100, 100));
}

// ── 1. Identical rects merge ──────────────────────────────────────────────────

#[test]
fn identical_rects_merge_into_one() {
    // Two identical rects: bbox = area_each → ratio = area / (2 × area) = 0.5 < 1.3.
    let r = rect(100, 200, 50, 30);
    let result = merge_damage_rects(vec![r, r]);
    assert_eq!(result.len(), 1, "two identical rects must merge into one");
    assert_eq!(result[0], r);
}

// ── 10. One rect contained in another ─────────────────────────────────────────

#[test]
fn contained_rect_merges_into_outer() {
    let outer = rect(0, 0, 200, 200);
    let inner = rect(50, 50, 50, 50);
    // bbox = outer; bbox_area = 40000; sum = 40000 + 2500 = 42500; ratio ≈ 0.94 < 1.3.
    let result = merge_damage_rects(vec![inner, outer]);
    assert_eq!(result.len(), 1, "contained rect must merge into the outer rect");
    assert_eq!(result[0], outer);
}

// ── 2. Ratio strictly below 1.3 → merged ─────────────────────────────────────

#[test]
fn ratio_below_threshold_merges() {
    // Two 100×1 rects with a 59 px gap.
    // bbox_area = (100 + 59 + 100) × 1 = 259; sum = 200.
    // ratio = 259/200 = 1.295 < 1.3 → must merge.
    let a = rect(0, 0, 100, 1);
    let b = rect(159, 0, 100, 1);
    let result = merge_damage_rects(vec![a, b]);
    assert_eq!(result.len(), 1, "rects with merge_ratio 1.295 must merge");
    let expected = bbox(&[a, b]);
    assert_eq!(result[0], expected);
}

// ── 3. Ratio exactly 1.3 → not merged ────────────────────────────────────────

#[test]
fn ratio_exactly_1_3_does_not_merge() {
    // Two 100×1 rects with a 60 px gap.
    // bbox_area = 260; sum = 200; ratio = 1.3 exactly → must NOT merge.
    let a = rect(0, 0, 100, 1);
    let b = rect(160, 0, 100, 1);
    let result = merge_damage_rects(vec![a, b]);
    assert_eq!(
        result.len(),
        2,
        "merge_ratio == 1.3 must not trigger a merge (threshold is strict <)"
    );
}

// ── 4. Ratio above 1.3 → not merged ──────────────────────────────────────────

#[test]
fn ratio_above_threshold_does_not_merge() {
    // Two 10×10 rects 1000 px apart.
    // bbox = 1010 × 10 = 10100; sum = 200; ratio = 50.5 >> 1.3.
    let a = rect(0, 0, 10, 10);
    let b = rect(1000, 0, 10, 10);
    let result = merge_damage_rects(vec![a, b]);
    assert_eq!(result.len(), 2, "distant rects must not be merged");
}

// ── 9. Touching (adjacent) rects merge ───────────────────────────────────────

#[test]
fn adjacent_rects_merge() {
    // Two rects that share an edge (gap = 0).
    // bbox = 200 × 50; sum = 100×50 + 100×50 = 10000; ratio = 10000/10000 = 1.0 < 1.3.
    let a = rect(0, 0, 100, 50);
    let b = rect(100, 0, 100, 50);
    let result = merge_damage_rects(vec![a, b]);
    assert_eq!(result.len(), 1, "touching rects must merge");
    assert_eq!(result[0], rect(0, 0, 200, 50));
}

// ── 11. Overlapping rects merge ───────────────────────────────────────────────

#[test]
fn overlapping_rects_merge() {
    // Two 100×100 rects overlapping by 50 px.
    // a=(0,0,100,100), b=(50,0,100,100)
    // bbox=(0,0,150,100), area=15000; sum=10000+10000=20000; ratio=0.75 < 1.3.
    let a = rect(0, 0, 100, 100);
    let b = rect(50, 0, 100, 100);
    let result = merge_damage_rects(vec![a, b]);
    assert_eq!(result.len(), 1, "overlapping rects must merge");
    assert_eq!(result[0], rect(0, 0, 150, 100));
}

// ── 8. Chain merging ──────────────────────────────────────────────────────────

#[test]
fn three_rects_chain_merge() {
    // Three rects each with a 59 px gap between them — each consecutive pair
    // has merge_ratio 1.295, triggering merges across multiple passes.
    //
    // a=(0,0,100,1)   b=(159,0,100,1)   c=(318,0,100,1)
    //
    // Pass 1: a merges with b → ab=(0,0,259,1).
    //   bbox_ab * sum_c: 259/(259+100) = 259/359 ≈ 0.72 < 1.3 → ab merges with c.
    // Final: (0, 0, 418, 1).
    let a = rect(0,   0, 100, 1);
    let b = rect(159, 0, 100, 1);
    let c = rect(318, 0, 100, 1);
    let result = merge_damage_rects(vec![a, b, c]);
    assert_eq!(result.len(), 1, "three chained rects must merge into one");
    assert_eq!(result[0], rect(0, 0, 418, 1));
}

// ── Mixed: some merge, some don't ────────────────────────────────────────────

#[test]
fn mixed_rects_only_close_pairs_merge() {
    // a=(0,0,100,1), b=(159,0,100,1) → merge_ratio=1.295 < 1.3 → merge.
    // c=(10000,0,100,1) → far from ab → does not merge.
    let a = rect(0,     0, 100, 1);
    let b = rect(159,   0, 100, 1);
    let c = rect(10000, 0, 100, 1);
    let mut result = merge_damage_rects(vec![a, b, c]);
    assert_eq!(result.len(), 2, "a+b must merge; c must stay separate");
    result.sort_by_key(|r| r.x);
    assert_eq!(result[0], rect(0, 0, 259, 1), "merged a+b");
    assert_eq!(result[1], c, "distant c unchanged");
}

// ── Merged rect is the bounding box ──────────────────────────────────────────

#[test]
fn merged_rect_equals_bounding_box() {
    // Verify the merged output is exactly the bounding box of the inputs.
    let a = rect(10, 20, 80, 40);
    let b = rect(50, 10, 60, 70);
    // bbox: x0=10, y0=10, x1=max(90,110)=110, y1=max(60,80)=80 → (10,10,100,70)
    // bbox_area = 100*70 = 7000; sum = 80*40 + 60*70 = 3200+4200 = 7400; ratio = 7000/7400 ≈ 0.946 < 1.3
    let result = merge_damage_rects(vec![a, b]);
    assert_eq!(result.len(), 1);
    let expected = bbox(&[a, b]);
    assert_eq!(result[0], expected, "merged rect must be the bounding box");
}

// ── Negative coordinates are handled ─────────────────────────────────────────

#[test]
fn negative_coordinate_rects_merge() {
    // Two small rects in the negative-coordinate quadrant with a small gap.
    // a=(-200,0,100,1), b=(-41,0,100,1)
    // bbox: x0=-200, x1=max(-100,59)=59 → width=259; bbox_area=259; sum=200; ratio=1.295 < 1.3
    let a = rect(-200, 0, 100, 1);
    let b = rect(-41,  0, 100, 1);
    let result = merge_damage_rects(vec![a, b]);
    assert_eq!(result.len(), 1, "negative-coordinate rects must merge when ratio < 1.3");
    assert_eq!(result[0], rect(-200, 0, 259, 1));
}
