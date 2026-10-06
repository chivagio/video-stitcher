//! Reduced `k1`-only intrinsics refinement.
//!
//! Refines the first radial distortion coefficient `k1` of a camera against
//! raw distorted-pixel correspondences, holding every other intrinsic fixed.
//! This is the well-posed reduction from the phase research
//! (`.planning/research/lens-handles-and-intrinsics-solver.md` Part C §C2):
//! `fx` is degenerate with `cam_d`/`intersect`, `cx/cy` with `x_ty`, and `fy`
//! with `fx` (square pixels) — so only `k1` is cleanly observable on a
//! small-baseline, mostly-planar rig.
//!
//! # Composition
//!
//! The solve sits *beside* the existing layout optimizer; it never adds
//! intrinsics to [`OptParams`] or changes the layout cost. For a candidate
//! `k1` it:
//!
//! 1. maps each raw distorted pixel to an undistorted pixel with the public
//!    [`reco_core::lens::distorted_to_undistorted`] inverse (candidate `k1`
//!    substituted into `params.d[0]`);
//! 2. converts to plane coordinates with the pipeline's single left/right
//!    swap (right pixel → `.left` plane, left pixel → `.right` plane);
//! 3. evaluates the existing trimmed seam-weighted reprojection error at the
//!    fixed layout.
//!
//! A bounded Nelder-Mead (reusing the same `argmin` plumbing and quadratic
//! bounds penalty as [`crate::optimizer`]) minimizes over `k1` alone.
//!
//! The module is pure: points in, result out — no GPU, file, or channel.
//!
//! # Coordinate conventions
//!
//! [`RawPixelMatch`] holds **raw distorted** pixels in *natural* camera order
//! (`left_px` = left camera, `right_px` = right camera). The swap into plane
//! space is the single encoding from [`crate::manual::pin_to_matched_point`];
//! this module never re-derives it.

use argmin::core::{CostFunction, Error, Executor, State};
use argmin::solver::neldermead::NelderMead;
use reco_core::calibration::{CameraParams, PlaneLayout};
use reco_core::lens::distorted_to_undistorted;

use crate::error::CalibrateError;
use crate::geometry::{self, OptParams, normalize_to_plane};
use crate::types::MatchedPoint;

/// Perturbation scale for the initial simplex (fraction of the `k1` range).
///
/// Mirrors `optimizer::SIMPLEX_PERTURBATION`; a 1-D problem needs 2 vertices.
const SIMPLEX_PERTURBATION: f64 = 0.10;

/// Quadratic penalty scale for a `k1` outside its bound.
///
/// Mirrors `optimizer::bounds_penalty` (scale `1e4`).
const BOUNDS_PENALTY_SCALE: f64 = 1e4;

/// Cost returned when a candidate `k1` makes the KB4 inverse diverge
/// (`distorted_to_undistorted` returns `None`).
///
/// A large *finite* value keeps Nelder-Mead well-defined (no NaN/Inf) while
/// pushing the simplex back toward the convergent region.
const OUT_OF_DOMAIN_COST: f64 = 1e12;

/// A raw distorted-pixel correspondence in natural camera order.
///
/// `left_px` is the point in the **left** camera's raw distorted image and
/// `right_px` the corresponding point in the **right** camera's raw distorted
/// image. Both are absolute `[x, y]` pixel coordinates with the origin at the
/// top-left of their own frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawPixelMatch {
    /// Point in the left camera's raw distorted image, `[x, y]` pixels.
    pub left_px: [f64; 2],
    /// Corresponding point in the right camera's raw distorted image, `[x, y]` pixels.
    pub right_px: [f64; 2],
}

/// Configuration for the reduced `k1` refinement.
#[derive(Debug, Clone, Copy)]
pub struct IntrinsicsConfig {
    /// Half-width of the `k1` search bound around the profile's current value.
    ///
    /// `0.3` matches the manual lens clamp (`04.1-UI-SPEC.md`).
    pub k1_bound: f64,
    /// Maximum Nelder-Mead iterations.
    pub max_iters: usize,
    /// Seam-proximity Gaussian sigma for the reprojection objective.
    pub sigma: f64,
    /// Fraction of worst points to drop in the trimmed objective
    /// (`0.0` = no trimming).
    pub trim_fraction: f64,
}

