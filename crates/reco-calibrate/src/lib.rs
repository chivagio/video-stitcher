//! Stereo camera calibration for reco.
//!
//! # Safety policy
//!
//! This crate deny's `unsafe_code` by default. The only existing
//! exceptions are SIMD hot paths inside the AKAZE feature detector
//! (`akaze/nonlinear_diffusion.rs`, `akaze/derivatives.rs`) which
//! carry targeted `#[allow(unsafe_code)]` annotations with SAFETY
//! comments. Any new `unsafe` in this crate must follow that pattern:
//! narrow scope, justified in a comment, reviewed in the PR.

#![deny(unsafe_code)]

//!
//! Computes the relative positioning of two camera planes by detecting
//! features in overlapping footage and optimizing placement parameters
//! to minimize reprojection error between matched points.
//!
//! ## Pipeline
//!
//! ```text
//! Frame pairs -> GPU Undistort -> AKAZE Detect -> Descriptor Match
//!   -> Spatial + RANSAC Filter -> Nelder-Mead Optimizer -> PlaneLayout
//! ```
//!
//! Each stage also has a trait interface ([`traits`]) with default
//! implementations in [`defaults`] for standalone use.
//!
//! ## Full pipeline usage
//!
//! ```ignore
//! use reco_calibrate::{calibrate, CalibrationConfig};
//! use reco_core::calibration::CameraParams;
//!
//! let result = calibrate(&gpu, &frames, &left_params, &right_params, &CalibrationConfig::default())?;
//! println!("Confidence: {:.1}%", result.confidence * 100.0);
//! ```
//!
//! ## Custom pipeline stages
//!
//! Use [`calibrate_with`] to plug in custom detector, matcher, or filter
//! implementations:
//!
//! ```ignore
//! use reco_calibrate::{calibrate_with, AkazeDetector, HammingMatcher, YDisparityFilter};
//!
//! let result = calibrate_with(
//!     &gpu, &frames, &left_params, &right_params, &config,
//!     &AkazeDetector::new(0.001),
//!     &HammingMatcher::new(0.6),
//!     &YDisparityFilter::default(),
//! )?;
//! ```

// `profile_scope!` is defined and exported by `reco_core`. Using it here
// via `reco_core::profile_scope!` avoids maintaining a local copy. When
// the `profiling` feature is enabled, `reco-core/profiling` is also
// activated (see Cargo.toml).
use reco_core::profile_scope;

pub(crate) mod akaze;
pub mod audio_sync;
pub mod defaults;
pub mod error;
pub mod features;
pub mod filter;
pub mod geometry;
pub mod intrinsics;
pub mod lens_database;
/// M6 live calibration — drive the calibration pipeline from a live
/// frame-pair source (OBS, V4L2, WebRTC, etc.). See the `live` module's
/// `calibrate_from_live` function and `LiveFramePairSource` trait. Does
/// not require the `io` feature — the source is a trait object supplied
/// by the consumer, not a file.
pub mod live;
pub mod manual;
pub mod optimizer;
pub mod pipeline;
mod ransac;
pub mod sampling;
pub mod telemetry;
pub mod traits;
pub mod types;
#[cfg(feature = "io")]
pub mod video;

pub use defaults::{
    AkazeDetector, HammingMatcher, NoOpFilter, RawReprojectionCost, SeamWeightedCost,
    YDisparityFilter,
};
pub use error::{CalibrateError, CalibrationFailure};
pub use intrinsics::{
    Conditioning, IntrinsicsConfig, IntrinsicsRefinement, RawPixelMatch, RefinementReason,
    conditioning, optimize_intrinsics, raw_to_matched_point, refine_intrinsics,
};
pub use manual::{ManualPin, ManualSolveResult, pin_to_matched_point, solve_manual_calibration};
pub use traits::{CostFunction, FeatureDetector, FeatureMatcher, PointFilter};
pub use types::{
    AkazeConfig, CalibrationConfig, CalibrationProgress, CalibrationQuality, CalibrationResult,
    CalibrationStep, DebugFrame, GrayFrame, LensProfileInfo, LensProfileSummary, MatchConfig,
    OptimizerConfig, ProfileSource, YuvFrame,
};

use reco_core::calibration::{CameraParams, MatchCalibration};
use reco_core::gpu::GpuContext;
use reco_core::lens::undistort::GpuUndistort;

