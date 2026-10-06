//! Pure, GPU-free calibration helpers for the worker (D-06 / Pattern 4).
//!
//! Every decision the calibration flow makes — compatibility checks, the
//! engine-step → host-stage mapping, the scorecard projection, and the options
//! build — lives here as a pure function so it is unit-testable without a GPU
//! (RESEARCH Pattern 4; FRICTION A9/A11 show mocks drift when they reimplement
//! decisions). The worker calls these; nothing here touches a device, a file,
//! or the event channel.
//!
//! Typed `thiserror` errors only; this module deliberately imports no `anyhow`.

use reco_calibrate::error::CalibrateError;
use reco_calibrate::geometry::{OptParams, per_point_reprojection_error};
use reco_calibrate::intrinsics::{IntrinsicsRefinement, RefinementReason};
use reco_calibrate::types::{
    CalibrationConfig, CalibrationResult, CalibrationStep, FrameMatches, LensProfileInfo,
    MatchedPoint, ProfileSource, SyncMethod as EngineSyncMethod,
};
use reco_core::calibration::{CameraParams, MatchCalibration, PlaneLayout};

use crate::events::{
    CalibrationDiagnosis, CalibrationOptions, CalibrationStage, ConfidenceBand, DebugPoint,
    DebugReport, DiagnosisMetrics, FrameMatchRow, InputMetadata, IntrinsicsRefinementView,
    LensProfileView, ReadinessCode, ReadinessFinding, ReadinessReport, ReadinessSeverity,
    SYNC_OFFSET_SEMANTICS, Scorecard, SyncMethod, SyncProvenance, SyncView, ValidationVerdict,
};

/// Sampled statistics feeding the readiness estimate (CALB-05).
///
/// Both fields are `None` when the cheap sampled pass could not run (a decode
/// failure, a non-UTF8 path, …): the report then states the value is unknown
/// rather than faking a number (T-04-07).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReadinessSamples {
    /// Estimated horizontal overlap fraction (`0.0..=1.0`), if sampled.
    pub overlap: Option<f64>,
    /// Estimated exposure difference in stops, if sampled.
    pub exposure_delta_stops: Option<f64>,
}

/// Exposure difference (stops) above which readiness flags a likely quality
/// loss (CALB-05 discretion). One stop is a visible mismatch on consumer gear.
pub const READINESS_EXPOSURE_WARN_STOPS: f64 = 1.0;

/// Overlap fraction below which readiness warns the pair may not stitch
/// (CALB-05 discretion).
pub const READINESS_OVERLAP_WARN: f64 = 0.20;

/// Severity-sorted readiness projection for the two selected inputs (CALB-05).
///
/// Supersedes the Phase-3 [`check_compatibility`]: the same resolution/aspect/
/// fps/codec checks become severity-classified findings, joined by the sampled
/// exposure/overlap estimates and lens-profile availability. Results are sorted
/// blocking-shape first, informational last. Non-blocking by design — the
/// operator may always calibrate anyway (D3-05).
///
/// Pure (no device/file/channel): the worker samples the clips and calls this;
/// the frontend only renders the findings. An unknown estimate is `None`, never
/// a fabricated `0.0` (T-04-07).
pub fn estimate_readiness(
    left: &InputMetadata,
    right: &InputMetadata,
    left_path: &str,
    right_path: &str,
    lens_available: bool,
    samples: ReadinessSamples,
) -> ReadinessReport {
    let mut findings = Vec::new();

    // Same-file guard: the only check shown even though it is almost always
    // wrong; still non-blocking (D3-05). An empty path is a not-yet-filled slot
    // and never equals another empty path for this purpose.
    if !left_path.is_empty() && left_path == right_path {
        findings.push(ReadinessFinding {
            code: ReadinessCode::SameFile,
            severity: ReadinessSeverity::BlockingShape,
            message: format!(
                "Camera A and Camera B point at the same file ({left_path}); they must be different clips"
            ),
            estimated: false,
        });
    }

    if let (Some((lw, lh)), Some((rw, rh))) = (parse_resolution(left), parse_resolution(right))
        && (lw, lh) != (rw, rh)
    {
        findings.push(ReadinessFinding {
            code: ReadinessCode::ResolutionMismatch,
            severity: ReadinessSeverity::BlockingShape,
            message: format!("Camera A is {lw}×{lh} but Camera B is {rw}×{rh}"),
            estimated: false,
        });
        // Aspect ratio, tolerance ~1% of the larger aspect.
        let left_aspect = lw as f64 / lh as f64;
        let right_aspect = rw as f64 / rh as f64;
        let tolerance = 0.01 * left_aspect.max(right_aspect);
        if (left_aspect - right_aspect).abs() > tolerance {
            findings.push(ReadinessFinding {
                code: ReadinessCode::AspectMismatch,
                severity: ReadinessSeverity::BlockingShape,
                message: format!(
                    "Camera A and Camera B have different aspect ratios ({left_aspect:.3} vs {right_aspect:.3})"
                ),
                estimated: false,
            });
        }
    }

    if let (Some(lfps), Some(rfps)) = (parse_fps(left), parse_fps(right))
        && (lfps - rfps).abs() > 0.5
    {
        findings.push(ReadinessFinding {
            code: ReadinessCode::FpsMismatch,
            severity: ReadinessSeverity::LikelyQuality,
            message: format!("Camera A runs at {lfps} fps but Camera B runs at {rfps} fps"),
            estimated: false,
        });
    }

    if let (Some(lcodec), Some(rcodec)) = (codec_name(left), codec_name(right))
        && lcodec != rcodec
    {
        findings.push(ReadinessFinding {
            code: ReadinessCode::CodecMismatch,
            severity: ReadinessSeverity::LikelyQuality,
            message: format!(
                "Camera A is {lcodec} but Camera B is {rcodec}; they should use the same codec"
            ),
            estimated: false,
        });
    }

    // Exposure mismatch from the sampled pass — labelled `estimated`, never
    // presented as a measured value (CALB-05 / prohibition).
    if let Some(stops) = samples.exposure_delta_stops
        && stops.abs() > READINESS_EXPOSURE_WARN_STOPS
    {
        findings.push(ReadinessFinding {
            code: ReadinessCode::ExposureMismatch,
            severity: ReadinessSeverity::LikelyQuality,
            message: format!(
                "Exposure differs by {:.1} stops between cameras.",
                stops.abs()
            ),
            estimated: true,
        });
    }

    // Low overlap from the sampled pass — labelled `estimated`.
    if let Some(overlap) = samples.overlap
        && overlap < READINESS_OVERLAP_WARN
    {
        findings.push(ReadinessFinding {
            code: ReadinessCode::LowOverlap,
            severity: ReadinessSeverity::LikelyQuality,
            message: format!(
                "Estimated overlap is only {:.0}% — the cameras may barely see the same scene.",
                overlap * 100.0
            ),
            estimated: true,
        });
    }

    // Lens-profile availability is informational, never blocking: the engine
    // auto-detects, so a missing profile is a specific, actionable note.
    if !lens_available {
        findings.push(ReadinessFinding {
            code: ReadinessCode::LensUnavailable,
            severity: ReadinessSeverity::Informational,
            message: "No lens profile matched; the engine will auto-detect.".to_string(),
            estimated: false,
        });
    }

    findings.sort_by_key(|finding| finding.severity);

    ReadinessReport {
        findings,
        overlap_estimate: samples.overlap,
        exposure_delta_stops: samples.exposure_delta_stops,
    }
}

