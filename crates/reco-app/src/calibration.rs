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

use reco_calibrate::types::{
    CalibrationConfig, CalibrationResult, CalibrationStep, LensProfileInfo, ProfileSource,
    SyncMethod as EngineSyncMethod,
};
use reco_core::calibration::CameraParams;

use crate::events::{
    CalibrationOptions, CalibrationStage, CompatibilityCode, CompatibilityIssue, ConfidenceBand,
    InputMetadata, LensProfileView, Scorecard, SyncMethod, SyncView,
};

/// The advisory compatibility findings for the two selected inputs (IMPT-03).
///
/// The checks are deliberately **advisory** (D3-05): a finding is reported as a
/// non-blocking warning, never a hard block. Overlap is never computed here — it
/// is **unknown until calibration** and is not faked with a sampling pre-pass
/// (D3-06).
///
/// Covers resolution, aspect (tolerance ~1%), frame rate (>0.5 fps), codec
/// (both present and different), and the same-file guard. A
/// `LensResolutionMismatch` is emitted by the worker when a lens override's
/// resolution differs from the input's — that comparison needs the override,
/// which this function does not receive.
pub fn check_compatibility(
    left: &InputMetadata,
    right: &InputMetadata,
    left_path: &str,
    right_path: &str,
) -> Vec<CompatibilityIssue> {
    let mut issues = Vec::new();

    // Same-file guard: the only check shown even though it is almost always
    // wrong; still non-blocking (D3-05). An empty path is a not-yet-filled slot
    // and never equals another empty path for this purpose.
    if !left_path.is_empty() && left_path == right_path {
        issues.push(CompatibilityIssue {
            code: CompatibilityCode::SameFile,
            message: format!(
                "Camera A and Camera B point at the same file ({left_path}); they must be different clips"
            ),
        });
    }

    if let (Some((lw, lh)), Some((rw, rh))) = (parse_resolution(left), parse_resolution(right))
        && (lw, lh) != (rw, rh)
    {
        issues.push(CompatibilityIssue {
            code: CompatibilityCode::ResolutionMismatch,
            message: format!("Camera A is {lw}×{lh} but Camera B is {rw}×{rh}"),
        });
        // Aspect ratio, tolerance ~1% of the larger aspect.
        let left_aspect = lw as f64 / lh as f64;
        let right_aspect = rw as f64 / rh as f64;
        let tolerance = 0.01 * left_aspect.max(right_aspect);
        if (left_aspect - right_aspect).abs() > tolerance {
            issues.push(CompatibilityIssue {
                code: CompatibilityCode::AspectMismatch,
                message: format!(
                    "Camera A and Camera B have different aspect ratios ({left_aspect:.3} vs {right_aspect:.3})"
                ),
            });
        }
    }

    if let (Some(lfps), Some(rfps)) = (parse_fps(left), parse_fps(right))
        && (lfps - rfps).abs() > 0.5
    {
        issues.push(CompatibilityIssue {
            code: CompatibilityCode::FpsMismatch,
            message: format!("Camera A runs at {lfps} fps but Camera B runs at {rfps} fps"),
        });
    }

    if let (Some(lcodec), Some(rcodec)) = (codec_name(left), codec_name(right))
        && lcodec != rcodec
    {
        issues.push(CompatibilityIssue {
            code: CompatibilityCode::CodecMismatch,
            message: format!(
                "Camera A is {lcodec} but Camera B is {rcodec}; they should use the same codec"
            ),
        });
    }

    issues
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
        CalibrationConfig, CalibrationResult, LensProfileInfo, ProfileSource, SyncInfo,
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
    fn different_resolutions_produce_a_resolution_mismatch() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let right = metadata("3840×2160", "30 fps", Some("h264"));
        let issues = check_compatibility(&left, &right, "/a.mp4", "/b.mp4");
        assert!(
            issues
                .iter()
                .any(|i| i.code == CompatibilityCode::ResolutionMismatch),
            "expected a ResolutionMismatch, got {issues:?}"
        );

        let same = check_compatibility(&left, &left.clone(), "/a.mp4", "/b.mp4");
        assert!(
            same.iter()
                .all(|i| i.code != CompatibilityCode::ResolutionMismatch),
            "identical resolutions must not report a mismatch: {same:?}"
        );
    }

    #[test]
    fn the_same_path_for_both_roles_produces_exactly_one_same_file_issue() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let issues = check_compatibility(&left, &left.clone(), "/a.mp4", "/a.mp4");
        let same_file: Vec<_> = issues
            .iter()
            .filter(|i| i.code == CompatibilityCode::SameFile)
            .collect();
        assert_eq!(
            same_file.len(),
            1,
            "expected exactly one SameFile: {issues:?}"
        );
    }

    #[test]
    fn fps_differing_by_more_than_half_a_frame_is_a_mismatch() {
        let left = metadata("1920×1080", "30 fps", Some("h264"));
        let right = metadata("1920×1080", "60 fps", Some("h264"));
        assert!(
            check_compatibility(&left, &right, "/a.mp4", "/b.mp4")
                .iter()
                .any(|i| i.code == CompatibilityCode::FpsMismatch),
            "30 vs 60 fps must report an FpsMismatch"
        );

        // Within tolerance: no issue.
        let near = metadata("1920×1080", "30.2 fps", Some("h264"));
        assert!(
            check_compatibility(&left, &near, "/a.mp4", "/b.mp4")
                .iter()
                .all(|i| i.code != CompatibilityCode::FpsMismatch),
            "30 vs 30.2 fps is within tolerance"
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
}
