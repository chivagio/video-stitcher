//! Manual (pin-driven) calibration solve.
//!
//! Turns hand-placed pixel correspondence pins — and, optionally, already
//! verified automatic matches — into a solved [`PlaneLayout`] through the
//! public [`optimizer::optimize`] entry point. The module is pure: points in,
//! layout out. No GPU, no file, no channel, so it sits at the same testable
//! optimizer boundary as the rest of the crate.
//!
//! # The left/right swap is encoded once
//!
//! The pipeline maps camera pixels into optimizer plane space with a
//! deliberate left/right swap: the **right** camera's pixels land on the
//! **left** plane (x-plane) and the **left** camera's pixels land on the
//! **right** plane (z-plane). See the `matched_point` closure in `lib.rs`
//! (`lib.rs:361-377`) — that closure is the single source of truth.
//!
//! Manual pins are collected as `(left_px, right_px)` pairs in *natural*
//! camera order, so [`pin_to_matched_point`] performs the identical swap.
//! This is the only place the swap is applied for manual solves; callers must
//! never re-derive it.
//!
//! # Why a test is mandatory
//!
//! A wrong swap does **not** fail loudly. Spike 001 measured that the
//! unswapped solve lands on a *different, internally consistent* rig with
//! residual `0.000000` (zero overlap, `intersect = 0.0`) — so the residual
//! alone cannot detect the error. The swap is therefore proven against a
//! synthetic truth in the module tests instead.

use reco_core::calibration::PlaneLayout;

use crate::error::CalibrateError;
use crate::geometry::normalize_to_plane;
use crate::optimizer;
use crate::types::{CalibrationConfig, MatchedPoint};

/// A hand-placed correspondence pin in natural camera pixel coordinates.
///
/// `left_px` is the point the operator clicked on the **left** frame and
/// `right_px` the corresponding point on the **right** frame. Both are
/// `[x, y]` pixel coordinates with the origin at the top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ManualPin {
    /// Clicked point on the left frame, `[x, y]` pixels.
    pub left_px: [f64; 2],
    /// Corresponding point on the right frame, `[x, y]` pixels.
    pub right_px: [f64; 2],
}

/// Result of a manual calibration solve.
#[derive(Debug, Clone)]
pub struct ManualSolveResult {
    /// Solved camera layout (ready to serialize as a calibration profile).
    pub layout: PlaneLayout,
    /// Residual seam-weighted reprojection error at the optimum (dimensionless).
    pub residual: f64,
    /// Number of manual pins that contributed to the solve.
    pub pins_used: usize,
    /// Number of pre-populated automatic matches that contributed.
    pub auto_used: usize,
}

/// Convert a natural-order pixel pair into an optimizer-space
/// [`MatchedPoint`], applying the pipeline's left/right swap **once**.
///
/// CRITICAL: this mirrors the `matched_point` closure in `lib.rs:361-377`
/// exactly — right pixel → left plane, left pixel → right plane — including
/// the normalized pixel-x seam coordinates. Do not re-derive the swap
/// anywhere else.
#[must_use]
pub fn pin_to_matched_point(
    left_px: [f64; 2],
    right_px: [f64; 2],
    left_wh: (u32, u32),
    right_wh: (u32, u32),
) -> MatchedPoint {
    let (lw, lh) = left_wh;
    let (rw, rh) = right_wh;
    MatchedPoint {
        // Right camera pixel -> left plane (x-plane in optimizer space).
        left: normalize_to_plane(right_px[0], right_px[1], rw, rh),
        // Left camera pixel -> right plane (z-plane in optimizer space).
        right: normalize_to_plane(left_px[0], left_px[1], lw, lh),
        // Seam-proximity weighting uses the same swapped pixel-x convention.
        left_pixel_nx: right_px[0] / rw.max(1) as f64,
        right_pixel_nx: left_px[0] / lw.max(1) as f64,
    }
}

/// Minimum variance (in plane units squared) of the combined point cloud
/// below which the set is treated as degenerate — all points coincident or
/// collinear. Such a set cannot constrain the layout, and the optimizer would
/// otherwise return a garbage rig.
const MIN_POINT_VARIANCE: f64 = 1e-9;

/// True when the combined plane coordinates are too poorly spread to
/// constrain the optimizer (all coincident, or all collinear).
///
/// The optimizer sees both the x-plane (`left`) and z-plane (`right`)
/// coordinates, so degeneracy is judged over their union. A 2×2 covariance
/// is formed and its smaller eigenvalue — the variance perpendicular to the
/// dominant axis — is compared against [`MIN_POINT_VARIANCE`]. Coincident
/// points give two near-zero eigenvalues; collinear points give one.
fn is_degenerate(points: &[MatchedPoint]) -> bool {
    let mut coords = Vec::with_capacity(points.len() * 2);
    for p in points {
        coords.push(p.left);
        coords.push(p.right);
    }
    if coords.len() < 2 {
        return true;
    }
    let n = coords.len() as f64;
    let mut mean = [0.0_f64, 0.0_f64];
    for c in &coords {
        mean[0] += c[0];
        mean[1] += c[1];
    }
    mean[0] /= n;
    mean[1] /= n;

    let (mut sxx, mut sxy, mut syy) = (0.0_f64, 0.0_f64, 0.0_f64);
    for c in &coords {
        let dx = c[0] - mean[0];
        let dy = c[1] - mean[1];
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    sxx /= n;
    sxy /= n;
    syy /= n;

    // Smaller eigenvalue of the symmetric 2×2 covariance:
    //   lambda_min = 0.5 * (trace - sqrt(trace^2 - 4*det))
    let trace = sxx + syy;
    let det = sxx * syy - sxy * sxy;
    let disc = (trace * trace - 4.0 * det).max(0.0).sqrt();
    let lambda_min = 0.5 * (trace - disc);
    lambda_min <= MIN_POINT_VARIANCE
}

/// Solve a manual calibration from pins plus optional pre-populated auto
/// matches.
///
/// `auto` points are **already in optimizer plane coordinates** — they came
/// from the pipeline's own swap-correct path (post-RANSAC) — so they are
/// appended as-is and never re-swapped. Each [`ManualPin`] is converted with
/// [`pin_to_matched_point`], the single swap-correct helper.
///
/// # Errors
///
/// * [`CalibrateError::InsufficientMatches`] with `min = 1` when the combined
///   point set is empty (a defined non-result, never a bogus zero layout).
/// * [`CalibrateError::InvalidConfig`] when the set is degenerate (all points
///   coincident or collinear) and cannot constrain a solve.
/// * Any error returned by [`optimizer::optimize`].
pub fn solve_manual_calibration(
    pins: &[ManualPin],
    auto: &[MatchedPoint],
    left_wh: (u32, u32),
    right_wh: (u32, u32),
    config: &CalibrationConfig,
) -> Result<ManualSolveResult, CalibrateError> {
    let mut points: Vec<MatchedPoint> = Vec::with_capacity(auto.len() + pins.len());
    points.extend_from_slice(auto);
    points.extend(
        pins.iter()
            .map(|p| pin_to_matched_point(p.left_px, p.right_px, left_wh, right_wh)),
    );

    if points.is_empty() {
        return Err(CalibrateError::InsufficientMatches { got: 0, min: 1 });
    }
    if is_degenerate(&points) {
        return Err(CalibrateError::InvalidConfig(
            "degenerate manual pin set: points are coincident or collinear".to_string(),
        ));
    }

    let (layout, residual) = optimizer::optimize(&points, config)?;
    Ok(ManualSolveResult {
        layout,
        residual,
        pins_used: pins.len(),
        auto_used: auto.len(),
    })
}