/// Parse a `"1920×1080"` display value into `(width, height)`.
pub(crate) fn parse_resolution(metadata: &InputMetadata) -> Option<(u32, u32)> {
    let value = metadata.resolution.value.as_deref()?;
    let (w, h) = value.split_once('×')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Parse a `"30 fps"` display value into its numeric frame rate.
fn parse_fps(metadata: &InputMetadata) -> Option<f64> {
    let value = metadata.fps.value.as_deref()?;
    let number = value.split_whitespace().next()?;
    number.parse().ok()
}

/// The codec name, when present.
fn codec_name(metadata: &InputMetadata) -> Option<&str> {
    metadata.codec.value.as_deref().filter(|c| !c.is_empty())
}

/// Map an engine [`CalibrationStep`] to its host [`CalibrationStage`] (CALB-01).
pub fn stage_from_step(step: CalibrationStep) -> CalibrationStage {
    match step {
        CalibrationStep::Probing => CalibrationStage::Probing,
        CalibrationStep::DetectingProfiles => CalibrationStage::DetectingProfiles,
        CalibrationStep::AudioSync => CalibrationStage::AudioSync,
        CalibrationStep::ExtractingFrames => CalibrationStage::ExtractingFrames,
        CalibrationStep::Undistorting => CalibrationStage::Undistorting,
        CalibrationStep::FeatureMatching => CalibrationStage::FeatureMatching,
        CalibrationStep::Optimizing => CalibrationStage::Optimizing,
    }
}

/// Aggregate per-frame match metrics for a failure diagnosis (CALB-04 / T-04-03).
///
/// Only counters cross the channel — never the raw match points — so a large
/// partial `FrameMatches` set cannot bloat the worker event (T-04-03). The
/// `keypoints_*` figures are the **minimum** across the frames that produced
/// matches (the weakest frame bounds the run); the other counters are sums;
/// `frames_used` is the frame count. Everything is zero when `frames` is empty.
fn aggregate_diagnosis_metrics(frames: &[FrameMatches]) -> DiagnosisMetrics {
    let sum = |f: fn(&FrameMatches) -> usize| frames.iter().map(f).sum::<usize>();
    DiagnosisMetrics {
        frames_used: frames.len(),
        total_matches: sum(|fm| fm.points.len()),
        post_ratio_test: sum(|fm| fm.post_ratio_test),
        post_spatial_filter: sum(|fm| fm.post_spatial_filter),
        post_ransac: sum(|fm| fm.post_ransac),
        keypoints_left: frames.iter().map(|fm| fm.keypoints_left).min().unwrap_or(0),
        keypoints_right: frames
            .iter()
            .map(|fm| fm.keypoints_right)
            .min()
            .unwrap_or(0),
    }
}

/// Map a typed engine failure to a plain-language cause and suggested fix
/// (CALB-04).
///
/// Pure (no device/file/channel): the worker calls this before the diagnosis
/// crosses the event channel, and the frontend only renders the result. Every
/// [`CalibrateError`] variant gets a specific cause and fix; `InvalidConfig`
/// and `FftError` fall back to the typed `Display` text plus a generic next
/// step — never a fabricated cause.
pub fn diagnose_calibration_failure(
    source: &CalibrateError,
    stage: CalibrationStage,
    frames: &[FrameMatches],
) -> CalibrationDiagnosis {
    let metrics = aggregate_diagnosis_metrics(frames);
    let (cause, fix) = match source {
        CalibrateError::NoKeypoints { camera, frame_idx } => (
            format!("No features were detected in the {camera} frame {frame_idx}."),
            "Shoot a well-lit, textured scene — avoid blank walls, plain sky, or a dark frame — \
             and check the camera's exposure and focus."
                .to_string(),
        ),
        CalibrateError::InsufficientMatches { got, min } => (
            format!("Only {got} matches were found, fewer than the {min} the calibration needs."),
            "Increase the overlap between the cameras, avoid motion blur and large exposure \
             differences, and check for rolling-shutter skew."
                .to_string(),
        ),
        CalibrateError::RansacFailed => (
            "Geometric verification rejected every candidate match.".to_string(),
            "The views may barely overlap or a large moving object dominates the scene; reduce \
             the mismatched regions (or mask them with the field ROI) and re-aim the cameras."
                .to_string(),
        ),
        CalibrateError::OptimizerFailed { max_evals } => (
            format!("The optimizer did not converge after {max_evals} evaluations."),
            "The cameras may be out of sync or the starting guess too far off; confirm the sync \
             offset and enable IMU rotation seeds."
                .to_string(),
        ),
        CalibrateError::NoUsableFrames => (
            "No frame pair produced usable matches.".to_string(),
            "Check that the two clips overlap, and set the correct lens profile so undistortion \
             matches the camera."
                .to_string(),
        ),
        CalibrateError::InvalidDimensions { width, height } => (
            format!("A frame had invalid dimensions ({width}×{height})."),
            "Re-export the clip at a normal video resolution and try again.".to_string(),
        ),
        CalibrateError::InvalidBuffer { expected, got } => (
            format!("A frame buffer was the wrong size (expected {expected} bytes, got {got})."),
            "This points to a decoding mismatch; re-import the clip or try a different codec."
                .to_string(),
        ),
        CalibrateError::ImageTooSmall { width, height } => (
            format!("The frames are too small for feature detection ({width}×{height})."),
            "Use higher-resolution footage — feature matching needs images at least a few hundred \
             pixels on each side."
                .to_string(),
        ),
        // `InvalidConfig` / `FftError` have no actionable mapping of their own:
        // fall back to the typed Display text plus a generic next step rather
        // than inventing a cause (CALB-04 / T-04-02).
        CalibrateError::FftError(_) | CalibrateError::InvalidConfig(_) => (
            source.to_string(),
            "Check the clips and settings, then try again.".to_string(),
        ),
    };
    CalibrationDiagnosis {
        cause,
        fix,
        raw_error: source.to_string(),
        stage,
        metrics,
    }
}

/// Band a calibration confidence into `High` / `Medium` / `Low` (CALB-03).
///
/// `High` for `>= 0.8`, `Medium` for `>= 0.5`, else `Low`.
pub fn confidence_band(confidence: f64) -> ConfidenceBand {
    if confidence >= 0.8 {
        ConfidenceBand::High
    } else if confidence >= 0.5 {
        ConfidenceBand::Medium
    } else {
        ConfidenceBand::Low
    }
}

/// Project a [`CalibrationResult`] into the CALB-03 [`Scorecard`] (D3-13).
///
/// Never fabricates a field: `per_frame_matches` is `0` only when
/// `frames_used == 0`, and a missing lens profile is `None`.
pub fn project_scorecard(result: &CalibrationResult) -> Scorecard {
    let per_frame_matches = if result.frames_used > 0 {
        result.total_matches as f64 / result.frames_used as f64
    } else {
        0.0
    };

    // Prefer the left profile, fall back to the right (the scorecard reports one
    // resolved profile + source).
    let lens_profile = result
        .left_lens_profile
        .as_ref()
        .or(result.right_lens_profile.as_ref())
        .map(|info| LensProfileView {
            name: profile_name(info),
            source: profile_source_label(&info.source),
        });

    Scorecard {
        confidence: result.confidence,
        confidence_band: confidence_band(result.confidence),
        residual_error: result.residual_error,
        total_matches: result.total_matches as u64,
        per_frame_matches,
        frames_used: result.frames_used as u64,
        lens_profile,
        sync: {
            // The provenance chain mark mirrors the method that produced the
            // offset; manual is the only path with no computed confidence.
            let method = map_sync_method(result.sync.method);
            SyncView {
                method,
                confidence: result.sync.confidence,
                offset_frames: result.sync.offset_frames,
                provenance: SyncProvenance {
                    ran: method,
                    is_manual: matches!(method, SyncMethod::Manual),
                },
                // The fixed semantics string is authored once (CALB-06): carry
                // it on the payload so no surface re-types it.
                offset_semantics: SYNC_OFFSET_SEMANTICS.to_string(),
            }
        },
    }
}

/// The user-facing text for a refinement reason (INTR-03).
///
/// Authored **once** in Rust — exactly as the CALB-04 diagnosis `cause` is — so
/// the webview renders the engine's verdict verbatim and never re-types a cause
/// (T-04.2-12). The rejection wording matches the UI-SPEC Copywriting Contract.
#[must_use]
pub fn refinement_reason_text(reason: RefinementReason) -> &'static str {
    match reason {
        RefinementReason::Accepted => "Refinement accepted.",
        RefinementReason::GuardRejected => {
            "Refinement rejected — it did not improve the held-out fit. Lens unchanged."
        }
        RefinementReason::NotEnoughSpread => {
            "Not enough spread to refine the lens — matches are too centre-weighted. Lens unchanged."
        }
        RefinementReason::InsufficientMatches => {
            "Not enough matches to refine the lens. Lens unchanged."
        }
        RefinementReason::IllConditioned => {
            "Lens refinement is ill-conditioned on this data. Lens unchanged."
        }
    }
}