use types::{FrameMatches, MatchedPoint};

/// Number of total matched points at which calibration confidence reaches 1.0.
///
/// Confidence is computed as `min(total_matches / FULL_CONFIDENCE_MATCHES, 1.0)`.
/// With 50 matches, confidence saturates at 100%. Fewer matches reduce
/// confidence linearly (e.g. 25 matches = 50% confidence). This threshold
/// is empirically chosen: 50 well-distributed matches across multiple frames
/// reliably produce sub-pixel calibration.
const FULL_CONFIDENCE_MATCHES: f64 = 50.0;

/// Frame rate assumed by the free calibration entry points that carry no
/// video metadata. Only used by the rolling-shutter row-dependent bound, which
/// is inert unless [`MatchConfig::rolling_shutter`] is explicitly enabled; the
/// [`CalibrationPipeline`](crate::pipeline::CalibrationPipeline) path threads
/// the probed `VideoInfo::fps` instead.
const DEFAULT_FPS: f64 = 30.0;

/// Fraction of [`MatchConfig::exposure_target`] below which a frame is
/// considered too dark to exposure-normalize (guards uniform-black frames).
const EXPOSURE_DARK_FRACTION: f64 = 0.01;

/// Maximum number of pixels sampled when computing exposure statistics.
///
/// Bounds the cost of the statistics pass on large frames (T-04-04).
const EXPOSURE_SAMPLE_BUDGET: usize = 65_536;

/// Lower bound on the per-frame exposure gain.
const EXPOSURE_GAIN_MIN: f64 = 0.25;
/// Upper bound on the per-frame exposure gain.
const EXPOSURE_GAIN_MAX: f64 = 4.0;

/// Convert an 8-bit sRGB channel to linear light (gamma 2.2 approximation).
fn srgb_to_linear(v: u8) -> f64 {
    (v as f64 / 255.0).powf(2.2)
}

