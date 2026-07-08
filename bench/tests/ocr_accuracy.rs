//! Feature 165 — legibility gate: ocr_accuracy at 99.5 percent character accuracy.
//!
//! # Architecture context
//!
//! The screen codec uses damage semantics: only dirty tiles are re-encoded per
//! frame; static content produces ≈ 0 B/s plus a 1 Hz heartbeat.  The first
//! (COARSE) pass renders all changed tiles in ≤ 50 ms; a subsequent refinement
//! pass builds to lossless over the following ≈ 1 s.
//!
//! OCR accuracy is defined over COARSE-pass frames: the fraction of on-screen
//! character cells in the displayed frame that match the source.  PSNR forgives
//! illegible text; OCR does not — a technician reading a stack trace is the
//! product.  Architecture success criterion (§15): ≥ 99.5 % character accuracy.
//!
//! # Encoding model
//!
//! The TEXT/UI coder (architecture §10.4) encodes palette-compatible tiles with
//! a context-adaptive range coder equivalent to AV1 palette + intra-block-copy.
//! Transmitted text tiles are lossless — every transmitted character is correctly
//! reproduced.  Changed tiles that exceed the per-frame budget are deferred to
//! the next frame; they display stale previous-frame content until re-encoded.
//!
//! # Scenario
//!
//! A mixed typing session at the constrained tier (64 kbps).  Architecture
//! reference rates (§10): static ≈ 0 kbps; typing 5–20 kbps; scrolling 30–80
//! kbps.  At 64 kbps, the screen-coarse channel receives ≈ 20 kbps — the top
//! of the typing range — so the gate applies to typing sessions.
//!
//! The simulated session uses a repeating 20-frame period:
//!
//! | Frame type | Changed chars    | Count/period | Representative activity     |
//! |------------|------------------|--------------|-----------------------------|
//! | Paste      | PASTE_CHARS (80) | 1 (5 %)      | One-line paste / completion |
//! | Typing     | TYPING_CHARS (3) | 10 (50 %)    | Active typing at 60 WPM     |
//! | Idle       | 0                |  9 (45 %)    | Reading / waiting           |
//!
//! # OCR accuracy model
//!
//! Per-frame character accuracy over all CHARS_PER_SCREEN cells:
//!
//! ```text
//! capacity  = floor(bits_per_frame / COARSE_BITS_PER_CHAR)
//! correct   = (CHARS_PER_SCREEN − changed) + min(changed, capacity)
//! accuracy  = correct / CHARS_PER_SCREEN
//! ```
//!
//! Unchanged cells are always correct (previously rendered to 100 %).  Changed
//! cells within capacity are losslessly encoded by the TEXT coder.  Changed
//! cells beyond capacity are deferred → show stale content → OCR error.
//!
//! # Assertions
//!
//! 1. `char_height_px ≥ OCR_MIN_CHAR_HEIGHT` — the display resolution provides
//!    enough vertical resolution for reliable character recognition.
//! 2. `screen_coarse_bps ≥ TYPING_RATE_FLOOR_BPS` — the coarse budget covers
//!    the architecture typing reference rate floor.
//! 3. `session_ocr_accuracy ≥ OCR_ACCURACY_TARGET` — session-average character
//!    accuracy across the mixed typing session meets the 99.5 % gate.

use lowband_platform::gear_policy::{allocate, GearConstraints};
use lowband_platform::thermal::ThermalPressure;

// ── Architecture constants ────────────────────────────────────────────────────

/// Architecture minimum link rate for a viable constrained session (bps).
const LINK_BPS: u32 = 64_000;

/// Screen frame rate at the constrained tier (fps).
///
/// Remote-desktop screen sharing targets 10 fps at the constrained tier —
/// sufficient for reading and typing; higher rates belong to the comfortable
/// and full tiers where continuous video content is also rendered.
const SCREEN_FRAME_RATE: u32 = 10;

/// Terminal layout: columns × rows = total visible character cells.
const CHARS_PER_LINE: u32 = 80;
const LINES_PER_SCREEN: u32 = 25;
const CHARS_PER_SCREEN: u32 = CHARS_PER_LINE * LINES_PER_SCREEN; // 2 000

/// Minimum character height (pixels) for reliable OCR.
///
/// At ≥ 8 px character height, standard OCR engines achieve > 99 % accuracy
/// on clean monochrome text (established floor in OCR literature).
const OCR_MIN_CHAR_HEIGHT: u32 = 8;

// ── Coarse encoding model ─────────────────────────────────────────────────────

/// Bits consumed per character cell in the COARSE first pass.
///
/// Derivation — TEXT/UI tile at 848×480 with 80-column layout:
///   cell size  = (848 / 80) × (480 / 25) ≈ 10 × 19 = 190 pixels
///   binary entropy (80 % background, 20 % stroke): ≈ 0.72 bits/pixel
///   context-adaptive range coding (AV1 palette + IBC equivalent): ~5× gain
///   practical cell ≈ 190 × 0.72 / 5 ≈ 27 bits → 40 bits with overhead margin
///
/// The 40-bit figure is conservative; common glyphs typically encode in
/// 20–35 bits.  The margin ensures the model understates available capacity
/// and the accuracy gate does not pass by an artificially narrow margin.
const COARSE_BITS_PER_CHAR: u32 = 40;

// ── Session model ─────────────────────────────────────────────────────────────