/// Project an engine [`IntrinsicsRefinement`] into the typed webview view
/// (INTR-03).
///
/// Pure (no device/file/channel): the worker passes the engine result plus the
/// profile's pre-refinement `k1` and this returns exactly what the readout
/// renders. The held-out residuals are `None` when the conditioning gate
/// refused before any held-out evaluation (an `InsufficientMatches` /
/// `NotEnoughSpread` / `IllConditioned` reason), so a refusal never renders a
/// fabricated `0.0` (UI-SPEC Readout & Guard Contract). `k1` is passed through
/// verbatim: on any rejection the engine returns the baseline `k1`, so the
/// readout always shows the value the profile actually holds.
#[must_use]
pub fn project_intrinsics_refinement(
    refinement: &IntrinsicsRefinement,
    baseline_k1: f64,
) -> IntrinsicsRefinementView {
    // Only an accepted or guard-rejected result has evaluated the held-out
    // split; a conditioning refusal returns before the split is built.
    let evaluated = matches!(
        refinement.reason,
        RefinementReason::Accepted | RefinementReason::GuardRejected
    );

    IntrinsicsRefinementView {
        k1: refinement.k1,
        baseline_k1,
        accepted: refinement.accepted,
        reason: refinement_reason_text(refinement.reason).to_string(),
        heldout_baseline: evaluated.then_some(refinement.heldout_baseline),
        heldout_refined: evaluated.then_some(refinement.heldout_refined),
    }
}

/// Maximum edge (px) of a debug thumbnail crossing the event channel (T-04-10).
///
/// The retained undistorted frame pair can be full-resolution; only the
/// downscaled thumbnail crosses to the webview, so the event payload stays
/// bounded regardless of the source resolution.
pub const DEBUG_THUMB_MAX_EDGE: u32 = 960;

/// Maximum number of verified + rejected points in one debug report (T-04-10).
///
/// Caps the overlay's per-frame marker count so a pathological match set cannot
/// freeze the canvas; the report records the truncation in `points_capped`.
pub const DEBUG_POINT_CAP: usize = 2000;

/// Maximum edge (px) of a manual preview/validation frame crossing the event
/// channel (MANU-03 / MANU-07).
///
/// Every manual frame the worker renders is box-downsampled to this bound before
/// it is emitted, exactly as [`build_debug_report`] bounds its thumbnails.
/// Without it the preview path shipped full-resolution readback RGBA on every
/// handle drag (1920×1080×4 ≈ 8.29 MB per frame, two cameras per mutation),
/// which flooded the webview event queue and drove `WebKitWebProcess` RSS to tens
/// of GB with 20–40 s input lag. At 960 px a 16:9 frame is ~960×540×4 ≈ 2.07 MB
/// (a ~4× reduction); the canvas scales the bounded frame, so the preview stays
/// visually correct. The bound matches [`DEBUG_THUMB_MAX_EDGE`] so both
/// RGBA-to-webview paths share one payload ceiling.
pub const MANUAL_FRAME_MAX_EDGE: u32 = 960;

/// Build the bounded debug inspector report for a completed run (CALB-08).
///
/// Pure (no device/file/channel): the worker passes the run's per-frame
/// `FrameMatches` (from a successful `CalibrationResult` or the partial
/// `CalibrationFailure`) and, on success, the fitted [`PlaneLayout`] so each
/// verified point gets a real per-point reprojection residual. Rejected points
/// carry the run's `residual_error` (their own residual is undefined once they
/// are excluded from the fit). The chosen frame is the one that retained an
/// undistorted pair; its thumbnails are box-downsampled to
/// [`DEBUG_THUMB_MAX_EDGE`] and the point lists are capped at
/// [`DEBUG_POINT_CAP`]. An empty input yields an empty, fail-closed report.
pub fn build_debug_report(
    frames: &[FrameMatches],
    layout: Option<&PlaneLayout>,
    residual_error: f64,
) -> DebugReport {
    let frames_total = frames.len() as u64;
    let per_frame: Vec<FrameMatchRow> = frames
        .iter()
        .enumerate()
        .map(|(i, fm)| FrameMatchRow {
            frame: i as u64,
            keypoints_left: fm.keypoints_left,
            keypoints_right: fm.keypoints_right,
            post_ratio_test: fm.post_ratio_test,
            post_spatial_filter: fm.post_spatial_filter,
            post_ransac: fm.post_ransac,
        })
        .collect();

    // Prefer the frame that retained an undistorted pair; else the first frame.
    let chosen = frames
        .iter()
        .position(|fm| fm.debug_frame.is_some())
        .unwrap_or(0);
    let frame = frames.get(chosen);

    let debug_frame = frame.and_then(|fm| fm.debug_frame.as_ref());
    let (left_thumb, left_w, left_h) = match debug_frame {
        Some(df) => downsample_rgba(
            &df.left,
            df.left_width,
            df.left_height,
            DEBUG_THUMB_MAX_EDGE,
        ),
        None => (Vec::new(), 0, 0),
    };
    let (right_thumb, right_w, right_h) = match debug_frame {
        Some(df) => downsample_rgba(
            &df.right,
            df.right_width,
            df.right_height,
            DEBUG_THUMB_MAX_EDGE,
        ),
        None => (Vec::new(), 0, 0),
    };

    let mut verified: Vec<DebugPoint> = Vec::new();
    let mut rejected: Vec<DebugPoint> = Vec::new();
    let mut points_capped = false;

    if let Some(fm) = frame {
        // Use the ORIGINAL undistorted frame's aspect for the y normalization
        // (the thumbnail preserves it only approximately under integer rounding).
        let (coord_w, coord_h) = debug_frame
            .map(|df| (df.left_width, df.left_height))
            .unwrap_or((left_w, left_h));
        let (cw, ch) = (coord_w.max(1) as f64, coord_h.max(1) as f64);
        let aspect = cw / ch;

        // Per-point residual when the fit is available (success path).
        let per_point = layout.map(|l| {
            let params = OptParams {
                x_ty: l.x_ty,
                intersect: l.intersect,
                cam_d: l.camera_axis_offset,
                x_rz: l.x_rz,
                z_rx: l.z_rx,
                z_rz: None,
                x_rx: None,
            };
            per_point_reprojection_error(&fm.points, &params)
        });
        let to_point = |p: &MatchedPoint, error: f64| DebugPoint {
            // `right_pixel_nx` is the LEFT camera keypoint's normalized x (the
            // engine's historical swap convention); `right` is its plane y.
            x_nx: p.right_pixel_nx.clamp(0.0, 1.0),
            y_nx: (p.right[1] * aspect + 0.5).clamp(0.0, 1.0),
            error,
        };

        for (i, p) in fm.points.iter().enumerate() {
            if verified.len() + rejected.len() >= DEBUG_POINT_CAP {
                points_capped = true;
                break;
            }
            let error = per_point
                .as_ref()
                .and_then(|v| v.get(i).copied())
                .unwrap_or(residual_error);
            verified.push(to_point(p, error));
        }
        for p in &fm.rejected {
            if verified.len() + rejected.len() >= DEBUG_POINT_CAP {
                points_capped = true;
                break;
            }
            rejected.push(to_point(p, residual_error));
        }
    }

    DebugReport {
        frame_index: chosen as u64,
        frames_total,
        left_width: left_w,
        left_height: left_h,
        right_width: right_w,
        right_height: right_h,
        left_thumb,
        right_thumb,
        verified,
        rejected,
        residual_error,
        per_frame,
        points_capped,
    }
}