/// Convert a linear-light value back to an 8-bit sRGB channel.
fn linear_to_srgb_byte(x: f64) -> u8 {
    (x.max(0.0).powf(1.0 / 2.2) * 255.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Mean linear luminance of an RGBA frame, from a bounded downsample.
///
/// Returns `0.0` for a zero-sized or undersized buffer. Samples at most
/// [`EXPOSURE_SAMPLE_BUDGET`] pixels so the cost stays bounded on 4K frames.
fn mean_linear_luminance(rgba: &[u8], w: u32, h: u32) -> f64 {
    let pixels = w as u64 * h as u64;
    if pixels == 0 || (rgba.len() as u64) < pixels * 4 {
        return 0.0;
    }
    let pixels = pixels as usize;
    let step = (pixels / EXPOSURE_SAMPLE_BUDGET).max(1);
    let mut sum = 0.0;
    let mut count = 0usize;
    let mut i = 0usize;
    while i < pixels {
        let idx = i * 4;
        let r = srgb_to_linear(rgba[idx]);
        let g = srgb_to_linear(rgba[idx + 1]);
        let b = srgb_to_linear(rgba[idx + 2]);
        // Rec. 709 luma weights on linearized channels.
        sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
        count += 1;
        i += step;
    }
    if count == 0 { 0.0 } else { sum / count as f64 }
}

/// Scale the RGB channels of an RGBA buffer by `gain` in linear light.
///
/// Uses a 256-entry transfer LUT so the inner loop is a table lookup rather
/// than a `powf` per pixel. Alpha is left untouched.
fn apply_exposure_gain(rgba: &mut [u8], w: u32, h: u32, gain: f64) {
    if (gain - 1.0).abs() < f64::EPSILON {
        return;
    }
    let mut lut = [0u8; 256];
    for (v, out) in lut.iter_mut().enumerate() {
        *out = linear_to_srgb_byte(srgb_to_linear(v as u8) * gain);
    }
    let pixels = (w as u64 * h as u64) as usize;
    let limit = pixels.min(rgba.len() / 4);
    for i in 0..limit {
        let idx = i * 4;
        rgba[idx] = lut[rgba[idx] as usize];
        rgba[idx + 1] = lut[rgba[idx + 1] as usize];
        rgba[idx + 2] = lut[rgba[idx + 2] as usize];
        // alpha (idx + 3) untouched
    }
}

/// Exposure-normalize an undistorted RGBA frame pair in linear light (CALB-07).
///
/// Computes each frame's mean linear luminance from a bounded downsample and
/// scales both frames toward their shared average, so two clips with different
/// exposure converge to a comparable level. Identical frames receive a unit
/// gain and are left numerically unchanged. A frame whose mean luminance is
/// below a small fraction of [`MatchConfig::exposure_target`] (e.g. a
/// uniform-black frame) causes the whole pair to pass through unchanged,
/// avoiding division by zero and amplification of near-zero noise. RGB channels
/// are scaled; alpha is untouched. Disabled entirely when
/// [`MatchConfig::exposure_normalize`] is `false`.
#[allow(clippy::too_many_arguments)]
fn normalize_exposure(
    left: &mut [u8],
    right: &mut [u8],
    lw: u32,
    lh: u32,
    rw: u32,
    rh: u32,
    cfg: &MatchConfig,
) {
    if !cfg.exposure_normalize {
        return;
    }
    let mean_left = mean_linear_luminance(left, lw, lh);
    let mean_right = mean_linear_luminance(right, rw, rh);
    let floor = (cfg.exposure_target * EXPOSURE_DARK_FRACTION).max(f64::EPSILON);
    if mean_left < floor || mean_right < floor {
        return;
    }
    // Equalize toward the pair's average luminance. When the frames are
    // identical the shared level equals each mean, so the gain is exactly 1.0.
    let shared = 0.5 * (mean_left + mean_right);
    let gain_left = (shared / mean_left).clamp(EXPOSURE_GAIN_MIN, EXPOSURE_GAIN_MAX);
    let gain_right = (shared / mean_right).clamp(EXPOSURE_GAIN_MIN, EXPOSURE_GAIN_MAX);
    apply_exposure_gain(left, lw, lh, gain_left);
    apply_exposure_gain(right, rw, rh, gain_right);
}

/// Process an undistorted RGBA frame pair through the feature matching pipeline.
///
/// Takes pre-undistorted RGBA data (from GPU phase) and runs feature
/// detection, matching, and filtering using the provided trait objects.
/// This function is thread-safe and called in parallel via rayon.
#[allow(clippy::too_many_arguments)]
fn process_undistorted_pair(
    left_rgba: &[u8],
    right_rgba: &[u8],
    lw: u32,
    lh: u32,
    rw: u32,
    rh: u32,
    frame_idx: usize,
    fps: f64,
    config: &CalibrationConfig,
    detector: &dyn traits::FeatureDetector,
    matcher: &dyn traits::FeatureMatcher,
    point_filter: &dyn traits::PointFilter,
) -> Option<FrameMatches> {
    profile_scope!("process_frame");
    let inner = config.matching.spatial_x_inner as f32;
    let y_min = config.akaze.detect_y_min as f32;
    let y_max = config.akaze.detect_y_max as f32;
    let left_region = features::DetectRegion {
        x_min: config.matching.spatial_x_threshold as f32,
        x_max: 1.0 - inner,
        y_min,
        y_max,
    };
    let right_region = features::DetectRegion {
        x_min: inner,
        x_max: 1.0 - config.matching.spatial_x_threshold as f32,
        y_min,
        y_max,
    };

    // Detect features on left and right concurrently (independent CPU work)
    let ((kp_left, desc_left), (kp_right, desc_right)) = rayon::join(
        || {
            profile_scope!("akaze_detect_left");
            detector.detect(
                left_rgba,
                lw,
                lh,
                Some(left_region),
                config.akaze.max_keypoints,
            )
        },
        || {
            profile_scope!("akaze_detect_right");
            detector.detect(
                right_rgba,
                rw,
                rh,
                Some(right_region),
                config.akaze.max_keypoints,
            )
        },
    );

    log::debug!(
        "frame {frame_idx}: {} left keypoints, {} right keypoints",
        kp_left.len(),
        kp_right.len()
    );

    if kp_left.is_empty() || kp_right.is_empty() {
        log::warn!("frame {frame_idx}: no keypoints in one or both images");
        return None;
    }

    // Match descriptors using the provided matcher, config-aware when the
    // matcher supports it (adaptive ratio + multi-scale, CALB-07).
    let raw_matches = matcher.match_features_with_config(&desc_left, &desc_right, &config.matching);
    let post_ratio_test = raw_matches.len();

    if raw_matches.len() < config.matching.min_matches {
        log::debug!(
            "frame {frame_idx}: only {} matches after ratio test (need {})",
            raw_matches.len(),
            config.matching.min_matches
        );
        return None;
    }

    // Spatial overlap filter
    let spatial_matches = filter::spatial_filter(
        &raw_matches,
        &kp_left,
        &kp_right,
        lw,
        lh,
        rw,
        rh,
        fps,
        config,
    );
    let post_spatial_filter = spatial_matches.len();

    // RANSAC outlier rejection
    let inlier_indices = match filter::ransac_filter(&spatial_matches, &kp_left, &kp_right, config)
    {
        Ok(indices) => indices,
        Err(e) => {
            log::debug!("frame {frame_idx}: RANSAC failed: {e}");
            return None;
        }
    };
    let post_ransac = inlier_indices.len();

    if post_ransac < config.matching.min_matches {
        log::debug!(
            "frame {frame_idx}: only {} inliers after RANSAC (need {})",
            post_ransac,
            config.matching.min_matches
        );
        return None;
    }

    // Normalize surviving matches to plane coordinates.
    //
    // Features were detected on the undistorted image (using original
    // intrinsics), which maps 1:1 to the GPU shader's plane UV space.
    // Linear normalization to [-0.5, 0.5] gives plane coordinates
    // directly - no KB4 remapping needed.
    //
    // CRITICAL: Apply the left/right swap from v1 (processing.py:693).
    // Right camera points -> left plane (x-plane) in optimizer space.
    // Left camera points -> right plane (z-plane) in optimizer space.
    //
    // `matched_point` is shared with the rejected-point collection below so the
    // two classes use one coordinate convention (CALB-08).
    let matched_point = |m: &features::RawMatch| -> MatchedPoint {
        let lp = &kp_left[m.left_idx];
        let rp = &kp_right[m.right_idx];
        MatchedPoint {
            left: geometry::normalize_to_plane(rp.x as f64, rp.y as f64, rw, rh),
            right: geometry::normalize_to_plane(lp.x as f64, lp.y as f64, lw, lh),
            // Store normalized pixel x for seam-proximity weighting
            left_pixel_nx: rp.x as f64 / rw as f64,
            right_pixel_nx: lp.x as f64 / lw as f64,
        }
    };

    let points: Vec<MatchedPoint> = inlier_indices
        .iter()
        .map(|&i| matched_point(&spatial_matches[i]))
        .collect();

    // Retain the rejected candidates for the debug inspector (CALB-08): the
    // ratio-test survivors the spatial filter dropped, plus the spatial
    // survivors RANSAC rejected. Diagnostic only — never fed to the optimizer.
    let inlier_set: std::collections::HashSet<usize> = inlier_indices.iter().copied().collect();
    let spatial_keys: std::collections::HashSet<(usize, usize)> = spatial_matches
        .iter()
        .map(|m| (m.left_idx, m.right_idx))
        .collect();
    let mut rejected: Vec<MatchedPoint> = raw_matches
        .iter()
        .filter(|m| !spatial_keys.contains(&(m.left_idx, m.right_idx)))
        .map(&matched_point)
        .collect();
    rejected.extend(
        spatial_matches
            .iter()
            .enumerate()
            .filter(|(i, _)| !inlier_set.contains(i))
            .map(|(_, m)| matched_point(m)),
    );

    // Apply the user-provided point filter (e.g. y-disparity rejection)
    let points = point_filter.filter(&points);

    Some(FrameMatches {
        points,
        rejected,
        keypoints_left: kp_left.len(),
        keypoints_right: kp_right.len(),
        min_descriptors: desc_left.len().min(desc_right.len()),
        post_ratio_test,
        post_spatial_filter,
        post_ransac,
        debug_frame: None,
    })
}

/// Run the full calibration pipeline with default implementations.
///
/// Uses AKAZE detection, Hamming matching, and a no-op point filter
/// (spatial + RANSAC filters are always applied internally). For custom
/// pipeline stages, use [`calibrate_with`] instead.
///
/// Takes pre-extracted YUV frame pairs (left, right) along with camera
/// intrinsics from lens profiles. GPU-undistorts each frame to rectilinear
/// RGBA before detecting features.
///
/// # Errors
///
/// Returns [`CalibrateError::NoUsableFrames`] if no frame pairs produce
/// enough matches, or [`CalibrateError::OptimizerFailed`] if all
/// optimization iterations fail.
pub fn calibrate(
    gpu: &GpuContext,
    frames: &[(YuvFrame, YuvFrame)],
    left_params: &CameraParams,
    right_params: &CameraParams,
    config: &CalibrationConfig,
) -> Result<CalibrationResult, CalibrateError> {
    calibrate_with_progress(gpu, frames, left_params, right_params, config, &mut |_| {})
}

/// Run the full calibration pipeline with default stages, reporting
/// progress (CALB-01 / D3-09).
///
/// Like [`calibrate`], but invokes `on_progress` before each frame's GPU
/// undistort ([`CalibrationStep::Undistorting`]) and before the optimizer
/// solves ([`CalibrationStep::Optimizing`]). Together with the stages the
/// caller emits around frame extraction and matching, this makes all seven
/// [`CalibrationStep`] variants reachable from one file-to-calibration run.
pub fn calibrate_with_progress(
    gpu: &GpuContext,
    frames: &[(YuvFrame, YuvFrame)],
    left_params: &CameraParams,
    right_params: &CameraParams,
    config: &CalibrationConfig,
    on_progress: &mut dyn FnMut(&CalibrationProgress),
) -> Result<CalibrationResult, CalibrateError> {
    calibrate_with_progress_diagnostic(
        gpu,
        frames,
        left_params,
        right_params,
        config,
        DEFAULT_FPS,
        on_progress,
    )
    .map_err(|failure| failure.error)
}

/// Run the full calibration pipeline, returning a typed [`CalibrationFailure`]
/// on error (CALB-04).
///
/// Identical to [`calibrate_with_progress`], but the error carries the failing
/// [`CalibrationStep`] and the partial `FrameMatches` accumulated before the
/// failure instead of a bare [`CalibrateError`]. The host maps it to a
/// plain-language diagnosis; callers that only want the error use
/// [`calibrate_with_progress`].
///
/// `fps` is the source frame rate, threaded into the rolling-shutter
/// row-dependent vertical-disparity bound (CALB-07); it is only consulted when
/// [`MatchConfig::rolling_shutter`] is enabled.
#[allow(clippy::too_many_arguments)]
pub fn calibrate_with_progress_diagnostic(
    gpu: &GpuContext,
    frames: &[(YuvFrame, YuvFrame)],
    left_params: &CameraParams,
    right_params: &CameraParams,
    config: &CalibrationConfig,
    fps: f64,
    on_progress: &mut dyn FnMut(&CalibrationProgress),
) -> Result<CalibrationResult, CalibrationFailure> {
    let detector = defaults::AkazeDetector::new(config.akaze.threshold);
    let matcher = defaults::HammingMatcher::new(config.matching.lowe_ratio);
    let filter = defaults::NoOpFilter;
    calibrate_impl(
        gpu,
        frames,
        left_params,
        right_params,
        config,
        fps,
        &detector,
        &matcher,
        &filter,
        on_progress,
    )
}

/// Run the full calibration pipeline with custom pipeline stages.
///
/// Like [`calibrate`], but accepts trait objects for the feature detector,
/// matcher, and point filter stages. Spatial filtering and RANSAC are
/// always applied internally (they depend on raw keypoint coordinates
/// and image dimensions).
///
/// # Arguments
///
/// * `detector` - Feature detection (e.g. [`AkazeDetector`])
/// * `matcher` - Descriptor matching (e.g. [`HammingMatcher`])
/// * `filter` - Point filter applied after normalization to plane coordinates
///   (e.g. [`YDisparityFilter`], [`NoOpFilter`])
///
/// # Errors
///
/// Returns [`CalibrateError::NoUsableFrames`] if no frame pairs produce
/// enough matches, or [`CalibrateError::OptimizerFailed`] if all
/// optimization iterations fail.
#[allow(clippy::too_many_arguments)]
pub fn calibrate_with(
    gpu: &GpuContext,
    frames: &[(YuvFrame, YuvFrame)],
    left_params: &CameraParams,
    right_params: &CameraParams,
    config: &CalibrationConfig,
    detector: &dyn traits::FeatureDetector,
    matcher: &dyn traits::FeatureMatcher,
    point_filter: &dyn traits::PointFilter,
) -> Result<CalibrationResult, CalibrateError> {
    calibrate_impl(
        gpu,
        frames,
        left_params,
        right_params,
        config,
        DEFAULT_FPS,
        detector,
        matcher,
        point_filter,
        &mut |_| {},
    )
    .map_err(|failure| failure.error)
}

/// Shared implementation behind [`calibrate`], [`calibrate_with`], and
/// [`calibrate_with_progress`].
///
/// Emits [`CalibrationStep::Undistorting`] before each frame's GPU
/// undistort and [`CalibrationStep::Optimizing`] before the optimizer
/// solves through `on_progress`.
///
/// On failure it returns a [`CalibrationFailure`] carrying the failing step and
/// the partial `FrameMatches` so the host can explain the failure (CALB-04).
#[allow(clippy::too_many_arguments)]
fn calibrate_impl(
    gpu: &GpuContext,
    frames: &[(YuvFrame, YuvFrame)],
    left_params: &CameraParams,
    right_params: &CameraParams,
    config: &CalibrationConfig,
    fps: f64,
    detector: &dyn traits::FeatureDetector,
    matcher: &dyn traits::FeatureMatcher,
    point_filter: &dyn traits::PointFilter,
    on_progress: &mut dyn FnMut(&CalibrationProgress),
) -> Result<CalibrationResult, CalibrationFailure> {
    config
        .validate()
        .map_err(|e| CalibrationFailure::at_step(e, CalibrationStep::FeatureMatching))?;

    // Create GPU undistort pipelines for each camera's resolution
    let (lw, lh) = if let Some((left, _)) = frames.first() {
        (left.width, left.height)
    } else {
        return Err(CalibrationFailure::at_step(
            CalibrateError::NoUsableFrames,
            CalibrationStep::FeatureMatching,
        ));
    };
    let (rw, rh) = if let Some((_, right)) = frames.first() {
        (right.width, right.height)
    } else {
        return Err(CalibrationFailure::at_step(
            CalibrateError::NoUsableFrames,
            CalibrationStep::FeatureMatching,
        ));
    };

    // Validate that frame dimensions are nonzero to prevent division-by-zero
    // downstream (e.g., in normalize_to_plane, seam weight calculations).
    if lw == 0 || lh == 0 || rw == 0 || rh == 0 {
        log::error!(
            "invalid frame dimensions: left={}x{}, right={}x{}",
            lw,
            lh,
            rw,
            rh
        );
        return Err(CalibrationFailure::at_step(
            CalibrateError::NoUsableFrames,
            CalibrationStep::Undistorting,
        ));
    }
    let left_aspect = lw as f32 / lh as f32;
    let right_aspect = rw as f32 / rh as f32;
    let left_undistort = GpuUndistort::new(gpu, lw, lh, left_aspect);
    let right_undistort = GpuUndistort::new(gpu, rw, rh, right_aspect);

    // Process one frame pair at a time: undistort on GPU, detect+match
    // on CPU, then drop pixel data before the next pair. Keeps peak
    // memory proportional to one frame pair (~100 MB) instead of all
    // pairs (~1 GB for 8 pairs at 4K).
    let mut successful_frames: Vec<FrameMatches> = Vec::new();
    for (i, (left, right)) in frames.iter().enumerate() {
        on_progress(&CalibrationProgress {
            step: CalibrationStep::Undistorting,
            detail: format!("Undistorting frame {}/{}", i + 1, frames.len()),
        });
        let (mut left_rgba, mut right_rgba) = {
            profile_scope!("gpu_undistort");
            let l = left_undistort.undistort(gpu, &left.y, &left.u, &left.v, left_params);
            let r = right_undistort.undistort(gpu, &right.y, &right.u, &right.v, right_params);
            (l, r)
        };
        {
            profile_scope!("exposure_normalize");
            normalize_exposure(
                &mut left_rgba,
                &mut right_rgba,
                lw,
                lh,
                rw,
                rh,
                &config.matching,
            );
        }
        let mut result = {
            profile_scope!("akaze_detect_match");
            process_undistorted_pair(
                &left_rgba,
                &right_rgba,
                lw,
                lh,
                rw,
                rh,
                i,
                fps,
                config,
                detector,
                matcher,
                point_filter,
            )
        };
        // Retain the undistorted RGBA pair of the FIRST frame that produced
        // matches, for the debug inspector (CALB-08). Only one pair is kept so
        // peak memory stays bounded (T-04-10). The borrow of `left_rgba`/
        // `right_rgba` by `process_undistorted_pair` has ended, so they can be
        // moved into the retained frame.
        if let Some(fm) = result.as_mut()
            && successful_frames.is_empty()
        {
            fm.debug_frame = Some(types::DebugFrame {
                left: std::mem::take(&mut left_rgba),
                left_width: lw,
                left_height: lh,
                right: std::mem::take(&mut right_rgba),
                right_width: rw,
                right_height: rh,
            });
        }
        if let Some(fm) = result {
            successful_frames.push(fm);
        }
    }

    if successful_frames.is_empty() {
        // Every frame was undistorted and matched before this point; the last
        // active stage is FeatureMatching, so attribute the empty result there
        // rather than to Undistorting (WR-03).
        return Err(CalibrationFailure::new(
            CalibrateError::NoUsableFrames,
            CalibrationStep::FeatureMatching,
            successful_frames,
        ));
    }

    let frames_used = successful_frames.len();
    log::info!(
        "{frames_used}/{} frame pairs produced matches",
        frames.len()
    );

    // Accumulate all matched points across frames
    let all_points: Vec<MatchedPoint> = successful_frames
        .iter()
        .flat_map(|fm| fm.points.iter().copied())
        .collect();

    let total_matches = all_points.len();
    log::info!("{total_matches} total matched points");

    // Log spatial distribution of matches for diagnostics
    if !all_points.is_empty() {
        let lx: Vec<f64> = all_points.iter().map(|p| p.left[0]).collect();
        let ly: Vec<f64> = all_points.iter().map(|p| p.left[1]).collect();
        let rx: Vec<f64> = all_points.iter().map(|p| p.right[0]).collect();
        let ry: Vec<f64> = all_points.iter().map(|p| p.right[1]).collect();
        log::info!(
            "x-plane range: x=[{:.3}, {:.3}] y=[{:.3}, {:.3}]",
            lx.iter().cloned().fold(f64::INFINITY, f64::min),
            lx.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            ly.iter().cloned().fold(f64::INFINITY, f64::min),
            ly.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        );
        log::info!(
            "z-plane range: x=[{:.3}, {:.3}] y=[{:.3}, {:.3}]",
            rx.iter().cloned().fold(f64::INFINITY, f64::min),
            rx.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            ry.iter().cloned().fold(f64::INFINITY, f64::min),
            ry.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        );
    }

    if total_matches < config.matching.min_matches {
        // Matching produced too few points — a FeatureMatching failure, not an
        // Undistorting one (WR-03).
        return Err(CalibrationFailure::new(
            CalibrateError::InsufficientMatches {
                got: total_matches,
                min: config.matching.min_matches,
            },
            CalibrationStep::FeatureMatching,
            successful_frames,
        ));
    }

    // Single-pass optimization on all points with trimmed cost.
    on_progress(&CalibrationProgress {
        step: CalibrationStep::Optimizing,
        detail: "Optimizing camera parameters".to_string(),
    });
    let (best_layout, best_residual) = {
        profile_scope!("optimizer");
        optimizer::optimize(&all_points, config)
    }
    .map_err(|e| {
        log::error!("optimization failed: {e}");
        CalibrationFailure::new(e, CalibrationStep::Optimizing, successful_frames.clone())
    })?;

    let confidence = (total_matches as f64 / FULL_CONFIDENCE_MATCHES).min(1.0);

    // Log both metrics for diagnostic comparison
    let best_params = geometry::OptParams {
        x_ty: best_layout.x_ty,
        intersect: best_layout.intersect,
        cam_d: best_layout.camera_axis_offset,
        x_rz: best_layout.x_rz,
        z_rx: best_layout.z_rx,
        z_rz: None,
        x_rx: None,
    };
    let total_reproj = geometry::reprojection_error(&all_points, &best_params);
    let angular_err = geometry::angular_error(&all_points, &best_params);
    let trimmed_err = geometry::trimmed_reprojection_error(&all_points, &best_params, 0.2);
    log::info!(
        "calibration complete: median_error={best_residual:.6}, trimmed={trimmed_err:.6}, \
         total_reproj={total_reproj:.6}, angular_error={angular_err:.6}, \
         confidence={confidence:.2}, z_rz={:.4}",
        best_layout.z_rz
    );

    let mut calibration = MatchCalibration {
        left: left_params.clone(),
        right: right_params.clone(),
        layout: best_layout,
        rig_tilt: 0.0, // set by CalibrationPipeline after calibrate()
        rig_roll: 0.0,
        sync_offset: 0,              // set by CalibrationPipeline after calibrate()
        field_roi: None,             // set manually or by a future field detection pipeline
        lens_correction_amount: 1.0, // full correction; user-tunable in the GUI
        blend_width: 0.05,           // renderer default; user-tunable in the GUI
    };
    // Guarantee the in-memory calibration is valid: the layout clamp covers the
    // solver's physical bounds, this covers the loader's positivity gates
    // (focal lengths, axis offset) for every solver mode.
    calibration.clamp_to_valid_ranges();

    Ok(CalibrationResult {
        calibration,
        total_matches,
        frames_used,
        residual_error: best_residual,
        confidence,
        per_frame: successful_frames,
        // Populated by `CalibrationPipeline::calibrate_with_progress`, which
        // owns the sampled indices; the free function has no pipeline context.
        frame_indices: Vec::new(),
        left_lens_profile: None,
        right_lens_profile: None,
        quality: Some(types::CalibrationQuality {
            mean_reprojection_error: total_reproj,
            trimmed_reprojection_error: trimmed_err,
            angular_error: angular_err,
        }),
        // Overwritten by `CalibrationPipeline::calibrate_with_progress` when a
        // pipeline drives this call; the free function has no sync context.
        sync: types::SyncInfo {
            method: types::SyncMethod::None,
            confidence: None,
            offset_frames: 0,
        },
    })
}

#[cfg(test)]
mod exposure_tests {
    use super::*;

    /// Build a tightly-packed RGBA buffer filled with one gray level.
    fn uniform_rgba(w: u32, h: u32, gray: u8) -> Vec<u8> {
        (0..w as usize * h as usize)
            .flat_map(|_| [gray, gray, gray, 255u8])
            .collect()
    }

    /// Mean linear luminance of an RGBA buffer, for assertions.
    fn mean_linear(rgba: &[u8]) -> f64 {
        let n = rgba.len() / 4;
        let sum: f64 = (0..n).map(|i| (rgba[i * 4] as f64 / 255.0).powf(2.2)).sum();
        sum / n as f64
    }

    #[test]
    fn normalizes_two_frames_toward_a_comparable_mean() {
        let (w, h) = (64u32, 64u32);
        let mut left = uniform_rgba(w, h, 100);
        let mut right = uniform_rgba(w, h, 200);
        let before = (mean_linear(&left) - mean_linear(&right)).abs();

        normalize_exposure(&mut left, &mut right, w, h, w, h, &MatchConfig::default());

        let after = (mean_linear(&left) - mean_linear(&right)).abs();
        assert!(
            after < before * 0.1,
            "means should converge: before={before:.4}, after={after:.4}"
        );
    }

    #[test]
    fn identical_frames_are_unchanged() {
        let (w, h) = (32u32, 32u32);
        let original = uniform_rgba(w, h, 128);
        let mut left = original.clone();
        let mut right = original.clone();

        normalize_exposure(&mut left, &mut right, w, h, w, h, &MatchConfig::default());

        assert_eq!(
            left, original,
            "unit gain must leave the left frame unchanged"
        );
        assert_eq!(
            right, original,
            "unit gain must leave the right frame unchanged"
        );
    }

    #[test]
    fn uniform_black_frame_is_passed_through() {
        let (w, h) = (32u32, 32u32);
        let mut left = uniform_rgba(w, h, 0);
        let mut right = uniform_rgba(w, h, 180);
        let left_before = left.clone();
        let right_before = right.clone();

        // Must not divide by zero; the black frame stays black.
        normalize_exposure(&mut left, &mut right, w, h, w, h, &MatchConfig::default());

        assert_eq!(left, left_before, "black frame must pass through unchanged");
        assert_eq!(
            right, right_before,
            "no normalization when a frame is too dark to trust"
        );
    }

    #[test]
    fn disabled_normalization_leaves_frames_untouched() {
        let (w, h) = (32u32, 32u32);
        let mut left = uniform_rgba(w, h, 40);
        let mut right = uniform_rgba(w, h, 220);
        let left_before = left.clone();
        let right_before = right.clone();
        let cfg = MatchConfig {
            exposure_normalize: false,
            ..Default::default()
        };

        normalize_exposure(&mut left, &mut right, w, h, w, h, &cfg);

        assert_eq!(left, left_before);
        assert_eq!(right, right_before);
    }
}
