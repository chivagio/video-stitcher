//! One-call calibration from video files.
//!
//! Available when the `io` feature is enabled. Wraps the full
//! calibration pipeline with reco-io's FFmpeg-based I/O into a
//! single function call.
//!
//! For live/in-memory calibration (Jetson cameras, mobile streams),
//! use [`calibrate()`](crate::calibrate) directly with pre-extracted
//! frame pairs instead.
//!
//! ```ignore
//! use std::sync::atomic::AtomicBool;
//! use reco_calibrate::video::{calibrate_videos, CalibrateVideosOptions};
//!
//! let interrupted = AtomicBool::new(false);
//! let result = calibrate_videos(
//!     "left.mp4".as_ref(),
//!     "right.mp4".as_ref(),
//!     CalibrateVideosOptions::default(),
//!     &mut |p| eprintln!("{}: {}", p.step, p.detail),
//!     &interrupted,
//! )?;
//! println!("Confidence: {:.1}%", result.confidence * 100.0);
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use reco_core::calibration::CameraParams;
use reco_core::gpu::{GpuContext, GpuError};
use reco_io::ffmpeg::calibration_io::{self, CalibrationIoError};

use crate::error::{CalibrateError, CalibrationFailure};
use crate::pipeline::{CalibrationPipeline, VideoInfo};
use crate::types::{CalibrationConfig, CalibrationProgress, CalibrationResult, CalibrationStep};

/// Options for [`calibrate_videos`].
///
/// All fields are optional with sensible defaults.
///
/// Lens profile resolution order (first match wins):
/// 1. `left_params` + `right_params` (both must be set)
/// 2. `left_profile` path (+ optional `right_profile`)
/// 3. auto-detect from video metadata
#[derive(Debug, Clone, Default)]
pub struct CalibrateVideosOptions {
    /// Calibration algorithm config. Uses [`CalibrationConfig::default()`] if `None`.
    pub config: Option<CalibrationConfig>,
    /// Path to the left lens profile. Auto-detects from video metadata if `None`.
    pub left_profile: Option<PathBuf>,
    /// Path to the right lens profile. Uses left profile if `None`.
    pub right_profile: Option<PathBuf>,
    /// Pre-loaded left camera params. Wins over `left_profile` when both are
    /// set. Must be set together with `right_params`.
    ///
    /// Lets consumers that already resolved profiles (e.g. via
    /// `LensDatabase::candidates()`) skip the file round-trip.
    pub left_params: Option<CameraParams>,
    /// Pre-loaded right camera params. See [`Self::left_params`].
    pub right_params: Option<CameraParams>,
    /// Manual sync offset in frames. Auto-detects via IMU/audio if `None`.
    pub sync_offset: Option<i64>,
    /// Explicit rolling-shutter indication (CALB-07).
    ///
    /// `None` (default) leaves [`MatchConfig::rolling_shutter`](crate::types::MatchConfig::rolling_shutter) at its
    /// configured value (default `false`, inert). Set `Some(true)` for sources
    /// with a known rolling-shutter readout to enable the row-dependent
    /// vertical-disparity bound. The filter is never enabled silently.
    pub rolling_shutter: Option<bool>,
}

