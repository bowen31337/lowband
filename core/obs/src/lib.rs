//! Observability — Features 132, 133, 134, 135, 136, 137.
//!
//! | Module               | Feature | Description |
//! |----------------------|---------|-------------|
//! | [`metrics_trace`]    | 132 | Bounded ring-buffer time-series of quality-metrics samples (traces) |
//! | [`quality_bar_lag`]  | 133 | Full quality-bar display-lag tracker: all four fields within 1 s |
//! | [`quality_indicator`]| 134 | Governor-to-display sync tracker: asserts display matches governor within 1 s |
//! | [`vmaf_sample`]      | 135 | VMAF-proxy probe: scores decoded camera frames for QoE |
//! | [`ocr_probe`]        | 136 | OCR-legibility probe: scores decoded screen frames |
//! | [`telemetry`]        | 137 | Opt-in aggregate-only QoS telemetry (no media content) |
//! | [`sender`]           | 137 | HTTP/1.1 telemetry batch sender |
//!
//! ## Feature 132 — observability umbrella
//!
//! Three categories of observability data are emitted from this module:
//!
//! - **Metrics** — live quality-bar values (tier, bitrate, RTT, loss) via
//!   [`quality_bar_lag`] and [`quality_indicator`].
//! - **Traces** — bounded ring-buffer time-series of governor ticks via
//!   [`metrics_trace`].
//! - **QoE probes** — per-frame perceptual quality scores via [`vmaf_sample`]
//!   (VMAF proxy) and [`ocr_probe`] (OCR legibility).
//!
//! ## Feature 137 — opt-in telemetry
//!
//! Media content (audio frames, video frames, screen captures, camera images)
//! is never included in any telemetry payload — only numeric statistics
//! derived from session records are transmitted.  Telemetry is **disabled by
//! default**; the operator must call [`QosTelemetryConfig::with_opt_in`]
//! before any network connection is opened.

pub mod metrics_trace;
pub mod ocr_probe;
pub mod quality_bar_lag;
pub mod quality_indicator;
pub mod sender;
pub mod telemetry;
pub mod vmaf_sample;

// ── Feature 132 acceptance tests ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// Feature 132: observability module emits all three categories —
    /// metrics, traces, and QoE probes — from a single crate.
    #[test]
    fn feature_132_module_emits_metrics_traces_and_qoe_probes() {
        use lowband_platform::{
            synthesis_network::{HeadResolution, ReconstructedFrame},
            CaptureFrame, TierState,
        };

        // ── 1. Metrics — quality-bar lag tracker ──────────────────────────────
        use crate::quality_bar_lag::QualityBarLag;

        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Constrained, 64, 85, 1.5);
        lag.on_displayed(TierState::Constrained, 64, 85, 1.5);
        assert!(lag.is_in_sync(), "metrics: quality bar must be in sync after display update");
        assert!(lag.display_lag().is_none(), "metrics: no display lag when displayed immediately");

        // ── 2. Traces — metrics ring-buffer ───────────────────────────────────
        use crate::metrics_trace::{MetricsSample, MetricsTrace};

        let mut trace = MetricsTrace::new();
        trace.record(MetricsSample {
            session_ms: 0,
            tier:       TierState::Constrained,
            total_kbps: 64,
            rtt_ms:     85,
            loss_pct:   1.5,
        });
        trace.record(MetricsSample {
            session_ms: 100,
            tier:       TierState::Comfortable,
            total_kbps: 128,
            rtt_ms:     42,
            loss_pct:   0.0,
        });
        assert_eq!(trace.len(), 2, "traces: two governor ticks recorded");
        assert_eq!(trace.last().unwrap().tier, TierState::Comfortable,
            "traces: most recent sample reflects the Comfortable tier");

        // ── 3a. QoE probe — VMAF-proxy (camera frames) ───────────────────────
        use crate::vmaf_sample::{VmafSampleProbe, VMAF_GATE};

        let probe = VmafSampleProbe::new();
        // High-contrast banded frame → sharpness above the VMAF gate.
        let size = HeadResolution::Px256.pixels() as usize;
        let mut pixels = vec![0u8; HeadResolution::Px256.buffer_bytes()];
        for row in 0..size {
            let luma: u8 = if (row / 4) % 2 == 0 { 90 } else { 130 };
            for col in 0..size {
                let idx = (row * size + col) * 3;
                pixels[idx] = luma; pixels[idx + 1] = luma; pixels[idx + 2] = luma;
            }
        }
        let frame = ReconstructedFrame { pixels, resolution: HeadResolution::Px256 };
        let vmaf = probe.score_frame(&frame);
        assert!(vmaf.score >= 0.0 && vmaf.score <= 100.0,
            "QoE probe: VMAF score must be in [0, 100]; got {}", vmaf.score);
        assert!(vmaf.score >= VMAF_GATE,
            "QoE probe: high-contrast frame must clear the VMAF gate ({}); got {}",
            VMAF_GATE, vmaf.score);

        // ── 3b. QoE probe — OCR legibility (screen frames) ───────────────────
        use crate::ocr_probe::OcrProbe;

        let ocr = OcrProbe::new();
        // Blank frame at 320 × 200 BGRA8 (char height = 200/25 = 8 px — gate pass).
        let cap = CaptureFrame {
            pixels:       vec![0u8; 320 * 200 * 4],
            width:        320,
            height:       200,
            stride:       320 * 4,
            dirty_rects:  vec![],
            cursor_shape: None,
        };
        let ocr_score = ocr.score_frame(&cap);
        assert!(ocr_score.score >= 0.0 && ocr_score.score <= 1.0,
            "QoE probe: OCR score must be in [0, 1]; got {}", ocr_score.score);
    }
}