impl Default for IntrinsicsConfig {
    fn default() -> Self {
        Self {
            k1_bound: 0.3,
            max_iters: 5000,
            sigma: 0.08,
            trim_fraction: 0.3,
        }
    }
}

/// Typed result of a reduced `k1` refinement.
///
/// `fx/fy`, `cx/cy`, and `cam_d` are fixed by construction and never appear
/// here — only the refined `k1` and the objective residual at the optimum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntrinsicsRefinement {
    /// The refined first radial distortion coefficient.
    pub k1: f64,
    /// Trimmed seam-weighted reprojection error at the optimum.
    pub residual: f64,
}

/// Convert a raw-pixel correspondence to an optimizer-space [`MatchedPoint`].
///
/// Each raw distorted pixel is mapped to an undistorted output pixel with
/// [`distorted_to_undistorted`] under `params_with_k1` (whose `d[0]` is the
/// **candidate** `k1`), then normalized to plane coordinates with the same
/// left/right swap as [`crate::manual::pin_to_matched_point`]: the right
/// camera's pixel lands on the left plane (`.left`) and the left camera's
/// pixel on the right plane (`.right`).
///
/// Returns `None` if either camera's KB4 inverse fails to converge.
#[must_use]
pub fn raw_to_matched_point(
    raw: &RawPixelMatch,
    params_with_k1: &CameraParams,
    left_wh: (u32, u32),
    right_wh: (u32, u32),
) -> Option<MatchedPoint> {
    let (lw, lh) = left_wh;
    let (rw, rh) = right_wh;

    // Left camera raw pixel -> undistorted output pixel.
    let left_und =
        distorted_to_undistorted(raw.left_px[0], raw.left_px[1], lw, lh, params_with_k1)?;
    // Right camera raw pixel -> undistorted output pixel.
    let right_und =
        distorted_to_undistorted(raw.right_px[0], raw.right_px[1], rw, rh, params_with_k1)?;

    Some(MatchedPoint {
        // Right camera pixel -> left plane (x-plane in optimizer space).
        left: normalize_to_plane(right_und.0, right_und.1, rw, rh),
        // Left camera pixel -> right plane (z-plane in optimizer space).
        right: normalize_to_plane(left_und.0, left_und.1, lw, lh),
        // Seam-proximity weighting uses the same swapped pixel-x convention.
        left_pixel_nx: right_und.0 / rw.max(1) as f64,
        right_pixel_nx: left_und.0 / lw.max(1) as f64,
    })
}

/// Cost function for the 1-D `k1` Nelder-Mead solve.
///
/// Mirrors [`crate::optimizer::CalibrationCost`] (bounds penalty + trimmed
/// seam-weighted objective) specialized to a single free variable.
#[derive(Clone)]
struct IntrinsicsCost<'a> {
    points: &'a [RawPixelMatch],
    base: &'a CameraParams,
    layout: OptParams,
    left_wh: (u32, u32),
    right_wh: (u32, u32),
    sigma: f64,
    trim_fraction: f64,
    /// Inclusive `k1` bounds `(lo, hi)`.
    k1_bounds: (f64, f64),
}

impl CostFunction for IntrinsicsCost<'_> {
    type Param = Vec<f64>;
    type Output = f64;

    fn cost(&self, p: &Self::Param) -> Result<Self::Output, Error> {
        let k1 = p[0];

        // Observations at the candidate k1: substitute into d[0], leaving
        // fx/fy/cx/cy/cam_d untouched (they never enter the free vector).
        let mut params = self.base.clone();
        params.d[0] = k1;

        let mut points = Vec::with_capacity(self.points.len());
        for raw in self.points {
            match raw_to_matched_point(raw, &params, self.left_wh, self.right_wh) {
                Some(mp) => points.push(mp),
                // Divergent inverse: reject this candidate with a large
                // finite cost (never a NaN/Inf).
                None => return Ok(OUT_OF_DOMAIN_COST),
            }
        }

        let err = if self.trim_fraction > 0.0 {
            geometry::trimmed_seam_weighted_reprojection_error(
                &points,
                &self.layout,
                self.sigma,
                self.trim_fraction,
            )
        } else {
            geometry::seam_weighted_reprojection_error(&points, &self.layout, self.sigma)
        };

        Ok(err + k1_bounds_penalty(k1, self.k1_bounds))
    }
}