/// Box-downsample an RGBA buffer so its longest edge is at most `max_edge`.
///
/// Returns `(rgba, width, height)`; a zero-sized or short buffer yields an
/// empty fail-closed result. A buffer already within the bound is copied
/// unchanged (still bounded by `max_edge`). Shared by the CALB-08 debug
/// thumbnails and the MANU-03/MANU-07 manual preview/validation frames, so both
/// RGBA-to-webview paths cross the same bounded size.
pub(crate) fn downsample_rgba(src: &[u8], w: u32, h: u32, max_edge: u32) -> (Vec<u8>, u32, u32) {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || src.len() < w * h * 4 || max_edge == 0 {
        return (Vec::new(), 0, 0);
    }
    let longest = w.max(h);
    if longest as u32 <= max_edge {
        return (src[..w * h * 4].to_vec(), w as u32, h as u32);
    }
    let scale = max_edge as f64 / longest as f64;
    let nw = ((w as f64 * scale).round() as usize).max(1);
    let nh = ((h as f64 * scale).round() as usize).max(1);
    let mut out = vec![0u8; nw * nh * 4];
    for oy in 0..nh {
        let sy0 = oy * h / nh;
        let sy1 = ((oy + 1) * h / nh).max(sy0 + 1).min(h);
        for ox in 0..nw {
            let sx0 = ox * w / nw;
            let sx1 = ((ox + 1) * w / nw).max(sx0 + 1).min(w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let idx = (sy * w + sx) * 4;
                    r += src[idx] as u32;
                    g += src[idx + 1] as u32;
                    b += src[idx + 2] as u32;
                    a += src[idx + 3] as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            let oidx = (oy * nw + ox) * 4;
            out[oidx] = (r / n) as u8;
            out[oidx + 1] = (g / n) as u8;
            out[oidx + 2] = (b / n) as u8;
            out[oidx + 3] = (a / n) as u8;
        }
    }
    (out, nw as u32, nh as u32)
}

/// Build the engine [`CalibrateVideosOptions`](reco_calibrate::video::CalibrateVideosOptions)
/// from the wizard's advanced options (D3-12) and the resolved lens overrides.
///
/// `left`/`right` params must be set **together** (the engine ignores a
/// one-sided pair — RESEARCH Pitfall 7); passing only one falls back to
/// auto-detect.
pub fn build_video_options(
    options: &CalibrationOptions,
    left: Option<CameraParams>,
    right: Option<CameraParams>,
) -> reco_calibrate::video::CalibrateVideosOptions {
    let mut config = CalibrationConfig::default();
    if let Some(num_frames) = options.num_frames {
        config.num_frames = num_frames;
    }
    if let Some(skip_start_secs) = options.skip_start_secs {
        config.skip_start_secs = skip_start_secs;
    }
    if let Some(skip_end_secs) = options.skip_end_secs {
        config.skip_end_secs = skip_end_secs;
    }
    if let Some(use_imu_rotation_seeds) = options.use_imu_rotation_seeds {
        config.use_imu_rotation_seeds = use_imu_rotation_seeds;
    }

    // Both or neither: the engine warns and falls back to auto-detect on a
    // one-sided pair, so drop a half-set override here rather than rely on that.
    let (left_params, right_params) = match (left, right) {
        (Some(l), Some(r)) => (Some(l), Some(r)),
        _ => (None, None),
    };

    reco_calibrate::video::CalibrateVideosOptions {
        config: Some(config),
        left_params,
        right_params,
        ..Default::default()
    }
}

/// Per-frame residual (px) at or below which a validation frame's advisory
/// verdict can pass (MANU-07).
///
/// Advisory only: the residual is the engine's seam-weighted reprojection error
/// on the validation frame. `0.5 px` is a generous pass band — a well-aligned
/// rig sits near `0.25 px` — chosen so the verdict flags a visibly off result
/// without failing a merely-good one. Agent discretion (04.1-CONTEXT).
pub const VALIDATION_RESIDUAL_THRESHOLD: f64 = 0.5;

/// Absolute layout agreement tolerance for the advisory verdict (MANU-07).
///
/// The engine's independent solve on the validation frame and the operator's
/// manual layout should agree; any of the seven fields differing by more than
/// this flags `CheckSeam`. A single absolute tolerance across the fields is
/// deliberately simple and advisory (agent discretion).
pub const VALIDATION_LAYOUT_TOLERANCE: f64 = 0.05;

/// Compute the advisory validation verdict for one additional frame (MANU-07).
///
/// `LooksGood` when every layout field of the manual layout agrees with the
/// engine's independent solve on the validation frame within
/// [`VALIDATION_LAYOUT_TOLERANCE`] **and** the per-frame residual is finite and
/// at or below [`VALIDATION_RESIDUAL_THRESHOLD`]; `CheckSeam` otherwise. Purely
/// advisory — it never blocks saving (MANU-07 prohibition). Pure: no device,
/// file, or channel.
pub fn validation_verdict(
    cal_layout: &PlaneLayout,
    val_layout: &PlaneLayout,
    residual: f64,
) -> ValidationVerdict {
    let agrees = [
        (cal_layout.camera_axis_offset, val_layout.camera_axis_offset),
        (cal_layout.intersect, val_layout.intersect),
        (cal_layout.x_ty, val_layout.x_ty),
        (cal_layout.x_rz, val_layout.x_rz),
        (cal_layout.z_rx, val_layout.z_rx),
        (cal_layout.x_rx, val_layout.x_rx),
        (cal_layout.z_rz, val_layout.z_rz),
    ]
    .iter()
    .all(|(a, b)| (a - b).abs() <= VALIDATION_LAYOUT_TOLERANCE);
    if agrees && residual.is_finite() && residual <= VALIDATION_RESIDUAL_THRESHOLD {
        ValidationVerdict::LooksGood
    } else {
        ValidationVerdict::CheckSeam
    }
}

/// Assemble the manual result as a normal calibration profile (MANU-07).
///
/// Copies every carried profile field from `base` — `field_roi`,
/// `lens_correction_amount`, `blend_width`, `rig_tilt`, `rig_roll` — verbatim,
/// and sets the operator's edited `left`/`right` intrinsics, the layout in
/// effect, and the chosen `sync_offset`. Manual and auto profiles are therefore
/// identical in shape and round-trip through Calibrate/Preview/Export unchanged
/// (T-04.1-18). Pure: no device, file, or channel.
pub fn build_manual_match_calibration(
    base: &MatchCalibration,
    left: CameraParams,
    right: CameraParams,
    layout: PlaneLayout,
    sync_offset: i64,
) -> MatchCalibration {
    MatchCalibration {
        left,
        right,
        layout,
        rig_tilt: base.rig_tilt,
        rig_roll: base.rig_roll,
        sync_offset,
        field_roi: base.field_roi.clone(),
        lens_correction_amount: base.lens_correction_amount,
        blend_width: base.blend_width,
    }
}

/// The human-readable name for a resolved lens profile.
fn profile_name(info: &LensProfileInfo) -> String {
    if info.lens.trim().is_empty() {
        info.camera.clone()
    } else {
        format!("{} {}", info.camera, info.lens)
    }
}

/// The human-readable source label for a resolved lens profile.
fn profile_source_label(source: &ProfileSource) -> String {
    match source {
        ProfileSource::Database => "database".to_string(),
        ProfileSource::File(path) => {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
            format!("file: {name}")
        }
        ProfileSource::AutoDetected => "auto-detected".to_string(),
        ProfileSource::Fallback => "fallback".to_string(),
    }
}

/// Map the engine sync method to the host vocabulary.
fn map_sync_method(method: EngineSyncMethod) -> SyncMethod {
    match method {
        EngineSyncMethod::Imu => SyncMethod::Imu,
        EngineSyncMethod::Audio => SyncMethod::Audio,
        EngineSyncMethod::Manual => SyncMethod::Manual,
        EngineSyncMethod::None => SyncMethod::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{MetadataField, Provenance};
    use reco_calibrate::types::{
        CalibrationConfig, CalibrationResult, DebugFrame, FrameMatches, LensProfileInfo,
        MatchedPoint, ProfileSource, SyncInfo,
    };
    use reco_core::calibration::{CameraParams, MatchCalibration, PlaneLayout};

    fn camera(width: u32, height: u32) -> CameraParams {
        CameraParams {
            width,
            height,
            fx: 1000.0,
            fy: 1000.0,
            cx: width as f64 / 2.0,
            cy: height as f64 / 2.0,
            d: [0.0, 0.0, 0.0, 0.0],
        }
    }

    fn sample_calibration() -> MatchCalibration {
        MatchCalibration {
            left: camera(1920, 1080),
            right: camera(1920, 1080),
            layout: PlaneLayout {
                camera_axis_offset: 0.25,
                intersect: 0.5,
                x_ty: 0.0,
                x_rz: 0.0,
                z_rx: 0.0,
                x_rx: 0.0,
                z_rz: 0.0,
            },
            rig_tilt: 0.0,
            rig_roll: 0.0,
            sync_offset: 0,
            field_roi: None,
            lens_correction_amount: 1.0,
            blend_width: 0.05,
        }
    }

    fn metadata(resolution: &str, fps: &str, codec: Option<&str>) -> InputMetadata {
        InputMetadata {
            resolution: MetadataField::probed(resolution),
            fps: MetadataField::probed(fps),
            duration: MetadataField::estimated("0:10"),
            codec: match codec {
                Some(c) => MetadataField::probed(c),
                None => MetadataField::missing(Provenance::Probed),
            },
        }
    }

    fn sample_result() -> CalibrationResult {
        CalibrationResult {
            calibration: sample_calibration(),
            total_matches: 900,
            frames_used: 3,
            residual_error: 0.25,
            confidence: 0.87,
            per_frame: Vec::new(),
            frame_indices: Vec::new(),
            left_lens_profile: None,
            right_lens_profile: None,
            quality: None,
            sync: SyncInfo {
                method: EngineSyncMethod::None,
                confidence: None,
                offset_frames: 0,
            },
        }
    }

    #[test]
    fn identical_resolutions_never_report_a_resolution_finding() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let report = estimate_readiness(
            &left,
            &left.clone(),
            "/a.mp4",
            "/b.mp4",
            true,
            ReadinessSamples::default(),
        );
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.code != ReadinessCode::ResolutionMismatch),
            "identical resolutions must not report a mismatch: {:?}",
            report.findings
        );
    }

    #[test]
    fn the_same_path_for_both_roles_produces_exactly_one_same_file_finding() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let report = estimate_readiness(
            &left,
            &left.clone(),
            "/a.mp4",
            "/a.mp4",
            true,
            ReadinessSamples::default(),
        );
        let same_file: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == ReadinessCode::SameFile)
            .collect();
        assert_eq!(
            same_file.len(),
            1,
            "expected exactly one SameFile: {:?}",
            report.findings
        );
    }

    #[test]
    fn fps_differing_by_more_than_half_a_frame_is_a_likely_quality_finding() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let right = metadata("1920×1080", "60 fps", Some("h264"));
        let report = estimate_readiness(
            &left,
            &right,
            "/a.mp4",
            "/b.mp4",
            true,
            ReadinessSamples::default(),
        );
        let fps = report
            .findings
            .iter()
            .find(|f| f.code == ReadinessCode::FpsMismatch)
            .expect("30 vs 60 fps must report an FpsMismatch");
        assert_eq!(fps.severity, ReadinessSeverity::LikelyQuality);

        // Within tolerance: no finding.
        let near = metadata("1920×1080", "30.2 fps", Some("h264"));
        let tolerant = estimate_readiness(
            &left,
            &near,
            "/a.mp4",
            "/b.mp4",
            true,
            ReadinessSamples::default(),
        );
        assert!(
            tolerant
                .findings
                .iter()
                .all(|f| f.code != ReadinessCode::FpsMismatch),
            "30 vs 30.2 fps is within tolerance"
        );
    }

    #[test]
    fn differing_resolutions_produce_a_blocking_shape_readiness_finding() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let right = metadata("3840×2160", "30 fps", Some("h264"));
        let report = estimate_readiness(
            &left,
            &right,
            "/a.mp4",
            "/b.mp4",
            true,
            ReadinessSamples::default(),
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.code == ReadinessCode::ResolutionMismatch
                    && f.severity == ReadinessSeverity::BlockingShape),
            "expected a BlockingShape resolution finding: {:?}",
            report.findings
        );
    }

    #[test]
    fn an_exposure_delta_above_threshold_is_a_likely_quality_estimate() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let right = metadata("1920×1080", "30 fps", Some("h264"));
        let report = estimate_readiness(
            &left,
            &right,
            "/a.mp4",
            "/b.mp4",
            true,
            ReadinessSamples {
                overlap: Some(0.5),
                exposure_delta_stops: Some(2.0),
            },
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.code == ReadinessCode::ExposureMismatch)
            .expect("an exposure finding");
        assert_eq!(finding.severity, ReadinessSeverity::LikelyQuality);
        assert!(
            finding.estimated,
            "an exposure delta from sampling must be labelled estimated"
        );
        assert_eq!(report.exposure_delta_stops, Some(2.0));
    }

    #[test]
    fn no_lens_profile_is_an_informational_finding_never_blocking() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let right = metadata("1920×1080", "30 fps", Some("h264"));
        let report = estimate_readiness(
            &left,
            &right,
            "/a.mp4",
            "/b.mp4",
            false,
            ReadinessSamples::default(),
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.code == ReadinessCode::LensUnavailable)
            .expect("a lens-unavailable finding");
        assert_eq!(finding.severity, ReadinessSeverity::Informational);
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.severity != ReadinessSeverity::BlockingShape),
            "no lens profile must never block: {:?}",
            report.findings
        );
    }

    #[test]
    fn findings_are_sorted_blocking_shape_then_likely_quality_then_informational() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let right = metadata("3840×2160", "60 fps", Some("hevc"));
        let report = estimate_readiness(
            &left,
            &right,
            "/a.mp4",
            "/b.mp4",
            false,
            ReadinessSamples {
                overlap: Some(0.05),
                exposure_delta_stops: Some(3.0),
            },
        );
        let severities: Vec<ReadinessSeverity> =
            report.findings.iter().map(|f| f.severity).collect();
        let mut sorted = severities.clone();
        sorted.sort();
        assert_eq!(
            severities, sorted,
            "findings must be severity-sorted: {severities:?}"
        );
        assert!(severities.contains(&ReadinessSeverity::BlockingShape));
        assert!(severities.contains(&ReadinessSeverity::LikelyQuality));
        assert!(severities.contains(&ReadinessSeverity::Informational));
    }

    #[test]
    fn unknown_overlap_and_exposure_serialize_as_null_never_zero() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let right = metadata("1920×1080", "30 fps", Some("h264"));
        let report = estimate_readiness(
            &left,
            &right,
            "/a.mp4",
            "/b.mp4",
            true,
            ReadinessSamples::default(),
        );
        assert_eq!(report.overlap_estimate, None);
        assert_eq!(report.exposure_delta_stops, None);
        let json = serde_json::to_value(&report).expect("serializes");
        assert!(
            json["overlap_estimate"].is_null(),
            "unknown overlap must be null, never 0.0: {json}"
        );
        assert!(
            json["exposure_delta_stops"].is_null(),
            "unknown exposure must be null, never 0.0: {json}"
        );
    }

    #[test]
    fn project_scorecard_derives_per_frame_matches_and_bands_confidence() {
        let mut result = sample_result();
        result.total_matches = 900;
        result.frames_used = 3;
        result.confidence = 0.87;
        let card = project_scorecard(&result);
        assert!((card.per_frame_matches - 300.0).abs() < 1e-9);
        assert_eq!(card.confidence_band, ConfidenceBand::High);

        // Zero frames used: no division, and the average is an honest 0.
        result.frames_used = 0;
        let card = project_scorecard(&result);
        assert_eq!(card.per_frame_matches, 0.0);

        assert_eq!(confidence_band(0.5), ConfidenceBand::Medium);
        assert_eq!(confidence_band(0.49), ConfidenceBand::Low);
        assert_eq!(confidence_band(0.8), ConfidenceBand::High);
    }

    #[test]
    fn stage_from_step_maps_all_seven_engine_steps() {
        assert_eq!(
            stage_from_step(CalibrationStep::Probing),
            CalibrationStage::Probing
        );
        assert_eq!(
            stage_from_step(CalibrationStep::DetectingProfiles),
            CalibrationStage::DetectingProfiles
        );
        assert_eq!(
            stage_from_step(CalibrationStep::AudioSync),
            CalibrationStage::AudioSync
        );
        assert_eq!(
            stage_from_step(CalibrationStep::ExtractingFrames),
            CalibrationStage::ExtractingFrames
        );
        assert_eq!(
            stage_from_step(CalibrationStep::Undistorting),
            CalibrationStage::Undistorting
        );
        assert_eq!(
            stage_from_step(CalibrationStep::FeatureMatching),
            CalibrationStage::FeatureMatching
        );
        assert_eq!(
            stage_from_step(CalibrationStep::Optimizing),
            CalibrationStage::Optimizing
        );
    }

    #[test]
    fn an_imu_sync_result_projects_with_no_confidence() {
        let mut result = sample_result();
        result.sync = SyncInfo {
            method: EngineSyncMethod::Imu,
            confidence: None,
            offset_frames: 12,
        };
        let card = project_scorecard(&result);
        assert_eq!(card.sync.method, SyncMethod::Imu);
        assert_eq!(card.sync.confidence, None);
    }

    #[test]
    fn imu_sync_projects_no_confidence_and_marks_the_imu_provenance_step() {
        let mut result = sample_result();
        result.sync = SyncInfo {
            method: EngineSyncMethod::Imu,
            confidence: None,
            offset_frames: 12,
        };
        let card = project_scorecard(&result);
        assert_eq!(card.sync.confidence, None, "IMU computes no confidence");
        assert_eq!(card.sync.provenance.ran, SyncMethod::Imu);
        assert!(!card.sync.provenance.is_manual);
    }

    #[test]
    fn audio_sync_projects_the_confidence_and_marks_the_audio_provenance_step() {
        let mut result = sample_result();
        result.sync = SyncInfo {
            method: EngineSyncMethod::Audio,
            confidence: Some(0.9),
            offset_frames: -3,
        };
        let card = project_scorecard(&result);
        assert_eq!(card.sync.confidence, Some(0.9));
        assert_eq!(card.sync.provenance.ran, SyncMethod::Audio);
        assert!(!card.sync.provenance.is_manual);
    }

    #[test]
    fn no_sync_projects_none_and_makes_no_provenance_claim() {
        let mut result = sample_result();
        result.sync = SyncInfo {
            method: EngineSyncMethod::None,
            confidence: None,
            offset_frames: 0,
        };
        let card = project_scorecard(&result);
        assert_eq!(card.sync.confidence, None);
        assert_eq!(card.sync.provenance.ran, SyncMethod::None);
        assert!(!card.sync.provenance.is_manual);
    }

    #[test]
    fn a_manual_sync_is_marked_manual() {
        let mut result = sample_result();
        result.sync = SyncInfo {
            method: EngineSyncMethod::Manual,
            confidence: None,
            offset_frames: 4,
        };
        let card = project_scorecard(&result);
        assert!(card.sync.provenance.is_manual);
        assert_eq!(card.sync.provenance.ran, SyncMethod::Manual);
    }

    #[test]
    fn the_offset_frames_sign_is_projected_verbatim() {
        let mut result = sample_result();
        result.sync = SyncInfo {
            method: EngineSyncMethod::Audio,
            confidence: Some(0.8),
            offset_frames: 12,
        };
        assert_eq!(project_scorecard(&result).sync.offset_frames, 12);

        result.sync.offset_frames = -7;
        assert_eq!(project_scorecard(&result).sync.offset_frames, -7);
    }

    #[test]
    fn the_offset_semantics_string_matches_the_fixed_constant() {
        let result = sample_result();
        let card = project_scorecard(&result);
        assert_eq!(card.sync.offset_semantics, SYNC_OFFSET_SEMANTICS);
        assert_eq!(
            SYNC_OFFSET_SEMANTICS,
            "positive offset skips right frames, negative skips left frames"
        );
    }

    #[test]
    fn project_scorecard_maps_the_lens_profile_and_source() {
        let mut result = sample_result();
        result.left_lens_profile = Some(LensProfileInfo {
            camera: "GoPro HERO10".to_string(),
            lens: "Wide".to_string(),
            source: ProfileSource::Database,
            path: None,
        });
        let card = project_scorecard(&result);
        let profile = card.lens_profile.expect("a profile is reported");
        assert_eq!(profile.name, "GoPro HERO10 Wide");
        assert_eq!(profile.source, "database");
    }

    #[test]
    fn build_video_options_applies_advanced_fields_and_pairs_the_overrides() {
        let options = CalibrationOptions {
            num_frames: Some(50),
            skip_start_secs: Some(1.5),
            skip_end_secs: Some(2.0),
            use_imu_rotation_seeds: Some(true),
        };
        let opts = build_video_options(&options, None, None);
        let config = opts.config.expect("a config is built");
        assert_eq!(config.num_frames, 50);
        assert!((config.skip_start_secs - 1.5).abs() < 1e-9);
        assert!((config.skip_end_secs - 2.0).abs() < 1e-9);
        assert!(config.use_imu_rotation_seeds);
        assert!(opts.left_params.is_none() && opts.right_params.is_none());

        // A one-sided override is dropped (the engine ignores it) — auto-detect.
        let one_sided = build_video_options(&options, Some(camera(1920, 1080)), None);
        assert!(one_sided.left_params.is_none() && one_sided.right_params.is_none());

        // A paired override is applied.
        let paired =
            build_video_options(&options, Some(camera(1920, 1080)), Some(camera(1920, 1080)));
        assert!(paired.left_params.is_some() && paired.right_params.is_some());

        // Default options leave the engine defaults untouched.
        let defaults = build_video_options(&CalibrationOptions::default(), None, None);
        let default_config = CalibrationConfig::default();
        assert_eq!(
            defaults.config.unwrap().num_frames,
            default_config.num_frames
        );
    }

    #[test]
    fn diagnose_calibration_failure_produces_a_non_empty_cause_and_fix() {
        // CALB-04: a failed run yields a non-empty plain-language cause and fix,
        // never a bare code and never a blank.
        let diagnosis = diagnose_calibration_failure(
            &CalibrateError::NoUsableFrames,
            CalibrationStage::FeatureMatching,
            &[],
        );
        assert!(!diagnosis.cause.is_empty(), "cause must not be empty");
        assert!(!diagnosis.fix.is_empty(), "fix must not be empty");
        assert_eq!(diagnosis.stage, CalibrationStage::FeatureMatching);
        assert_eq!(diagnosis.metrics.frames_used, 0);
        assert_eq!(
            diagnosis.raw_error,
            "no usable frame pairs (all frames failed matching)"
        );
    }

    /// A synthetic `FrameMatches` with the given counters and point count.
    fn frame_matches(
        points: usize,
        keypoints_left: usize,
        keypoints_right: usize,
        post_ratio_test: usize,
        post_spatial_filter: usize,
        post_ransac: usize,
    ) -> FrameMatches {
        FrameMatches {
            points: (0..points)
                .map(|_| MatchedPoint::from_planes([0.0, 0.0], [0.0, 0.0]))
                .collect(),
            rejected: Vec::new(),
            keypoints_left,
            keypoints_right,
            min_descriptors: 0,
            post_ratio_test,
            post_spatial_filter,
            post_ransac,
            debug_frame: None,
        }
    }

    /// One `FrameMatches` with `n` verified points spread along the frame.
    fn debug_frames(n: usize) -> Vec<FrameMatches> {
        let denom = n.max(1) as f64;
        let points: Vec<MatchedPoint> = (0..n)
            .map(|i| MatchedPoint {
                left: [0.0, 0.0],
                right: [0.0, i as f64 / denom - 0.5],
                left_pixel_nx: 0.5,
                right_pixel_nx: i as f64 / denom,
            })
            .collect();
        vec![FrameMatches {
            points,
            rejected: Vec::new(),
            keypoints_left: n,
            keypoints_right: n,
            min_descriptors: n,
            post_ratio_test: n,
            post_spatial_filter: n,
            post_ransac: n,
            debug_frame: None,
        }]
    }

    #[test]
    fn debug_report_caps_points_and_flags_truncation() {
        // T-04-10: a pathological match set must not cross unbounded; the cap
        // holds and the report says so rather than silently dropping points.
        let frames = debug_frames(DEBUG_POINT_CAP + 500);
        let report = build_debug_report(&frames, None, 0.5);
        assert_eq!(report.verified.len(), DEBUG_POINT_CAP);
        assert!(report.points_capped);
    }

    #[test]
    fn debug_report_downscales_thumbnails_to_the_max_edge() {
        let (w, h) = (2000u32, 1000u32);
        let rgba = vec![128u8; (w * h * 4) as usize];
        let mut fm = frame_matches(1, 1, 1, 1, 1, 1);
        fm.debug_frame = Some(DebugFrame {
            left: rgba.clone(),
            left_width: w,
            left_height: h,
            right: rgba,
            right_width: w,
            right_height: h,
        });
        let report = build_debug_report(&[fm], None, 0.0);
        assert!(
            report.left_width.max(report.left_height) <= DEBUG_THUMB_MAX_EDGE,
            "thumbnail must be bounded: {}x{}",
            report.left_width,
            report.left_height
        );
        assert_eq!(
            report.left_thumb.len() as u32,
            report.left_width * report.left_height * 4,
            "thumbnail length must match its geometry"
        );
    }

    #[test]
    fn manual_frame_bound_caps_full_resolution_rgba() {
        // Regression (Phase 04.1 headless pass): the manual preview/validation
        // path must never cross the event channel at full resolution. A
        // 1920×1080 RGBA frame (8.29 MB) must be bounded to
        // `MANUAL_FRAME_MAX_EDGE` on its longest edge, with the returned
        // geometry still matching the byte length.
        let (w, h) = (1920u32, 1080u32);
        let src = vec![200u8; (w * h * 4) as usize];
        let (rgba, bw, bh) = downsample_rgba(&src, w, h, MANUAL_FRAME_MAX_EDGE);
        assert!(
            bw.max(bh) <= MANUAL_FRAME_MAX_EDGE,
            "manual frame must be bounded to {MANUAL_FRAME_MAX_EDGE}px, got {bw}x{bh}"
        );
        assert_eq!(
            rgba.len() as u32,
            bw * bh * 4,
            "bounded frame length must match its geometry"
        );
        assert!(
            rgba.len() < src.len(),
            "a full-resolution frame must actually shrink"
        );
    }

    #[test]
    fn manual_frame_bound_leaves_a_small_frame_untouched() {
        // A frame already within the bound is copied unchanged, so the bound is
        // not a quality tax on the mock/small-frame paths.
        let (w, h) = (320u32, 180u32);
        let src = vec![7u8; (w * h * 4) as usize];
        let (rgba, bw, bh) = downsample_rgba(&src, w, h, MANUAL_FRAME_MAX_EDGE);
        assert_eq!((bw, bh), (w, h));
        assert_eq!(rgba, src);
    }

    #[test]
    fn manual_frame_bound_fails_closed_on_a_short_buffer() {
        // A malformed payload never crosses as a truncated frame; it becomes an
        // empty fail-closed result the frontend ignores.
        let (rgba, w, h) = downsample_rgba(&[0u8; 8], 1920, 1080, MANUAL_FRAME_MAX_EDGE);
        assert!(rgba.is_empty());
        assert_eq!((w, h), (0, 0));
    }

    #[test]
    fn debug_report_maps_points_into_the_unit_square() {
        let frames = debug_frames(10);
        let report = build_debug_report(&frames, None, 0.0);
        assert_eq!(report.verified.len(), 10);
        for p in &report.verified {
            assert!((0.0..=1.0).contains(&p.x_nx), "x_nx out of range: {p:?}");
            assert!((0.0..=1.0).contains(&p.y_nx), "y_nx out of range: {p:?}");
        }
    }

    #[test]
    fn debug_report_fails_closed_on_empty_input() {
        let report = build_debug_report(&[], None, 0.0);
        assert_eq!(report.frames_total, 0);
        assert!(report.left_thumb.is_empty());
        assert_eq!(report.left_width, 0);
        assert!(report.verified.is_empty());
        assert!(report.per_frame.is_empty());
        assert!(!report.points_capped);
    }

    #[test]
    fn debug_report_uses_the_run_residual_without_a_layout() {
        let frames = debug_frames(3);
        let report = build_debug_report(&frames, None, 0.25);
        assert!(
            report
                .verified
                .iter()
                .all(|p| (p.error - 0.25).abs() < 1e-12),
            "without a fit, points carry the run residual"
        );
    }

    #[test]
    fn debug_report_uses_per_point_residuals_with_a_layout() {
        let frames = debug_frames(5);
        let layout = PlaneLayout {
            camera_axis_offset: 0.25,
            intersect: 0.5,
            x_ty: 0.0,
            x_rz: 0.0,
            z_rx: 0.0,
            x_rx: 0.0,
            z_rz: 0.0,
        };
        let report = build_debug_report(&frames, Some(&layout), 0.25);
        assert_eq!(report.verified.len(), 5);
        assert!(
            report.verified.iter().all(|p| p.error.is_finite()),
            "per-point residuals must be finite"
        );
    }

    #[test]
    fn no_keypoints_cause_names_the_camera_and_frame_with_an_exposure_fix() {
        let diagnosis = diagnose_calibration_failure(
            &CalibrateError::NoKeypoints {
                camera: "right",
                frame_idx: 7,
            },
            CalibrationStage::FeatureMatching,
            &[],
        );
        assert!(
            diagnosis.cause.contains("right"),
            "cause must name the camera: {}",
            diagnosis.cause
        );
        assert!(
            diagnosis.cause.contains('7'),
            "cause must name the frame: {}",
            diagnosis.cause
        );
        let fix = diagnosis.fix.to_lowercase();
        assert!(
            fix.contains("exposure") || fix.contains("texture") || fix.contains("light"),
            "fix must point at exposure / feature texture: {}",
            diagnosis.fix
        );
        assert!(!diagnosis.cause.is_empty() && !diagnosis.fix.is_empty());
    }

    #[test]
    fn insufficient_matches_cause_includes_the_numbers_and_the_fix_names_the_causes() {
        let diagnosis = diagnose_calibration_failure(
            &CalibrateError::InsufficientMatches { got: 3, min: 20 },
            CalibrationStage::FeatureMatching,
            &[],
        );
        assert!(
            diagnosis.cause.contains('3') && diagnosis.cause.contains("20"),
            "cause must include got/min: {}",
            diagnosis.cause
        );
        let fix = diagnosis.fix.to_lowercase();
        assert!(
            fix.contains("overlap") || fix.contains("exposure") || fix.contains("rolling"),
            "fix must mention overlap / exposure / rolling shutter: {}",
            diagnosis.fix
        );
    }

    #[test]
    fn ransac_failed_names_geometric_verification_and_suggests_reducing_mismatched_regions() {
        let diagnosis = diagnose_calibration_failure(
            &CalibrateError::RansacFailed,
            CalibrationStage::Optimizing,
            &[],
        );
        assert!(
            diagnosis.cause.to_lowercase().contains("geometric"),
            "cause must name geometric verification: {}",
            diagnosis.cause
        );
        let fix = diagnosis.fix.to_lowercase();
        assert!(
            fix.contains("mismatch") || fix.contains("region") || fix.contains("roi"),
            "fix must suggest reducing mismatched regions / ROI: {}",
            diagnosis.fix
        );
    }

    #[test]
    fn optimizer_failed_names_convergence_and_suggests_sync_or_imu_seeds() {
        let diagnosis = diagnose_calibration_failure(
            &CalibrateError::OptimizerFailed { max_evals: 200 },
            CalibrationStage::Optimizing,
            &[],
        );
        assert!(
            diagnosis.cause.to_lowercase().contains("converge"),
            "cause must name convergence: {}",
            diagnosis.cause
        );
        let fix = diagnosis.fix.to_lowercase();
        assert!(
            fix.contains("sync") || fix.contains("imu"),
            "fix must suggest checking sync / IMU seeds: {}",
            diagnosis.fix
        );
    }

    #[test]
    fn no_usable_frames_names_the_cause_and_the_fix_mentions_overlap_or_lens_profile() {
        let diagnosis = diagnose_calibration_failure(
            &CalibrateError::NoUsableFrames,
            CalibrationStage::Undistorting,
            &[],
        );
        assert!(
            diagnosis
                .cause
                .to_lowercase()
                .contains("no frame pair produced usable matches"),
            "cause must name the run-level failure: {}",
            diagnosis.cause
        );
        let fix = diagnosis.fix.to_lowercase();
        assert!(
            fix.contains("overlap") || fix.contains("lens profile") || fix.contains("lens"),
            "fix must mention overlap / lens profile: {}",
            diagnosis.fix
        );
    }

    #[test]
    fn an_unmapped_variant_falls_back_to_the_typed_display_text_never_an_empty_cause() {
        let source = CalibrateError::InvalidConfig("seam_sigma must be > 0".to_string());
        let diagnosis = diagnose_calibration_failure(&source, CalibrationStage::Probing, &[]);
        assert_eq!(
            diagnosis.cause,
            source.to_string(),
            "an unmapped variant must surface the typed Display text verbatim"
        );
        assert!(
            diagnosis.cause.contains("seam_sigma"),
            "the raw reason must survive: {}",
            diagnosis.cause
        );
        assert!(
            !diagnosis.fix.is_empty(),
            "the fallback must still offer a generic next step"
        );
    }

    #[test]
    fn diagnosis_metrics_aggregate_exactly_over_a_known_frame_set() {
        let frames = [
            frame_matches(5, 100, 90, 40, 20, 5),
            frame_matches(3, 80, 120, 30, 15, 3),
        ];
        let diagnosis = diagnose_calibration_failure(
            &CalibrateError::NoUsableFrames,
            CalibrationStage::Undistorting,
            &frames,
        );
        let m = diagnosis.metrics;
        assert_eq!(m.frames_used, 2);
        assert_eq!(m.total_matches, 8);
        assert_eq!(m.post_ratio_test, 70);
        assert_eq!(m.post_spatial_filter, 35);
        assert_eq!(m.post_ransac, 8);
        // Keypoint figures are the minimum across the frames.
        assert_eq!(m.keypoints_left, 80);
        assert_eq!(m.keypoints_right, 90);
    }

    fn sample_field_roi() -> reco_core::calibration::FieldRoi {
        reco_core::calibration::FieldRoi {
            left: vec![[0.1, 0.9], [0.3, 0.7], [0.5, 0.6]],
            right: vec![[0.6, 0.9], [0.8, 0.7], [0.7, 0.6]],
        }
    }

    #[test]
    fn build_manual_match_calibration_preserves_the_carried_fields() {
        // MANU-07 / T-04.1-18: every carried profile field survives the manual
        // assembly verbatim — never dropped, never re-derived.
        let mut base = sample_calibration();
        base.field_roi = Some(sample_field_roi());
        base.lens_correction_amount = 0.6;
        base.blend_width = 0.11;
        base.rig_tilt = 0.05;
        base.rig_roll = -0.02;

        let cal = build_manual_match_calibration(
            &base,
            camera(1920, 1080),
            camera(1920, 1080),
            base.layout.clone(),
            7,
        );
        assert_eq!(
            cal.field_roi, base.field_roi,
            "field_roi must survive the manual assembly (T-04.1-18)"
        );
        assert!((cal.lens_correction_amount - 0.6).abs() < 1e-9);
        assert!((cal.blend_width - 0.11).abs() < 1e-9);
        assert!((cal.rig_tilt - 0.05).abs() < 1e-9);
        assert!((cal.rig_roll + 0.02).abs() < 1e-9);
        assert_eq!(cal.sync_offset, 7, "the chosen offset must be carried");
        cal.validate()
            .expect("a sane manual assembly must pass MatchCalibration::validate");
    }

    #[test]
    fn build_manual_match_calibration_fails_validation_for_a_non_finite_k1() {
        // MANU-07 / T-04.1-16: an invalid profile must fail `validate` so the
        // save gate can refuse to write it. `validate` enforces finiteness (not
        // magnitude) for the distortion coefficients, so a NaN k1 is the
        // out-of-range case that trips it.
        let base = sample_calibration();
        let mut left = camera(1920, 1080);
        left.d[0] = f64::NAN;
        let cal =
            build_manual_match_calibration(&base, left, camera(1920, 1080), base.layout.clone(), 0);
        assert!(
            cal.validate().is_err(),
            "a non-finite k1 must fail MatchCalibration::validate"
        );
    }

    #[test]
    fn validation_verdict_passes_on_agreement_and_flags_a_disagreement() {
        // MANU-07: the verdict is advisory — LooksGood only when the layouts
        // agree within tolerance AND the residual is low; otherwise CheckSeam.
        let layout = sample_calibration().layout;
        assert_eq!(
            validation_verdict(&layout, &layout, 0.25),
            ValidationVerdict::LooksGood
        );
        assert_eq!(
            validation_verdict(&layout, &layout, 0.9),
            ValidationVerdict::CheckSeam,
            "a high residual must flag CheckSeam"
        );

        let mut other = layout.clone();
        other.camera_axis_offset += 0.2;
        assert_eq!(
            validation_verdict(&layout, &other, 0.1),
            ValidationVerdict::CheckSeam,
            "a layout disagreement must flag CheckSeam"
        );
        assert_eq!(
            validation_verdict(&layout, &layout, f64::NAN),
            ValidationVerdict::CheckSeam,
            "a non-finite residual can never pass"
        );
    }

    #[test]
    fn saving_the_manual_profile_twice_overwrites_the_target() {
        // MANU-07 idempotency: a repeated save overwrites the target profile; it
        // never accumulates.
        let base = sample_calibration();
        let first = build_manual_match_calibration(
            &base,
            camera(1920, 1080),
            camera(1920, 1080),
            base.layout.clone(),
            1,
        );
        let mut second = build_manual_match_calibration(
            &base,
            camera(1920, 1080),
            camera(1920, 1080),
            base.layout.clone(),
            2,
        );
        second.layout.intersect = 0.42;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "reco-manual-save-{}-{nanos}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        first.to_file(&path).expect("the first save must write");
        second
            .to_file(&path)
            .expect("the second save must overwrite");
        let loaded = MatchCalibration::from_file(&path).expect("the overwritten profile must load");
        assert_eq!(
            loaded.sync_offset, 2,
            "the second save must win, not accumulate"
        );
        assert!((loaded.layout.intersect - 0.42).abs() < 1e-9);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn project_intrinsics_refinement_maps_accept_and_reject_verbatim() {
        // INTR-03: an accepted refinement carries the real new k1, the baseline
        // (for `old → new`), and the held-out delta — nothing is fabricated.
        let accepted = IntrinsicsRefinement {
            k1: 0.21,
            residual: 0.5,
            accepted: true,
            reason: RefinementReason::Accepted,
            heldout_baseline: 1.25,
            heldout_refined: 0.75,
        };
        let view = project_intrinsics_refinement(&accepted, 0.10);
        assert_eq!(view.k1, 0.21);
        assert_eq!(view.baseline_k1, 0.10);
        assert!(view.accepted);
        assert_eq!(view.heldout_baseline, Some(1.25));
        assert_eq!(view.heldout_refined, Some(0.75));
        assert_eq!(view.reason, "Refinement accepted.");

        // A guard rejection returns the baseline k1 (never a new value) plus the
        // engine-authored user-facing reason.
        let rejected = IntrinsicsRefinement {
            k1: 0.10,
            residual: 1.0,
            accepted: false,
            reason: RefinementReason::GuardRejected,
            heldout_baseline: 1.25,
            heldout_refined: 1.30,
        };
        let view = project_intrinsics_refinement(&rejected, 0.10);
        assert_eq!(
            view.k1, 0.10,
            "a rejection must show the unchanged baseline k1"
        );
        assert_eq!(view.baseline_k1, 0.10);
        assert!(!view.accepted);
        assert!(
            view.reason.contains("did not improve"),
            "reason: {}",
            view.reason
        );
        assert_eq!(view.heldout_baseline, Some(1.25));
        assert_eq!(view.heldout_refined, Some(1.30));
    }

    #[test]
    fn project_intrinsics_refinement_never_fabricates_a_held_out_zero() {
        // INTR-03 edge probe "reject is visible": a conditioning refusal
        // evaluates no held-out split, so the residuals are `None` (rendered
        // "Not reported"), never a fabricated 0.0, and the profile k1 is shown
        // unchanged.
        for reason in [
            RefinementReason::InsufficientMatches,
            RefinementReason::NotEnoughSpread,
            RefinementReason::IllConditioned,
        ] {
            let refusal = IntrinsicsRefinement {
                k1: 0.10,
                residual: 0.0,
                accepted: false,
                reason,
                heldout_baseline: 0.0,
                heldout_refined: 0.0,
            };
            let view = project_intrinsics_refinement(&refusal, 0.10);
            assert!(!view.accepted, "reason {reason:?}");
            assert_eq!(view.heldout_baseline, None, "reason {reason:?}");
            assert_eq!(view.heldout_refined, None, "reason {reason:?}");
            assert_eq!(view.k1, 0.10, "reason {reason:?}");
            assert!(!view.reason.is_empty(), "reason {reason:?}");
        }
    }
}
