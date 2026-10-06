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
use reco_calibrate::types::{
    CalibrationConfig, CalibrationResult, CalibrationStep, FrameMatches, LensProfileInfo,
    ProfileSource, SyncMethod as EngineSyncMethod,
};
use reco_core::calibration::CameraParams;

use crate::events::{
    CalibrationDiagnosis, CalibrationOptions, CalibrationStage, ConfidenceBand, DiagnosisMetrics,
    InputMetadata, LensProfileView, ReadinessCode, ReadinessFinding, ReadinessReport,
    ReadinessSeverity, Scorecard, SyncMethod, SyncView,
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
        sync: SyncView {
            method: map_sync_method(result.sync.method),
            confidence: result.sync.confidence,
        },
    }
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
        CalibrationConfig, CalibrationResult, FrameMatches, LensProfileInfo, MatchedPoint,
        ProfileSource, SyncInfo,
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
            keypoints_left,
            keypoints_right,
            min_descriptors: 0,
            post_ratio_test,
            post_spatial_filter,
            post_ransac,
        }
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
}