/// Quadratic penalty for a `k1` outside `(lo, hi)` (Nelder-Mead is unconstrained).
fn k1_bounds_penalty(k1: f64, (lo, hi): (f64, f64)) -> f64 {
    if k1 < lo {
        let d = lo - k1;
        BOUNDS_PENALTY_SCALE * d * d
    } else if k1 > hi {
        let d = k1 - hi;
        BOUNDS_PENALTY_SCALE * d * d
    } else {
        0.0
    }
}

/// Refine `k1` against raw-pixel correspondences at a fixed layout.
///
/// Solves the reduced 1-parameter problem: `k1` alone is free, bounded to
/// `[base.d[0] - k1_bound, base.d[0] + k1_bound]`; `fx/fy`, `cx/cy`, and
/// `cam_d` are fixed (they never enter the free vector). The objective
/// converts each observation to plane coordinates at the candidate `k1`
/// (via [`raw_to_matched_point`]) and evaluates the trimmed seam-weighted
/// reprojection error at `layout` (converted to [`OptParams`] from the
/// [`PlaneLayout`] fields).
///
/// Frame dimensions are taken from `base.width`/`base.height` for both
/// cameras; callers with a genuinely different right-camera resolution can
/// convert with [`raw_to_matched_point`] directly.
///
/// # Errors
///
/// * [`CalibrateError::InsufficientMatches`] with `min = 1` for an empty
///   observation set — a defined non-result, never a zero-`k1` answer.
/// * [`CalibrateError::OptimizerFailed`] if Nelder-Mead returns no best
///   parameter.
/// * [`CalibrateError::InvalidConfig`] if the solver cannot be constructed
///   or run.
pub fn optimize_intrinsics(
    points: &[RawPixelMatch],
    layout: &PlaneLayout,
    base: &CameraParams,
    cfg: &IntrinsicsConfig,
) -> Result<IntrinsicsRefinement, CalibrateError> {
    if points.is_empty() {
        return Err(CalibrateError::InsufficientMatches { got: 0, min: 1 });
    }

    // PlaneLayout -> OptParams in the from_5param order
    // [x_ty, intersect, cam_d, x_rz, z_rx].
    let layout_params = OptParams::from_5param(&[
        layout.x_ty,
        layout.intersect,
        layout.camera_axis_offset,
        layout.x_rz,
        layout.z_rx,
    ]);

    let lo = base.d[0] - cfg.k1_bound;
    let hi = base.d[0] + cfg.k1_bound;

    let cost = IntrinsicsCost {
        points,
        base,
        layout: layout_params,
        left_wh: (base.width, base.height),
        right_wh: (base.width, base.height),
        sigma: cfg.sigma,
        trim_fraction: cfg.trim_fraction,
        k1_bounds: (lo, hi),
    };

    // 1-D simplex: two vertices around the profile's current k1.
    let start = base.d[0].clamp(lo, hi);
    let perturbation = SIMPLEX_PERTURBATION * (hi - lo);
    let second = if start + perturbation <= hi {
        start + perturbation
    } else {
        start - perturbation
    };
    let simplex = vec![vec![start], vec![second]];

    let solver = NelderMead::new(simplex)
        .with_sd_tolerance(1e-12)
        .map_err(|e| CalibrateError::InvalidConfig(format!("intrinsics solver init: {e}")))?;

    let result = Executor::new(cost, solver)
        .configure(|state| state.max_iters(cfg.max_iters as u64))
        .run()
        .map_err(|e| CalibrateError::InvalidConfig(format!("intrinsics optimization: {e}")))?;

    let best = result
        .state()
        .get_best_param()
        .ok_or(CalibrateError::OptimizerFailed {
            max_evals: cfg.max_iters,
        })?;

    Ok(IntrinsicsRefinement {
        k1: best[0],
        residual: result.state().get_best_cost(),
    })
}