/// Errors from [`calibrate_videos`]. `Clone + Send + Sync` so a
/// calibration worker can post the typed result back to the UI
/// thread through an mpsc channel without stringifying.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CalibrateVideosError {
    /// Video I/O error (probe, decode, audio extraction).
    #[error("I/O: {0}")]
    Io(#[from] CalibrationIoError),

    /// GPU initialization error.
    #[error("GPU: {0}")]
    Gpu(#[from] GpuError),

    /// Calibration error.
    #[error("calibration: {0}")]
    Calibrate(#[from] CalibrateError),

    /// Calibration failed with a typed diagnostic (CALB-04).
    ///
    /// Carries the failing [`CalibrationFailure`]: the source error, the active
    /// step, and the partial per-frame metrics accumulated before failure. The
    /// host maps this to a plain-language diagnosis; the structure is preserved
    /// rather than flattened at this boundary.
    #[error("calibration: {}", .0.error)]
    Diagnostic(CalibrationFailure),

    /// No frames could be extracted from the videos.
    #[error("no frames extracted from videos")]
    NoFrames,

    /// Operation was cancelled via the interrupted flag.
    #[error("calibration cancelled")]
    Cancelled,
}

// Compile-time assertion (plan step 7 tail): `CalibrateVideosError`
// and its transitive inner types are `Clone + Send + Sync`. Regresses
// if a future variant introduces a non-Clone `#[from]` wrap.
const _: fn() = || {
    fn assert_clone_send_sync<T: Clone + Send + Sync + 'static>() {}
    assert_clone_send_sync::<CalibrateVideosError>();
    assert_clone_send_sync::<crate::error::CalibrateError>();
    assert_clone_send_sync::<crate::error::CalibrationFailure>();
};

/// Check the interrupted flag and return `Cancelled` if set.
fn check_interrupted(interrupted: &AtomicBool) -> Result<(), CalibrateVideosError> {
    if interrupted.load(Ordering::Relaxed) {
        Err(CalibrateVideosError::Cancelled)
    } else {
        Ok(())
    }
}

/// Try audio cross-correlation sync, logging each failure reason.
///
/// N-1 (deep-review-2026-04-18): this used to swallow three distinct
/// error paths (extract_audio_pcm for left, for right, and
/// pipeline.audio_sync itself) via `let (Ok, Ok) = ...` +
/// `let _ = audio_sync`. A silent fall-through to sync_offset=0 made
/// calibration look blurry in the output without any hint in the
/// logs. Each path now logs a warn with specifics so post-deployment
/// diagnostic bundles carry the failure reason.
fn try_audio_sync(
    pipeline: &mut crate::pipeline::CalibrationPipeline,
    left_video: &std::path::Path,
    right_video: &std::path::Path,
) {
    let sample_rate = 44100;
    let left_audio = match calibration_io::extract_audio_pcm(left_video, sample_rate) {
        Ok(a) => a,
        Err(e) => {
            log::warn!(
                "sync: audio extraction failed for left={} ({e}); sync_offset stays at 0",
                left_video.display()
            );
            return;
        }
    };
    let right_audio = match calibration_io::extract_audio_pcm(right_video, sample_rate) {
        Ok(a) => a,
        Err(e) => {
            log::warn!(
                "sync: audio extraction failed for right={} ({e}); sync_offset stays at 0",
                right_video.display()
            );
            return;
        }
    };
    match pipeline.audio_sync(&left_audio, &right_audio, sample_rate) {
        Ok(offset) => {
            log::info!("sync: audio cross-correlation produced offset={offset} frames");
        }
        Err(e) => {
            log::warn!("sync: audio_sync failed ({e}); sync_offset stays at 0");
        }
    }
}

/// An audio-sync estimate from [`detect_audio_sync`] (MANU-02).
///
/// Carries the rounded offset in frames and the cross-correlation confidence,
/// so the manual flow can show a confidence readout and fall back to a manual
/// nudge when the estimate is absent or low. A genuine zero offset is a real
/// estimate with a confidence — never confused with "unavailable".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioSyncEstimate {
    /// Rounded sync offset in frames (positive advances the right stream; see
    /// `events::SYNC_OFFSET_SEMANTICS`).
    pub offset_frames: i64,
    /// Peak cross-correlation confidence (higher = more confident).
    pub confidence: f64,
}

/// Estimate the temporal sync offset between two clips from their audio
/// (MANU-02).
///
/// This is the manual flow's audio auto-sync. It reuses the *same* engine path
/// as auto-calibration — [`calibration_io::extract_audio_pcm`] for both clips
/// and [`CalibrationPipeline::audio_sync`] for the cross-correlation — so the
/// operator sees the engine's estimate, never a second implementation.
///
/// # Errors
///
/// Returns a typed [`CalibrateVideosError`] when either clip's metadata cannot
/// be probed, either clip's audio cannot be extracted, or the correlation
/// fails. The caller **must** be able to distinguish "unavailable" from a
/// genuine zero offset, so this never silently returns `0`.
pub fn detect_audio_sync(
    left_video: &Path,
    right_video: &Path,
    sample_rate: u32,
) -> Result<AudioSyncEstimate, CalibrateVideosError> {
    reco_io::init();

    // The frame rate is needed to round the sub-frame offset to whole frames;
    // probe the left clip (the correlation is frame-rate agnostic).
    let probe = calibration_io::probe_video(left_video)?;
    let left_audio = calibration_io::extract_audio_pcm(left_video, sample_rate)?;
    let right_audio = calibration_io::extract_audio_pcm(right_video, sample_rate)?;

    let info = VideoInfo {
        path: left_video.into(),
        width: probe.width,
        height: probe.height,
        fps: probe.fps,
        total_frames: probe.total_frames,
    };
    let mut pipeline = CalibrationPipeline::new(info.clone(), info, CalibrationConfig::default());
    let offset_frames = pipeline.audio_sync(&left_audio, &right_audio, sample_rate)?;
    let confidence = pipeline.sync_confidence().unwrap_or(0.0);
    Ok(AudioSyncEstimate {
        offset_frames,
        confidence,
    })
}

/// Emit a progress update for the given step.
fn emit_progress(
    on_progress: &mut dyn FnMut(&CalibrationProgress),
    step: CalibrationStep,
    detail: impl Into<String>,
) {
    on_progress(&CalibrationProgress {
        step,
        detail: detail.into(),
    });
}

/// Calibrate two video files with a single function call.
///
/// Handles the full workflow: video probing, lens profile detection,
/// sync estimation (IMU, then audio fallback), frame extraction, GPU
/// initialization, and calibration.
///
/// The `on_progress` callback is invoked before each major step so
/// callers can display status. The `interrupted` flag is checked
/// before each step and returns [`CalibrateVideosError::Cancelled`]
/// if set.
///
/// This wrapper creates its own [`GpuContext`] with a blocking device
/// request. Callers that already own a device
/// (e.g. a single-owner engine worker — FOUND-03/D-03) must use
/// [`calibrate_videos_with_gpu`] instead so calibration runs on the
/// shared device.
///
/// For advanced use cases (custom sync, debug output), use
/// [`CalibrationPipeline`] directly. For live/in-memory frames
/// (Jetson cameras, mobile streams), use
/// [`calibrate()`](crate::calibrate) directly.
pub fn calibrate_videos(
    left_video: &Path,
    right_video: &Path,
    options: CalibrateVideosOptions,
    on_progress: &mut dyn FnMut(&CalibrationProgress),
    interrupted: &AtomicBool,
) -> Result<CalibrationResult, CalibrateVideosError> {
    let gpu = GpuContext::new_blocking()?;
    calibrate_videos_with_gpu(
        &gpu,
        left_video,
        right_video,
        options,
        on_progress,
        interrupted,
    )
}

/// Calibrate two video files on a caller-supplied [`GpuContext`] (E1 / FOUND-03).
///
/// Identical orchestration to [`calibrate_videos`], but never creates its
/// own device: calibration runs on the caller's context, preserving the
/// single-device-owner invariant (D-03). Emits all seven
/// [`CalibrationStep`] variants through `on_progress`:
/// `Probing`, `DetectingProfiles`, `AudioSync`, `ExtractingFrames`,
/// `FeatureMatching` (this function) and `Undistorting`, `Optimizing`
/// (via [`CalibrationPipeline::calibrate_with_progress`]).
pub fn calibrate_videos_with_gpu(
    gpu: &GpuContext,
    left_video: &Path,
    right_video: &Path,
    options: CalibrateVideosOptions,
    on_progress: &mut dyn FnMut(&CalibrationProgress),
    interrupted: &AtomicBool,
) -> Result<CalibrationResult, CalibrateVideosError> {
    reco_io::init();

    let mut config = options.config.unwrap_or_default();
    // Explicit rolling-shutter indication only; never enabled silently (CALB-07).
    if let Some(rolling_shutter) = options.rolling_shutter {
        config.matching.rolling_shutter = rolling_shutter;
    }

    // Probe video metadata
    check_interrupted(interrupted)?;
    emit_progress(
        on_progress,
        CalibrationStep::Probing,
        "Probing video metadata",
    );
    let left_probe = calibration_io::probe_video(left_video)?;
    let right_probe = calibration_io::probe_video(right_video)?;
    let fps = left_probe.fps;

    let left_info = VideoInfo {
        path: left_video.into(),
        width: left_probe.width,
        height: left_probe.height,
        fps: left_probe.fps,
        total_frames: left_probe.total_frames,
    };
    let right_info = VideoInfo {
        path: right_video.into(),
        width: right_probe.width,
        height: right_probe.height,
        fps: right_probe.fps,
        total_frames: right_probe.total_frames,
    };

    let mut pipeline = CalibrationPipeline::new(left_info, right_info, config);

    // Lens profiles: direct params > path > auto-detect.
    match (options.left_params.clone(), options.right_params.clone()) {
        (Some(lp), Some(rp)) => pipeline.set_profiles(lp, rp),
        (left_params, right_params) => {
            if left_params.is_some() || right_params.is_some() {
                log::warn!(
                    "CalibrateVideosOptions: left_params/right_params must both \
                     be set (got left={}, right={}); falling back to path / auto-detect",
                    left_params.is_some(),
                    right_params.is_some(),
                );
            }
            if let Some(ref lp) = options.left_profile {
                pipeline.load_profiles(lp, options.right_profile.as_deref())?;
            } else {
                // Distinct step label - telemetry parse dominates here on
                // heavy sources (DJI Action 4, newer GoPro). Without this
                // the UI stays on "Probing video metadata" for 30-60s.
                check_interrupted(interrupted)?;
                emit_progress(
                    on_progress,
                    CalibrationStep::DetectingProfiles,
                    "Detecting lens profiles",
                );
                pipeline.detect_profiles()?;
            }
        }
    }

    // Sync: manual > IMU > audio > default (0)
    check_interrupted(interrupted)?;
    emit_progress(
        on_progress,
        CalibrationStep::AudioSync,
        "Detecting sync offset",
    );
    if let Some(offset) = options.sync_offset {
        log::info!("sync: manual override {offset} frames");
        pipeline.set_sync_offset(offset);
    } else {
        // N-1 (deep-review-2026-04-18): the fallback used to drop every
        // error silently and leave sync_offset at 0, producing visibly
        // blurry calibration with no explanation in the logs. Surface
        // each failure so post-deployment diagnostic bundles carry the
        // reason.
        match pipeline.imu_sync() {
            Ok(Some(offset)) => {
                log::info!("sync: IMU produced offset={offset} frames");
            }
            Ok(None) => {
                log::warn!(
                    "sync: IMU returned no offset (one or both telemetry streams missing); trying audio"
                );
                try_audio_sync(&mut pipeline, left_video, right_video);
            }
            Err(e) => {
                log::warn!("sync: IMU path failed ({e}); trying audio");
                try_audio_sync(&mut pipeline, left_video, right_video);
            }
        }
    }

    // Extract frames at computed indices
    check_interrupted(interrupted)?;
    let (left_indices, right_indices) = pipeline.frame_indices();
    emit_progress(
        on_progress,
        CalibrationStep::ExtractingFrames,
        format!("Extracting {} frame pairs", left_indices.len()),
    );
    log::info!(
        "extracting {} frames (fps: {fps:.1}, sync_offset: {})",
        left_indices.len(),
        pipeline.sync_offset(),
    );

    let left_frames = calibration_io::extract_frames(left_video, &left_indices)?;
    let right_frames = calibration_io::extract_frames(right_video, &right_indices)?;

    let pair_count = left_frames.len().min(right_frames.len());
    if pair_count == 0 {
        return Err(CalibrateVideosError::NoFrames);
    }

    let frame_pairs: Vec<_> = left_frames.into_iter().zip(right_frames).collect();

    // GPU init + calibrate
    check_interrupted(interrupted)?;
    emit_progress(
        on_progress,
        CalibrationStep::FeatureMatching,
        "GPU init and feature matching",
    );
    log::info!("GPU: {}", gpu.gpu_name());

    let result = pipeline
        .calibrate_with_progress(gpu, &frame_pairs, on_progress)
        .map_err(CalibrateVideosError::Diagnostic)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time proof that both entry points exist with the expected
    /// signatures: `calibrate_videos_with_gpu` takes `&GpuContext`, and
    /// `calibrate_videos` remains the self-creating wrapper (public API
    /// unchanged). Casting each item to an explicit `fn` pointer type fails
    /// to compile if either signature drifts.
    #[test]
    fn entry_point_signatures_compile() {
        let _gpu_aware = calibrate_videos_with_gpu
            as fn(
                &GpuContext,
                &Path,
                &Path,
                CalibrateVideosOptions,
                &mut dyn FnMut(&CalibrationProgress),
                &AtomicBool,
            ) -> Result<CalibrationResult, CalibrateVideosError>;
        let _wrapper = calibrate_videos
            as fn(
                &Path,
                &Path,
                CalibrateVideosOptions,
                &mut dyn FnMut(&CalibrationProgress),
                &AtomicBool,
            ) -> Result<CalibrationResult, CalibrateVideosError>;
    }

    #[test]
    fn detect_audio_sync_errors_on_an_unreadable_path() {
        // MANU-02: "unavailable" must be a typed `Err`, never a silent
        // `Ok(0)`, so the UI can distinguish it from a genuine zero offset and
        // require an explicit manual offset.
        let missing = Path::new("/nonexistent/reco/definitely-not-a-clip.mp4");
        let result = detect_audio_sync(missing, missing, 44100);
        assert!(
            result.is_err(),
            "an unreadable clip must be an error, got {result:?}"
        );
    }

    #[test]
    fn detect_audio_sync_estimates_from_the_real_mismatched_clips() {
        // MANU-02 integration (headless): the real mismatched Xiaomi clips carry
        // AAC audio, so the engine estimate is exercised end to end. The clips
        // are gitignored and may be absent on a fresh checkout — skip when so,
        // so the unit suite never depends on them.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-media/xiaomi");
        let left = root.join("xiaomi-11tpro-left.mp4");
        let right = root.join("xiaomi-14tpro-right.mp4");
        if !left.exists() || !right.exists() {
            eprintln!("skipping: the real Xiaomi clips are not present");
            return;
        }
        let estimate = detect_audio_sync(&left, &right, 44100)
            .expect("the real clips must yield an audio-sync estimate");
        assert!(
            estimate.confidence.is_finite(),
            "a real estimate must carry a finite confidence: {estimate:?}"
        );
        assert!(
            (0.0..=1.0).contains(&estimate.confidence),
            "the confidence must be a normalized coefficient in [0, 1], got {}",
            estimate.confidence
        );
        eprintln!(
            "real audio sync: offset_frames={} confidence={:.3}",
            estimate.offset_frames, estimate.confidence
        );
    }
}
