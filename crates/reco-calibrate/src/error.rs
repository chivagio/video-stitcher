//! Error types for the calibration pipeline.

use thiserror::Error;

use crate::types::{CalibrationStep, FrameMatches};

/// Errors from the calibration pipeline. `Clone + Send + Sync` so
/// the calibration background thread can post results back to the
/// UI thread with typed errors (no stringification at the mpsc
/// boundary).
#[derive(Debug, Clone, Error)]
pub enum CalibrateError {
    /// No keypoints were detected in a frame.
    #[error("no keypoints detected in {camera} frame {frame_idx}")]
    NoKeypoints {
        /// Which camera (`"left"` or `"right"`).
        camera: &'static str,
        /// Frame index (0-based).
        frame_idx: usize,
    },

    /// Too few feature matches survived filtering.
    #[error("insufficient matches: got {got}, need at least {min}")]
    InsufficientMatches {
        /// Number of matches found.
        got: usize,
        /// Minimum required.
        min: usize,
    },

    /// RANSAC rejected all candidate matches.
    #[error("RANSAC rejected all matches")]
    RansacFailed,

    /// The optimizer did not converge to a solution.
    #[error("optimizer did not converge after {max_evals} evaluations")]
    OptimizerFailed {
        /// Maximum evaluations allowed.
        max_evals: usize,
    },

    /// No frame pairs produced usable matches after the full pipeline.
    #[error("no usable frame pairs (all frames failed matching)")]
    NoUsableFrames,

    /// A frame has invalid dimensions (zero or too large).
    #[error("invalid frame dimensions: {width}x{height}")]
    InvalidDimensions {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
    },

    /// An RGBA buffer doesn't match the expected dimensions.
    #[error("invalid buffer size: expected {expected} bytes, got {got}")]
    InvalidBuffer {
        /// Expected buffer size in bytes.
        expected: usize,
        /// Actual buffer size in bytes.
        got: usize,
    },

    /// Image is too small for feature detection.
    #[error("image too small for AKAZE: {width}x{height} (minimum ~40px)")]
    ImageTooSmall {
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
    },

    /// FFT computation failed during audio sync.
    #[error("FFT error: {0}")]
    FftError(String),

    /// Configuration has invalid values.
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

/// A typed calibration failure carrying the diagnostic context the operator
/// needs to understand *why* a run failed (CALB-04).
///
/// Unlike a bare [`CalibrateError`], this also carries the pipeline
/// [`CalibrationStep`] that was active when the failure occurred and the
/// per-frame [`FrameMatches`] accumulated before it (empty when the run failed
/// before any frame produced matches). It stays `Clone + Send + Sync` so the
/// engine can post it back to the UI thread through the worker channel without
/// flattening the structure to a string.
///
/// The host maps this to a plain-language cause/fix in
/// `reco-app`'s `calibration::diagnose_calibration_failure`; the frontend never
/// invents a cause (CALB-04 / D3 lineage).
#[derive(Debug, Clone)]
pub struct CalibrationFailure {
    /// The underlying engine error.
    pub error: CalibrateError,
    /// The pipeline step active when the failure occurred.
    pub step: CalibrationStep,
    /// Per-frame match statistics accumulated before the failure.
    ///
    /// Empty when the run failed before any frame pair produced a match.
    pub frames: Vec<FrameMatches>,
}

impl CalibrationFailure {
    /// Create a failure diagnostic from its three parts.
    pub fn new(error: CalibrateError, step: CalibrationStep, frames: Vec<FrameMatches>) -> Self {
        Self {
            error,
            step,
            frames,
        }
    }

    /// Create a failure diagnostic with no partial frames.
    pub fn at_step(error: CalibrateError, step: CalibrationStep) -> Self {
        Self::new(error, step, Vec::new())
    }
}

impl From<CalibrationFailure> for CalibrateError {
    fn from(failure: CalibrationFailure) -> Self {
        failure.error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_failure_carries_the_source_error_and_partial_frames() {
        let failure = CalibrationFailure::at_step(
            CalibrateError::NoUsableFrames,
            CalibrationStep::FeatureMatching,
        );
        assert_eq!(failure.frames.len(), 0);
        assert_eq!(failure.step, CalibrationStep::FeatureMatching);
        // The source error survives the conversion back to the bare type.
        let source: CalibrateError = failure.into();
        assert!(matches!(source, CalibrateError::NoUsableFrames));
    }
}