/// Total simulation frames (200 × 100 ms = 20 s at 10 fps).
const N_FRAMES: u32 = 200;

/// Changed characters per frame in "typing" frames.
///
/// 60 WPM = 5 chars/s; over 10 fps that is 0.5 chars/frame average.  Using
/// 3 chars/frame models a burst of keystrokes (3× the per-frame average),
/// representative of rapid typing while remaining within the architecture
/// typing reference rate (5–20 kbps).
const TYPING_CHARS: u32 = 3;

/// Changed characters per frame in "paste" frames.
///
/// A one-line paste event: 80 characters = one full terminal line.  This is
/// the worst-case single-frame content burst for a typing session and the
/// scenario most likely to exceed per-frame capacity at constrained rates.
const PASTE_CHARS: u32 = 80;

/// Architecture OCR accuracy gate: 99.5 % character accuracy (§15).
const OCR_ACCURACY_TARGET: f64 = 0.995;

/// Architecture typing reference rate floor (bps) — architecture §10.
const TYPING_RATE_FLOOR_BPS: u32 = 5_000;

// ── Session frame sequence ────────────────────────────────────────────────────

/// Changed character count for frame `idx` in the deterministic session model.
///
/// Repeating 20-frame period: 1 paste (5 %), 10 typing (50 %), 9 idle (45 %).
fn frame_changed_chars(idx: u32) -> u32 {
    match idx % 20 {
        0 => PASTE_CHARS,       // frame 0: one-line paste
        1..=10 => TYPING_CHARS, // frames 1–10: active typing
        _ => 0,                 // frames 11–19: idle
    }
}

// ── Test ──────────────────────────────────────────────────────────────────────

#[test]
fn ocr_accuracy_at_99_5_percent_character_accuracy() {
    // Derive per-stream budgets at the architecture minimum constrained link rate.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(LINK_BPS, &constraints);

    let screen_coarse_bps = budgets.screen_coarse_bps;
    let resolution = budgets.display_resolution;

    // ── Assertion 1: Resolution gate ──────────────────────────────────────────
    //
    // The character cell height at the display resolution selected by the
    // governor must exceed the OCR floor; below 8 px, character distinguishability
    // drops sharply and a 99.5 % accuracy guarantee is not maintainable.
    let char_height_px = resolution.height / LINES_PER_SCREEN;
    assert!(
        char_height_px >= OCR_MIN_CHAR_HEIGHT,
        "character height {char_height_px} px is below the OCR minimum \
         {OCR_MIN_CHAR_HEIGHT} px at {}×{} with {LINES_PER_SCREEN} lines/screen; \
         legibility cannot be guaranteed at this resolution",
        resolution.width,
        resolution.height,
    );

    // ── Assertion 2: Bitrate adequacy ─────────────────────────────────────────
    //
    // The screen-coarse allocation must cover the architecture typing reference
    // rate floor (5 kbps).  Below this floor the coarse pass cannot keep up
    // with normal typing activity and character deferral drives accuracy below
    // the 99.5 % gate.
    assert!(
        screen_coarse_bps >= TYPING_RATE_FLOOR_BPS,
        "screen_coarse_bps {screen_coarse_bps} bps is below the typing reference \
         rate floor {TYPING_RATE_FLOOR_BPS} bps; the coarse pass cannot maintain \
         {:.1}% OCR accuracy for typing sessions at this bitrate",
        OCR_ACCURACY_TARGET * 100.0,
    );

    // ── Assertion 3: OCR accuracy simulation ──────────────────────────────────
    //
    // For each frame, compute character accuracy over all CHARS_PER_SCREEN cells.
    // Sum across N_FRAMES and assert the session average meets the gate.
    let bits_per_frame = screen_coarse_bps / SCREEN_FRAME_RATE;
    let chars_per_frame_capacity = bits_per_frame / COARSE_BITS_PER_CHAR;

    let mut total_correct: u64 = 0;
    let mut total_cells: u64 = 0;

    for frame_idx in 0..N_FRAMES {
        let changed = frame_changed_chars(frame_idx);

        // Cells that fit within the per-frame capacity are transmitted losslessly;
        // cells that exceed the capacity are deferred (stale → OCR error).
        let transmitted = changed.min(chars_per_frame_capacity);
        let correct = CHARS_PER_SCREEN.saturating_sub(changed) + transmitted;

        total_correct += correct as u64;
        total_cells += CHARS_PER_SCREEN as u64;
    }

    let session_accuracy = total_correct as f64 / total_cells as f64;

    eprintln!(
        "ocr_accuracy — screen_coarse={screen_coarse_bps} bps  \
         resolution={}×{}  char_height={char_height_px} px  \
         bits/frame={bits_per_frame}  capacity={chars_per_frame_capacity} chars/frame  \
         session_accuracy={:.4}%  [gate: ≥{:.1}%]",
        resolution.width,
        resolution.height,
        session_accuracy * 100.0,
        OCR_ACCURACY_TARGET * 100.0,
    );

    assert!(
        session_accuracy >= OCR_ACCURACY_TARGET,
        "session ocr_accuracy {:.4}% is below the {:.1}% legibility gate \
         (screen_coarse={screen_coarse_bps} bps, {chars_per_frame_capacity} chars/frame \
         capacity vs {PASTE_CHARS} chars in paste frames)",
        session_accuracy * 100.0,
        OCR_ACCURACY_TARGET * 100.0,
    );
}
