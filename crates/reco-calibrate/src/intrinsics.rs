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
    /// Seed for the deterministic fit/held-out split (INTR-02).
    ///
    /// The split is a seeded shuffle of the observation indices, so a given
    /// `(seed, observation count)` always yields the same fit and held-out
    /// sets — the held-out guard is reproducible run to run.
    pub seed: u64,
    /// Fraction of observations reserved as the held-out guard set.
    ///
    /// `0.2` matches the phase decision (≈20% held out). At least one
    /// observation is always held out and at least one kept for the fit, so a
    /// small set is still guarded.
    pub heldout_fraction: f64,
    /// Minimum held-out improvement required to accept a refinement (INTR-02).
    ///
    /// A refinement is accepted only when the held-out seam-weighted
    /// reprojection error improves by more than this epsilon:
    /// `heldout_refined < heldout_baseline - improvement_epsilon`. The epsilon
    /// stops an epsilon-sized or numerical-noise "improvement" from being
    /// reported as a real refinement (INTR-02 edge probe "no-improvement").
    pub improvement_epsilon: f64,
    /// Maximum alternating layout↔`k1` rounds.
    ///
    /// The loop stops early when the held-out residual stops improving, so this
    /// is an upper bound (2-3 rounds is enough in practice).
    pub max_rounds: usize,
    /// Minimum observation count for the conditioning gate (plan 02).
    pub min_matches: usize,
    /// Minimum mean normalized radial spread for the conditioning gate.
    pub min_spread: f64,
}

impl Default for IntrinsicsConfig {
    fn default() -> Self {
        Self {
            k1_bound: 0.3,
            max_iters: 5000,
            sigma: 0.08,
            trim_fraction: 0.3,
            seed: 0x5EED_0420,
            heldout_fraction: 0.2,
            improvement_epsilon: 1e-6,
            max_rounds: 3,
            min_matches: RECOMMENDED_MIN_MATCHES,
            min_spread: RECOMMENDED_MIN_SPREAD,
        }
    }
}

/// Typed result of a reduced `k1` refinement.
///
/// `fx/fy`, `cx/cy`, and `cam_d` are fixed by construction and never appear
/// here — only the refined `k1`, the fit residual, and the held-out guard
/// verdict. The caller (worker) writes `k1` into the profile **only** when
/// [`Self::accepted`] is `true`; on any rejection [`Self::k1`] is the baseline
/// `base.d[0]`, so a rejected refinement is never applied (INTR-02).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntrinsicsRefinement {
    /// The refined first radial distortion coefficient.
    ///
    /// Equals the baseline `base.d[0]` whenever the refinement is rejected, so
    /// an unconditional write of this value is always safe.
    pub k1: f64,
    /// Fit-set seam-weighted reprojection error at the returned solution.
    pub residual: f64,
    /// Whether the refinement passed the held-out guard and may be applied.
    pub accepted: bool,
    /// Why the refinement was accepted or rejected.
    pub reason: RefinementReason,
    /// Held-out seam-weighted reprojection error at the baseline `k1`.
    ///
    /// `0.0` when the conditioning gate refused before any held-out evaluation.
    pub heldout_baseline: f64,
    /// Held-out seam-weighted reprojection error at the returned `k1`.
    ///
    /// `0.0` when the conditioning gate refused before any held-out evaluation.
    pub heldout_refined: f64,
}

/// Why a reduced `k1` refinement was accepted or rejected (INTR-02).
///
/// The caller renders this directly; it never invents a reason. A rejection
/// always leaves the profile's `k1` unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefinementReason {
    /// The held-out reprojection improved beyond
    /// [`IntrinsicsConfig::improvement_epsilon`].
    Accepted,
    /// The refinement did not improve the held-out fit (a regression or a
    /// tie within epsilon); the profile is unchanged (the overfitting guard).
    GuardRejected,
    /// Too few observations to split into fit/held-out and constrain `k1`.
    InsufficientMatches,
    /// The observations lack the radial spread `k1` needs (centre-weighted).
    NotEnoughSpread,
    /// The layout cannot host a solve (non-finite).
    IllConditioned,
}

/// Result of the conditioning gate that guards the reduced solve.
///
/// The gate answers "is this observation set able to constrain `k1` at all?"
/// *before* the solver runs, so an ill-conditioned set is refused with a typed
/// non-result rather than silently fitted (research C2: `k1` is separable from
/// `fx` only by the *shape* of the radial field, which needs features spanning
/// a wide radius range).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conditioning {
    /// Number of raw-pixel observations considered.
    pub match_count: usize,
    /// Mean normalized radial position of the observations, in `[0, 1]`.
    ///
    /// Each match contributes the average of its two cameras' distances from
    /// their principal points, normalized by the frame's corner radius
    /// (`hypot(width/2, height/2)`). `0.0` is the optical centre, `1.0` the
    /// frame corner. A centre-weighted set scores ≈`0.05`; a set spread across
    /// the frame scores ≈`0.4`–`0.5`.
    pub radial_spread: f64,
    /// Whether the set passes *both* the match-count and radial-spread gates
    /// (and sits at a finite layout).
    pub well_conditioned: bool,
}

/// Recommended minimum match count for a well-conditioned `k1` solve.
///
/// Rationale: the pipeline's `min_matches` floor admits far fewer points than a
/// 1-parameter radial solve needs to average out detection noise, and the
/// research (C2) asks for "≥N well-spread matches across ≥2 frames". `30` is a
/// conservative floor — the synthetic harness recovers `k1` comfortably with
/// ~40+ in-frame observations — and leaves margin for the ~20% held-out split
/// (04.2-03) without dropping the fit set below a workable size.
pub const RECOMMENDED_MIN_MATCHES: usize = 30;

/// Recommended minimum mean normalized radius for a well-conditioned solve.
///
/// `radial_spread` is the mean, over observations, of each match's distance
/// from the principal point normalized by the frame's corner radius. `0.30`
/// requires the average observation to sit at least ~30% of the way to the
/// corner — enough radial leverage for `k1` to be separated from `fx` by the
/// shape of the radial field (research C2) — while not demanding the extreme
/// edge the pipeline's border filter removes (`features.rs`).
pub const RECOMMENDED_MIN_SPREAD: f64 = 0.30;

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

    // The unguarded reduced solve always returns its bounded optimum; the
    // held-out guard fields are populated by [`refine_intrinsics`]. There is no
    // fit/held-out split here, so the two held-out fields mirror the fit
    // residual and the result is reported as accepted.
    let best_cost = result.state().get_best_cost();
    Ok(IntrinsicsRefinement {
        k1: best[0],
        residual: best_cost,
        accepted: true,
        reason: RefinementReason::Accepted,
        heldout_baseline: best_cost,
        heldout_refined: best_cost,
    })
}

/// Frame corner radius in pixels for `params`.
///
/// The diagonal half-extent `hypot(width/2, height/2)` is the natural
/// normalization for a radial measurement: `0` at the principal point, `1` at
/// the frame corner.
fn frame_corner_radius(params: &CameraParams) -> f64 {
    let hw = params.width.max(1) as f64 / 2.0;
    let hh = params.height.max(1) as f64 / 2.0;
    (hw * hw + hh * hh).sqrt()
}

/// Distance of a pixel from the principal point, normalized by `radius`.
fn normalized_radius(px: [f64; 2], params: &CameraParams, radius: f64) -> f64 {
    let dx = px[0] - params.cx;
    let dy = px[1] - params.cy;
    (dx * dx + dy * dy).sqrt() / radius
}

/// Evaluate whether an observation set is conditioned to constrain `k1`.
///
/// The gate is the "well-posed" half of INTR-01: a reduced solve is only
/// meaningful when the correspondences span a wide radius range, so this
/// predicate reports the match count *and* the radial spread of the raw
/// observations, and a set that fails it must not reach the solver.
///
/// `radial_spread` is the mean, over observations, of each match's distance
/// from the principal point (per-camera `base.cx`/`base.cy`), normalized by the
/// frame's corner radius and averaged across the two cameras. `well_conditioned`
/// requires `match_count >= min_matches`, `radial_spread >= min_spread`, and a
/// finite `layout` — the layout is the fixed solve context, and a layout that
/// cannot host a solve is itself a reason to refuse. Recommended thresholds are
/// [`RECOMMENDED_MIN_MATCHES`] and [`RECOMMENDED_MIN_SPREAD`].
///
/// This mirrors the covariance-style reasoning of
/// [`crate::manual::solve_manual_calibration`]'s degeneracy check, applied to
/// the *radial distribution* that `k1` specifically needs.
#[must_use]
pub fn conditioning(
    points: &[RawPixelMatch],
    layout: &PlaneLayout,
    base: &CameraParams,
    min_matches: usize,
    min_spread: f64,
) -> Conditioning {
    let match_count = points.len();
    let radius = frame_corner_radius(base);

    let mut spread_sum = 0.0_f64;
    for raw in points {
        let left_r = normalized_radius(raw.left_px, base, radius);
        let right_r = normalized_radius(raw.right_px, base, radius);
        // A match constrains `k1` only where both cameras carry radial
        // leverage, so the per-match position is their average.
        spread_sum += 0.5 * (left_r + right_r);
    }
    let radial_spread = if match_count == 0 {
        0.0
    } else {
        spread_sum / match_count as f64
    };

    let layout_finite = layout.x_ty.is_finite()
        && layout.intersect.is_finite()
        && layout.camera_axis_offset.is_finite()
        && layout.x_rz.is_finite()
        && layout.z_rx.is_finite();

    Conditioning {
        match_count,
        radial_spread,
        well_conditioned: match_count >= min_matches
            && radial_spread >= min_spread
            && layout_finite,
    }
}

// ---------------------------------------------------------------------------
// Alternating layout ↔ k1 driver with a held-out guard (INTR-02)
// ---------------------------------------------------------------------------

/// Whether every [`PlaneLayout`] field is finite (a layout that cannot host a
/// solve).
fn layout_is_finite(layout: &PlaneLayout) -> bool {
    layout.x_ty.is_finite()
        && layout.intersect.is_finite()
        && layout.camera_axis_offset.is_finite()
        && layout.x_rz.is_finite()
        && layout.z_rx.is_finite()
}

/// Map the conditioning verdict to the typed refusal reason.
fn conditioning_reason(
    cond: &Conditioning,
    layout: &PlaneLayout,
    cfg: &IntrinsicsConfig,
) -> RefinementReason {
    if !layout_is_finite(layout) {
        RefinementReason::IllConditioned
    } else if cond.match_count < cfg.min_matches {
        RefinementReason::InsufficientMatches
    } else {
        // The only remaining way to fail the gate is insufficient spread.
        RefinementReason::NotEnoughSpread
    }
}

/// Build a typed non-result (a rejection) with the baseline `k1`.
fn rejected(
    k1: f64,
    residual: f64,
    heldout_baseline: f64,
    heldout_refined: f64,
    reason: RefinementReason,
) -> IntrinsicsRefinement {
    IntrinsicsRefinement {
        k1,
        residual,
        accepted: false,
        reason,
        heldout_baseline,
        heldout_refined,
    }
}

/// Split observation indices into a deterministic fit / held-out partition.
///
/// The indices are a seeded shuffle of `0..n`; the first
/// `round(n · heldout_fraction)` (clamped to `[1, n-1]`) become the held-out
/// guard set and the rest the fit set. Deterministic for a given
/// `(seed, n)`, so the guard is reproducible. Caller guarantees `n >= 2`.
fn split_indices(n: usize, cfg: &IntrinsicsConfig) -> (Vec<usize>, Vec<usize>) {
    use rand::SeedableRng;
    use rand::seq::SliceRandom;

    let mut order: Vec<usize> = (0..n).collect();
    let mut rng = rand::rngs::SmallRng::seed_from_u64(cfg.seed);
    order.shuffle(&mut rng);

    let held = ((n as f64 * cfg.heldout_fraction).round() as usize).clamp(1, n - 1);
    let (held_idx, fit_idx) = order.split_at(held);
    (fit_idx.to_vec(), held_idx.to_vec())
}

/// Partition observations into fit and held-out sets using [`split_indices`].
fn split_fit_heldout(
    points: &[RawPixelMatch],
    cfg: &IntrinsicsConfig,
) -> (Vec<RawPixelMatch>, Vec<RawPixelMatch>) {
    let n = points.len();
    let (fit_idx, held_idx) = split_indices(n, cfg);
    let fit = fit_idx.into_iter().map(|i| points[i]).collect();
    let held = held_idx.into_iter().map(|i| points[i]).collect();
    (fit, held)
}

/// Map raw observations to optimizer-space plane coordinates at a candidate
/// `k1` (both cameras sharing `base`'s intrinsics).
///
/// Returns `None` if any KB4 inverse diverges at this candidate.
fn map_matched(
    points: &[RawPixelMatch],
    base: &CameraParams,
    k1: f64,
    wh: (u32, u32),
) -> Option<Vec<MatchedPoint>> {
    let mut params = base.clone();
    params.d[0] = k1;
    points
        .iter()
        .map(|raw| raw_to_matched_point(raw, &params, wh, wh))
        .collect()
}

/// Seam-weighted reprojection error of `points` at `(k1, layout)`.
///
/// Uses the untrimmed objective so a held-out regression cannot be hidden by
/// the trim. A divergent inverse at this candidate `k1` returns
/// [`f64::INFINITY`], so the guard rejects it.
fn reprojection_residual(
    points: &[RawPixelMatch],
    layout: &PlaneLayout,
    base: &CameraParams,
    k1: f64,
    wh: (u32, u32),
    sigma: f64,
) -> f64 {
    let params = OptParams::from_5param(&[
        layout.x_ty,
        layout.intersect,
        layout.camera_axis_offset,
        layout.x_rz,
        layout.z_rx,
    ]);
    match map_matched(points, base, k1, wh) {
        Some(matched) if !matched.is_empty() => {
            geometry::seam_weighted_reprojection_error(&matched, &params, sigma)
        }
        _ => f64::INFINITY,
    }
}

/// Cost for the warm-started layout re-solve: the trimmed seam-weighted
/// reprojection error over `[cam_d, intersect, x_ty, x_rz, z_rx]` plus the
/// quadratic bounds penalty (mirrors [`crate::optimizer`]).
#[derive(Clone)]
struct LayoutCost<'a> {
    points: &'a [MatchedPoint],
    sigma: f64,
    trim_fraction: f64,
    bounds: [(f64, f64); 5],
}

impl CostFunction for LayoutCost<'_> {
    type Param = Vec<f64>;
    type Output = f64;

    fn cost(&self, p: &Self::Param) -> Result<Self::Output, Error> {
        let params = OptParams {
            cam_d: p[0],
            intersect: p[1],
            x_ty: p[2],
            x_rz: p[3],
            z_rx: p[4],
            z_rz: None,
            x_rx: None,
        };
        let err = if self.trim_fraction > 0.0 {
            geometry::trimmed_seam_weighted_reprojection_error(
                self.points,
                &params,
                self.sigma,
                self.trim_fraction,
            )
        } else {
            geometry::seam_weighted_reprojection_error(self.points, &params, self.sigma)
        };
        Ok(err + layout_bounds_penalty(p, &self.bounds))
    }
}

/// Quadratic penalty for layout parameters outside `bounds` (Nelder-Mead is
/// unconstrained).
fn layout_bounds_penalty(p: &[f64], bounds: &[(f64, f64); 5]) -> f64 {
    let mut penalty = 0.0;
    for (i, &val) in p.iter().enumerate().take(bounds.len()) {
        let (lo, hi) = bounds[i];
        if val < lo {
            let d = lo - val;
            penalty += BOUNDS_PENALTY_SCALE * d * d;
        } else if val > hi {
            let d = val - hi;
            penalty += BOUNDS_PENALTY_SCALE * d * d;
        }
    }
    penalty
}

/// Build a 6-vertex simplex around `start` (clamped into `bounds`), perturbing
/// each dimension by [`SIMPLEX_PERTURBATION`] of its range.
fn build_layout_simplex(start: &[f64], bounds: &[(f64, f64); 5]) -> Vec<Vec<f64>> {
    let clamped: Vec<f64> = start
        .iter()
        .zip(bounds.iter())
        .map(|(v, (lo, hi))| v.clamp(*lo, *hi))
        .collect();

    let mut vertices = Vec::with_capacity(6);
    vertices.push(clamped.clone());
    for (i, (lo, hi)) in bounds.iter().enumerate() {
        let mut vertex = clamped.clone();
        let delta = SIMPLEX_PERTURBATION * (hi - lo);
        if vertex[i] + delta <= *hi {
            vertex[i] += delta;
        } else {
            vertex[i] -= delta;
        }
        vertices.push(vertex);
    }
    vertices
}

/// Re-solve the layout warm-started from `start`, minimizing the trimmed
/// seam-weighted reprojection error on `points` (already at the current `k1`).
///
/// A single-start Nelder-Mead wrapper rather than [`crate::optimizer::optimize`]
/// because the layout optimizer exposes no starting-point/seed API, so it cannot
/// warm-start from the previous layout (see `crates/reco-calibrate/FRICTION.md`).
/// `x_rx`/`z_rz` are carried through unchanged — this driver never solves them.
fn solve_layout_warm(
    points: &[MatchedPoint],
    start: &PlaneLayout,
    cfg: &IntrinsicsConfig,
) -> Option<PlaneLayout> {
    if points.is_empty() {
        return None;
    }
    let start_vec = [
        start.camera_axis_offset,
        start.intersect,
        start.x_ty,
        start.x_rz,
        start.z_rx,
    ];
    let cost = LayoutCost {
        points,
        sigma: cfg.sigma,
        trim_fraction: cfg.trim_fraction,
        bounds: crate::optimizer::BOUNDS_5,
    };
    let simplex = build_layout_simplex(&start_vec, &crate::optimizer::BOUNDS_5);
    let solver = NelderMead::new(simplex).with_sd_tolerance(1e-12).ok()?;
    let res = Executor::new(cost, solver)
        .configure(|state| state.max_iters(cfg.max_iters as u64))
        .run()
        .ok()?;
    let best = res.state().get_best_param()?;

    Some(PlaneLayout {
        camera_axis_offset: best[0],
        intersect: best[1],
        x_ty: best[2],
        x_rz: best[3],
        z_rx: best[4],
        x_rx: start.x_rx,
        z_rz: start.z_rz,
    })
}

/// Refine `k1` against raw-pixel observations, composed with the layout solve
/// behind a held-out guard (INTR-02).
///
/// This is the phase's safety contract. It:
///
/// 1. runs the conditioning gate ([`conditioning`]) before any solve — an
///    ill-conditioned set returns `accepted == false` with a typed reason and
///    never reaches the solver;
/// 2. splits the observations into a deterministic fit/held-out partition
///    ([`split_fit_heldout`], ~80/20 from [`IntrinsicsConfig::seed`]);
/// 3. alternates (bounded by [`IntrinsicsConfig::max_rounds`]): a warm-started
///    layout solve on the fit set → [`optimize_intrinsics`] on the fit set at
///    that layout → a warm-started layout re-solve — stopping as soon as the
///    held-out residual stops improving;
/// 4. accepts the refinement only when the held-out reprojection error improves
///    by more than [`IntrinsicsConfig::improvement_epsilon`]. Otherwise the
///    result is `accepted == false` with [`RefinementReason::GuardRejected`] and
///    [`IntrinsicsRefinement::k1`] equals the baseline `base.d[0]`.
///
/// `base` is never mutated: the caller writes the returned `k1` into the
/// profile only when `accepted` is `true`. The layout is re-solved internally to
/// keep the `k1` stage coherent; only `k1` crosses back to the caller.
///
/// # Errors
///
/// * [`CalibrateError::InvalidConfig`] / [`CalibrateError::OptimizerFailed`] if
///   the reduced `k1` solver cannot be constructed or run. Conditioning
///   refusals are **not** errors — they are typed `accepted == false` results.
pub fn refine_intrinsics(
    points: &[RawPixelMatch],
    layout: &PlaneLayout,
    base: &CameraParams,
    cfg: &IntrinsicsConfig,
) -> Result<IntrinsicsRefinement, CalibrateError> {
    // (1) The conditioning gate runs before any solve: an ill-conditioned set
    //     is refused with a typed reason, never silently fitted.
    let cond = conditioning(points, layout, base, cfg.min_matches, cfg.min_spread);
    if !cond.well_conditioned {
        return Ok(rejected(
            base.d[0],
            0.0,
            0.0,
            0.0,
            conditioning_reason(&cond, layout, cfg),
        ));
    }

    // A fit/held-out split needs at least one observation on each side.
    if points.len() < 2 {
        return Ok(rejected(
            base.d[0],
            0.0,
            0.0,
            0.0,
            RefinementReason::InsufficientMatches,
        ));
    }

    let (fit, heldout) = split_fit_heldout(points, cfg);
    if fit.is_empty() || heldout.is_empty() {
        return Ok(rejected(
            base.d[0],
            0.0,
            0.0,
            0.0,
            RefinementReason::InsufficientMatches,
        ));
    }

    let wh = (base.width, base.height);

    // Baseline: the profile's k1 at the caller's layout (a coherent pair).
    let heldout_baseline = reprojection_residual(&heldout, layout, base, base.d[0], wh, cfg.sigma);
    let baseline_fit = reprojection_residual(&fit, layout, base, base.d[0], wh, cfg.sigma);

    let mut current_layout = layout.clone();
    let mut current_k1 = base.d[0];
    // (best k1, held-out residual, fit residual)
    let mut best: Option<(f64, f64, f64)> = None;

    for _ in 0..cfg.max_rounds.max(1) {
        // (2) Warm-started layout solve on the fit set at the current k1.
        let Some(fit_matched) = map_matched(&fit, base, current_k1, wh) else {
            break;
        };
        let Some(solved_layout) = solve_layout_warm(&fit_matched, &current_layout, cfg) else {
            break;
        };

        // (3) Refine k1 at that layout on the fit set.
        let refinement = optimize_intrinsics(&fit, &solved_layout, base, cfg)?;
        let candidate_k1 = refinement.k1;

        // (4) Re-solve the layout warm-started at the refined k1, so the layout
        //     never wanders and the held-out evaluation uses a coherent pair.
        let Some(refined_matched) = map_matched(&fit, base, candidate_k1, wh) else {
            break;
        };
        let final_layout =
            solve_layout_warm(&refined_matched, &solved_layout, cfg).unwrap_or(solved_layout);

        // (5) Held-out (and fit) residual at the refined k1 and re-solved layout.
        let heldout_refined =
            reprojection_residual(&heldout, &final_layout, base, candidate_k1, wh, cfg.sigma);
        let fit_refined =
            reprojection_residual(&fit, &final_layout, base, candidate_k1, wh, cfg.sigma);

        // Stop as soon as the held-out fit stops improving.
        let improves = best
            .as_ref()
            .is_none_or(|(_, h, _)| heldout_refined < *h - cfg.improvement_epsilon);
        if !improves {
            break;
        }
        best = Some((candidate_k1, heldout_refined, fit_refined));
        current_layout = final_layout;
        current_k1 = candidate_k1;
    }

    let Some((best_k1, heldout_refined, fit_residual)) = best else {
        return Ok(rejected(
            base.d[0],
            baseline_fit,
            heldout_baseline,
            heldout_baseline,
            RefinementReason::GuardRejected,
        ));
    };

    // Accept iff the held-out reprojection improved beyond epsilon. A tie or an
    // epsilon-sized improvement is a rejection: the profile's k1 is unchanged.
    if heldout_refined < heldout_baseline - cfg.improvement_epsilon {
        Ok(IntrinsicsRefinement {
            k1: best_k1,
            residual: fit_residual,
            accepted: true,
            reason: RefinementReason::Accepted,
            heldout_baseline,
            heldout_refined,
        })
    } else {
        Ok(rejected(
            base.d[0],
            baseline_fit,
            heldout_baseline,
            heldout_refined,
            RefinementReason::GuardRejected,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::plane_to_pixel;
    use approx::assert_abs_diff_eq;
    use rand::{Rng, SeedableRng};
    use reco_core::lens::undistorted_to_distorted;

    /// Synthetic test-rig resolution (both cameras share it).
    const LW: u32 = 1920;
    const LH: u32 = 1080;
    const RW: u32 = 1920;
    const RH: u32 = 1080;

    /// A truth rig consistent with the crate's ray-trace generator
    /// (rotations are zero because the generator does not encode them).
    fn truth() -> OptParams {
        OptParams {
            x_ty: 0.01,
            intersect: 0.55,
            cam_d: 0.24,
            x_rz: 0.0,
            z_rx: 0.0,
            z_rz: None,
            x_rx: None,
        }
    }

    /// Camera intrinsics for the harness; `k1` is the free variable.
    fn camera_params(k1: f64) -> CameraParams {
        CameraParams {
            width: LW,
            height: LH,
            fx: 898.16,
            fy: 898.16,
            cx: LW as f64 / 2.0,
            cy: LH as f64 / 2.0,
            // k2..k4 are fixed and shared by synthesis and solve, so only k1
            // is under test.
            d: [k1, 0.02, -0.01, 0.005],
        }
    }

    /// Ray-trace `n` plane-coordinate pairs exactly consistent with
    /// `true_params`. Mirrors `optimizer::tests::synthetic_points` (private to
    /// its module, so a small local copy keeps this module self-contained).
    fn synthetic_points(true_params: &OptParams, n: usize) -> Vec<MatchedPoint> {
        use crate::geometry::PLANE_WIDTH;

        let half_offset = PLANE_WIDTH / 2.0 * (1.0 - true_params.intersect);
        let cam = nalgebra::Vector3::new(true_params.cam_d, 0.0, true_params.cam_d);

        let mut points = Vec::with_capacity(n);
        let grid = (n as f64).sqrt().ceil() as usize;

        for iy in 0..grid {
            for ix in 0..grid {
                if points.len() >= n {
                    break;
                }
                let fx = (ix as f64 + 0.5) / grid as f64;
                let fy = (iy as f64 + 0.5) / grid as f64;

                let yaw = -0.9 + fx * 0.5;
                let pitch = (fy - 0.5) * 0.4;
                let d = nalgebra::Vector3::new(yaw, pitch, yaw - 0.3).normalize();

                if d.z.abs() < 1e-10 || d.x.abs() < 1e-10 {
                    continue;
                }
                let t_x = -cam.z / d.z;
                let t_z = -cam.x / d.x;
                if t_x < 0.0 || t_z < 0.0 {
                    continue;
                }

                let hit_x = cam + t_x * d;
                let hit_z = cam + t_z * d;

                let x_coord = hit_x.x - half_offset;
                let y_coord = -(hit_x.y - true_params.x_ty);
                let z_coord = -(hit_z.z - half_offset);
                let z_y = -hit_z.y;

                points.push(MatchedPoint::from_planes(
                    [x_coord, y_coord],
                    [z_coord, z_y],
                ));
            }
        }
        points
    }

    /// Synthesize raw distorted pixels from plane-coordinate pairs using the
    /// forward world→pixel chain: plane coord → undistorted pixel
    /// (`plane_to_pixel`) → raw pixel (`undistorted_to_distorted`).
    ///
    /// `.left` holds the right camera's plane coord (x-plane); `.right` holds
    /// the left camera's (z-plane). Only points whose undistorted pixels land
    /// inside the frame are kept, so the KB4 inverse stays convergent.
    fn raw_from_plane_points(points: &[MatchedPoint], synth: &CameraParams) -> Vec<RawPixelMatch> {
        let mut out = Vec::with_capacity(points.len());
        for mp in points {
            // Left camera pixel (z-plane coord is in `.right`).
            let left_und = plane_to_pixel(mp.right, LW, LH);
            // Right camera pixel (x-plane coord is in `.left`).
            let right_und = plane_to_pixel(mp.left, RW, RH);

            // Keep the observation inside the frame so the inverse is
            // well-conditioned (mirrors the pipeline's border filter).
            let inside = |p: [f64; 2], w: u32, h: u32| {
                let mx = 0.08 * w as f64;
                let my = 0.08 * h as f64;
                p[0] >= mx && p[0] <= w as f64 - mx && p[1] >= my && p[1] <= h as f64 - my
            };
            if !inside(left_und, LW, LH) || !inside(right_und, RW, RH) {
                continue;
            }

            let left_raw = undistorted_to_distorted(left_und[0], left_und[1], LW, LH, synth);
            let right_raw = undistorted_to_distorted(right_und[0], right_und[1], RW, RH, synth);

            out.push(RawPixelMatch {
                left_px: [left_raw.0, left_raw.1],
                right_px: [right_raw.0, right_raw.1],
            });
        }
        out
    }

    /// Add seeded Gaussian pixel noise to both coordinates of each match.
    fn add_noise(matches: &mut [RawPixelMatch], seed: u64, sigma_px: f64) {
        let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
        let gauss = |rng: &mut rand::rngs::SmallRng| {
            // Box-Muller from two uniforms.
            let u1: f64 = rng.random::<f64>().max(1e-12);
            let u2: f64 = rng.random::<f64>();
            (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
        };
        for m in matches.iter_mut() {
            for k in 0..2 {
                m.left_px[k] += sigma_px * gauss(&mut rng);
                m.right_px[k] += sigma_px * gauss(&mut rng);
            }
        }
    }

    /// An alternate truth rig: a modestly different baseline and intersection
    /// so the widened sweep exercises more than one geometry.
    fn alt_rig() -> OptParams {
        OptParams {
            x_ty: -0.015,
            intersect: 0.48,
            cam_d: 0.20,
            x_rz: 0.0,
            z_rx: 0.0,
            z_rz: None,
            x_rx: None,
        }
    }

    /// The fixed solve layout implied by a truth rig.
    fn layout_from(t: &OptParams) -> PlaneLayout {
        PlaneLayout {
            camera_axis_offset: t.cam_d,
            intersect: t.intersect,
            x_ty: t.x_ty,
            x_rz: t.x_rz,
            z_rx: t.z_rx,
            x_rx: 0.0,
            z_rz: 0.0,
        }
    }

    /// Build `n` observations whose pixels are placed by `place` in both
    /// cameras (used by the conditioning tests, which only care about the
    /// radial distribution).
    fn matches_from(place: impl Fn(usize) -> [f64; 2], n: usize) -> Vec<RawPixelMatch> {
        (0..n)
            .map(|i| {
                let p = place(i);
                RawPixelMatch {
                    left_px: p,
                    right_px: p,
                }
            })
            .collect()
    }

    /// The gate: recover a known `k1` from synthesized raw pixels.
    ///
    /// Returns the absolute recovery error `|recovered - true|`.
    fn recover_k1_error(seed: u64, noise_px: f64, true_k1: f64) -> f64 {
        recover_k1_error_with_rig(seed, noise_px, true_k1, &truth())
    }

    /// Recovery error for an arbitrary truth rig — the widened sweep varies
    /// the rig geometry, not just the seed and noise level.
    fn recover_k1_error_with_rig(seed: u64, noise_px: f64, true_k1: f64, t: &OptParams) -> f64 {
        let synth = camera_params(true_k1);
        // The solver starts from a deliberately wrong k1 (0.0) so recovery is
        // a genuine search, not a no-op.
        let base = camera_params(0.0);

        let plane_points = synthetic_points(t, 196);
        assert!(
            plane_points.len() >= 40,
            "not enough synthetic points: {}",
            plane_points.len()
        );
        let mut raw = raw_from_plane_points(&plane_points, &synth);
        assert!(
            raw.len() >= 24,
            "not enough in-frame observations: {}",
            raw.len()
        );
        add_noise(&mut raw, seed, noise_px);

        let layout = layout_from(t);

        let cfg = IntrinsicsConfig::default();
        let refinement =
            optimize_intrinsics(&raw, &layout, &base, &cfg).expect("reduced solve should succeed");
        (refinement.k1 - true_k1).abs()
    }

    /// Test 1 (the gate): a known `k1` is recovered within tolerance across
    /// >=3 seeds and >=2 noise levels, before any real-footage claim.
    #[test]
    fn recovers_known_k1_across_seeds_and_noise() {
        let true_k1 = 0.15;
        // Tolerance scaled to the noise floor; the sensitivity table measures
        // ~1000 px of raw displacement per unit k1, so a few px of noise
        // leaves the recovery well inside this band.
        let cases = [
            (1u64, 0.25_f64, 0.05_f64),
            (2, 0.25, 0.05),
            (3, 0.25, 0.05),
            (1, 1.5, 0.10),
            (2, 1.5, 0.10),
            (3, 1.5, 0.10),
        ];
        for (seed, noise, tol) in cases {
            let err = recover_k1_error(seed, noise, true_k1);
            assert!(
                err <= tol,
                "seed={seed} noise={noise}: |recovered - true| = {err} > {tol}"
            );
        }
    }

    /// Test 2 (forward/inverse identity): `distorted_to_undistorted` is the
    /// point-wise inverse of `undistorted_to_distorted` on a pixel grid across
    /// several aspect ratios. Fails loudly if either map is broken.
    #[test]
    fn distorted_to_undistorted_inverts_undistorted_to_distorted() {
        for &(w, h) in &[
            (1920_u32, 1080_u32),
            (3840, 2160),
            (1280, 720),
            (1000, 1000),
        ] {
            let params = CameraParams {
                width: w,
                height: h,
                fx: 0.4677 * w as f64,
                fy: 0.4677 * w as f64,
                cx: w as f64 / 2.0,
                cy: h as f64 / 2.0,
                d: [0.12, 0.02, -0.01, 0.005],
            };
            let steps = 8;
            for ix in 0..=steps {
                for iy in 0..=steps {
                    let out_x = (0.15 + 0.70 * ix as f64 / steps as f64) * w as f64;
                    let out_y = (0.15 + 0.70 * iy as f64 / steps as f64) * h as f64;

                    let (raw_x, raw_y) = undistorted_to_distorted(out_x, out_y, w, h, &params);
                    let (back_x, back_y) = distorted_to_undistorted(raw_x, raw_y, w, h, &params)
                        .expect("inverse should converge inside the frame");

                    assert_abs_diff_eq!(back_x, out_x, epsilon = 1e-6);
                    assert_abs_diff_eq!(back_y, out_y, epsilon = 1e-6);
                }
            }
        }
    }

    /// Test 3 (fixed params): only `k1` moves. The base profile's
    /// `fx/fy/cx/cy/cam_d` are bit-identical before and after the solve, while
    /// `k1` is genuinely updated.
    #[test]
    fn optimize_intrinsics_moves_only_k1() {
        let t = truth();
        let true_k1 = 0.18;
        let synth = camera_params(true_k1);
        let base = camera_params(0.0);
        let base_before = base.clone();

        let plane_points = synthetic_points(&t, 196);
        let raw = raw_from_plane_points(&plane_points, &synth);
        let layout = PlaneLayout {
            camera_axis_offset: t.cam_d,
            intersect: t.intersect,
            x_ty: t.x_ty,
            x_rz: t.x_rz,
            z_rx: t.z_rx,
            x_rx: 0.0,
            z_rz: 0.0,
        };

        let refinement = optimize_intrinsics(&raw, &layout, &base, &IntrinsicsConfig::default())
            .expect("solve should succeed");

        // k1 moved away from the (wrong) starting value...
        assert!(
            (refinement.k1 - base.d[0]).abs() > 1e-3,
            "k1 should be updated, got {}",
            refinement.k1
        );
        // ...but every fixed intrinsic is bit-identical.
        assert_eq!(base.fx.to_bits(), base_before.fx.to_bits());
        assert_eq!(base.fy.to_bits(), base_before.fy.to_bits());
        assert_eq!(base.cx.to_bits(), base_before.cx.to_bits());
        assert_eq!(base.cy.to_bits(), base_before.cy.to_bits());
        assert_eq!(base.d[1], base_before.d[1]);
        assert_eq!(base.d[2], base_before.d[2]);
        assert_eq!(base.d[3], base_before.d[3]);
        // The refined k1 stays inside the configured bound.
        assert!((refinement.k1 - base.d[0]).abs() <= IntrinsicsConfig::default().k1_bound + 1e-9);
    }

    /// Test 4 (empty): an empty observation set is a typed error, never a
    /// zero-`k1` result.
    #[test]
    fn empty_observation_set_is_a_typed_error() {
        let layout = PlaneLayout {
            camera_axis_offset: 0.24,
            intersect: 0.55,
            x_ty: 0.0,
            x_rz: 0.0,
            z_rx: 0.0,
            x_rx: 0.0,
            z_rz: 0.0,
        };
        let err = optimize_intrinsics(
            &[],
            &layout,
            &camera_params(0.0),
            &IntrinsicsConfig::default(),
        )
        .expect_err("an empty set must not solve");

        match err {
            CalibrateError::InsufficientMatches { got, min } => {
                assert_eq!(got, 0);
                assert_eq!(min, 1);
            }
            other => panic!("expected InsufficientMatches, got {other:?}"),
        }
    }

    /// Test 5 (noise monotonicity): recovery error grows with the noise level —
    /// a sanity check that the harness is not trivially exact.
    #[test]
    fn recovery_error_grows_with_noise() {
        let true_k1 = 0.15;
        let mean_err = |noise: f64| {
            let sum: f64 = (1u64..=4)
                .map(|seed| recover_k1_error(seed, noise, true_k1))
                .sum();
            sum / 4.0
        };
        let low = mean_err(0.1);
        let high = mean_err(3.0);
        assert!(
            high > low,
            "recovery error should grow with noise: low={low}, high={high}"
        );
    }

    /// The swap guard: `raw_to_matched_point` uses the pipeline's swap, so a
    /// noiseless round-trip through the forward model recovers the source
    /// plane coordinates exactly.
    #[test]
    fn raw_to_matched_point_round_trips_the_forward_model() {
        let t = truth();
        let synth = camera_params(0.12);

        let mut checked = 0usize;
        for mp in synthetic_points(&t, 196) {
            let left_und = plane_to_pixel(mp.right, LW, LH);
            let right_und = plane_to_pixel(mp.left, RW, RH);
            let inside = |p: [f64; 2], w: u32, h: u32| {
                let mx = 0.08 * w as f64;
                let my = 0.08 * h as f64;
                p[0] >= mx && p[0] <= w as f64 - mx && p[1] >= my && p[1] <= h as f64 - my
            };
            if !inside(left_und, LW, LH) || !inside(right_und, RW, RH) {
                continue;
            }

            let left_raw = undistorted_to_distorted(left_und[0], left_und[1], LW, LH, &synth);
            let right_raw = undistorted_to_distorted(right_und[0], right_und[1], RW, RH, &synth);
            let raw = RawPixelMatch {
                left_px: [left_raw.0, left_raw.1],
                right_px: [right_raw.0, right_raw.1],
            };

            let back = raw_to_matched_point(&raw, &synth, (LW, LH), (RW, RH))
                .expect("inverse should converge");
            // `.left` is the right camera plane; `.right` the left camera plane.
            assert!((back.left[0] - mp.left[0]).abs() < 1e-6);
            assert!((back.left[1] - mp.left[1]).abs() < 1e-6);
            assert!((back.right[0] - mp.right[0]).abs() < 1e-6);
            assert!((back.right[1] - mp.right[1]).abs() < 1e-6);
            checked += 1;
        }
        assert!(checked >= 24, "not enough in-frame observations: {checked}");
    }

    /// A full-frame spread set (both axes), used by the conditioning tests.
    fn spread_matches(n: usize) -> Vec<RawPixelMatch> {
        matches_from(
            |i| {
                let col = i % 8;
                let row = i / 8;
                [120.0 + col as f64 * 240.0, 100.0 + row as f64 * 220.0]
            },
            n,
        )
    }

    /// The conditioning gate refuses a centre-weighted set *before* any solve:
    /// with no radial leverage, `k1` cannot be constrained (T-04.2-05).
    #[test]
    fn conditioning_rejects_centre_weighted_set() {
        let points = matches_from(
            |i| {
                let a = i as f64 * 0.7;
                [960.0 + 40.0 * a.cos(), 540.0 + 40.0 * a.sin()]
            },
            40,
        );
        let c = conditioning(
            &points,
            &layout_from(&truth()),
            &camera_params(0.0),
            RECOMMENDED_MIN_MATCHES,
            RECOMMENDED_MIN_SPREAD,
        );
        assert_eq!(c.match_count, 40);
        assert!(
            c.radial_spread < 0.10,
            "centre-weighted spread should be tiny, got {}",
            c.radial_spread
        );
        assert!(
            !c.well_conditioned,
            "a centre-weighted set must be refused, never solved"
        );
    }

    /// The conditioning gate accepts a set spread across the frame.
    #[test]
    fn conditioning_accepts_spread_set() {
        let points = spread_matches(40);
        let c = conditioning(
            &points,
            &layout_from(&truth()),
            &camera_params(0.0),
            RECOMMENDED_MIN_MATCHES,
            RECOMMENDED_MIN_SPREAD,
        );
        assert_eq!(c.match_count, 40);
        assert!(
            c.radial_spread >= RECOMMENDED_MIN_SPREAD,
            "a full-frame spread should clear the threshold, got {}",
            c.radial_spread
        );
        assert!(c.well_conditioned, "a spread set should be accepted");
    }

    /// A well-spread but too-small set is refused on the count gate, and an
    /// empty set is refused on both gates without dividing by zero.
    #[test]
    fn conditioning_rejects_too_few_and_empty_sets() {
        let few = spread_matches(10);
        let c = conditioning(
            &few,
            &layout_from(&truth()),
            &camera_params(0.0),
            RECOMMENDED_MIN_MATCHES,
            RECOMMENDED_MIN_SPREAD,
        );
        assert_eq!(c.match_count, 10);
        assert!(!c.well_conditioned, "10 < min_matches must be refused");

        let empty = conditioning(
            &[],
            &layout_from(&truth()),
            &camera_params(0.0),
            RECOMMENDED_MIN_MATCHES,
            RECOMMENDED_MIN_SPREAD,
        );
        assert_eq!(empty.match_count, 0);
        assert_eq!(empty.radial_spread, 0.0);
        assert!(!empty.well_conditioned);
    }

    /// A non-finite layout cannot host a solve and is refused even with a
    /// well-spread set (the gate is evaluated in the solve's context).
    #[test]
    fn conditioning_refuses_non_finite_layout() {
        let points = spread_matches(40);
        let mut layout = layout_from(&truth());
        layout.camera_axis_offset = f64::NAN;
        let c = conditioning(
            &points,
            &layout,
            &camera_params(0.0),
            RECOMMENDED_MIN_MATCHES,
            RECOMMENDED_MIN_SPREAD,
        );
        assert!(c.radial_spread >= RECOMMENDED_MIN_SPREAD);
        assert!(!c.well_conditioned, "a non-finite layout must be refused");
    }

    /// Test 1, widened (the robustness gate): a known `k1` is recovered within
    /// tolerance across >= 5 seeds, >= 3 noise levels, and >= 2 rig geometries
    /// — the INTR-03 claim is stated over a sweep, not one lucky configuration.
    #[test]
    fn recovers_known_k1_across_wide_sweep() {
        let true_k1 = 0.15;
        let rigs = [truth(), alt_rig()];
        let noises = [0.10_f64, 0.25, 1.5];
        for (ri, rig) in rigs.iter().enumerate() {
            for &noise in &noises {
                for seed in 1u64..=5 {
                    let err = recover_k1_error_with_rig(seed, noise, true_k1, rig);
                    // Tolerance scales with the noise floor; the sensitivity
                    // table measures ~1000 px of raw displacement per unit k1,
                    // so a few px of noise leaves recovery well inside this band.
                    let tol = if noise >= 1.0 { 0.12 } else { 0.06 };
                    assert!(
                        err <= tol,
                        "rig={ri} seed={seed} noise={noise}: \
                         |recovered - true| = {err} > {tol}"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // `k2` observability experiment (Task 2)
    //
    // Production stays a 1-parameter solve: `k2` is NEVER in the free vector.
    // This experiment asks whether it *could* be: it synthesizes a known
    // `(k1, k2)` and attempts a joint `(k1, k2)` recovery at a realistic
    // feature spread, then measures the recovery error and the k1/k2 coupling.
    // The recorded go/no-go decision (04.2-GO-NO-GO.md) is asserted here.
    // -----------------------------------------------------------------------

    /// Camera intrinsics with an explicit `(k1, k2)` pair; `k3/k4` stay fixed
    /// and shared by synthesis and solve, so only the two radial terms vary.
    fn camera_params_k2(k1: f64, k2: f64) -> CameraParams {
        let mut p = camera_params(k1);
        p.d[1] = k2;
        p
    }

    /// Objective of a joint `(k1, k2)` solve at a fixed layout.
    ///
    /// Shared by the test-local joint cost and the finite-difference Hessian
    /// used to measure the k1/k2 coupling.
    fn joint_objective(
        points: &[RawPixelMatch],
        base: &CameraParams,
        layout: &OptParams,
        sigma: f64,
        trim_fraction: f64,
        k1: f64,
        k2: f64,
    ) -> f64 {
        let mut params = base.clone();
        params.d[0] = k1;
        params.d[1] = k2;
        let wh = (base.width, base.height);
        let mut matched = Vec::with_capacity(points.len());
        for raw in points {
            match raw_to_matched_point(raw, &params, wh, wh) {
                Some(mp) => matched.push(mp),
                None => return OUT_OF_DOMAIN_COST,
            }
        }
        if trim_fraction > 0.0 {
            geometry::trimmed_seam_weighted_reprojection_error(
                &matched,
                layout,
                sigma,
                trim_fraction,
            )
        } else {
            geometry::seam_weighted_reprojection_error(&matched, layout, sigma)
        }
    }

    /// Test-local 2-parameter `(k1, k2)` cost — the joint solve the experiment
    /// runs. Mirrors the production 1-D [`IntrinsicsCost`] with a second free
    /// radial term; production never frees `k2`.
    #[derive(Clone)]
    struct JointK2Cost<'a> {
        points: &'a [RawPixelMatch],
        base: &'a CameraParams,
        layout: OptParams,
        sigma: f64,
        trim_fraction: f64,
        bounds: [(f64, f64); 2],
    }

    impl CostFunction for JointK2Cost<'_> {
        type Param = Vec<f64>;
        type Output = f64;

        fn cost(&self, p: &Self::Param) -> Result<Self::Output, Error> {
            let obj = joint_objective(
                self.points,
                self.base,
                &self.layout,
                self.sigma,
                self.trim_fraction,
                p[0],
                p[1],
            );
            Ok(obj
                + k1_bounds_penalty(p[0], self.bounds[0])
                + k1_bounds_penalty(p[1], self.bounds[1]))
        }
    }

    /// Jointly solve `(k1, k2)` from a deliberately wrong start `(0, 0)`,
    /// bounded to `±k1_bound` around each starting value.
    fn joint_solve_k2(
        points: &[RawPixelMatch],
        layout: &OptParams,
        base: &CameraParams,
        cfg: &IntrinsicsConfig,
    ) -> (f64, f64) {
        let range = cfg.k1_bound;
        let bounds = [
            (base.d[0] - range, base.d[0] + range),
            (base.d[1] - range, base.d[1] + range),
        ];
        let cost = JointK2Cost {
            points,
            base,
            layout: *layout,
            sigma: cfg.sigma,
            trim_fraction: cfg.trim_fraction,
            bounds,
        };
        let start = [
            base.d[0].clamp(bounds[0].0, bounds[0].1),
            base.d[1].clamp(bounds[1].0, bounds[1].1),
        ];
        let p = SIMPLEX_PERTURBATION * (bounds[0].1 - bounds[0].0);
        let simplex = vec![
            vec![start[0], start[1]],
            vec![(start[0] + p).min(bounds[0].1), start[1]],
            vec![start[0], (start[1] + p).min(bounds[1].1)],
        ];
        let solver = NelderMead::new(simplex)
            .with_sd_tolerance(1e-12)
            .expect("joint simplex should construct");
        let result = Executor::new(cost, solver)
            .configure(|s| s.max_iters(cfg.max_iters as u64))
            .run()
            .expect("joint solve should run");
        let best = result
            .state()
            .get_best_param()
            .expect("joint solve should have a best");
        (best[0], best[1])
    }

    /// Central-difference Hessian of the smooth (untrimmed) joint objective at
    /// `(k1, k2)`. Its off-diagonal correlation and eigenvalue ratio are the
    /// k1/k2 coupling evidence.
    fn joint_hessian(
        points: &[RawPixelMatch],
        base: &CameraParams,
        layout: &OptParams,
        sigma: f64,
        k1: f64,
        k2: f64,
    ) -> [[f64; 2]; 2] {
        let h = 1e-4;
        let f = |a: f64, b: f64| joint_objective(points, base, layout, sigma, 0.0, a, b);
        let f00 = f(k1, k2);
        let h11 = (f(k1 + h, k2) - 2.0 * f00 + f(k1 - h, k2)) / (h * h);
        let h22 = (f(k1, k2 + h) - 2.0 * f00 + f(k1, k2 - h)) / (h * h);
        let h12 = (f(k1 + h, k2 + h) - f(k1 + h, k2 - h) - f(k1 - h, k2 + h) + f(k1 - h, k2 - h))
            / (4.0 * h * h);
        [[h11, h12], [h12, h22]]
    }

    /// Absolute k1/k2 correlation `|rho|` and Hessian condition number from a
    /// coupling matrix.
    fn coupling(h: &[[f64; 2]; 2]) -> (f64, f64) {
        let rho = (h[0][1] / (h[0][0].abs() * h[1][1].abs()).sqrt()).abs();
        let trace = h[0][0] + h[1][1];
        let det = h[0][0] * h[1][1] - h[0][1] * h[0][1];
        let disc = (trace * trace - 4.0 * det).max(0.0).sqrt();
        let cond = (trace + disc) / (trace - disc).max(1e-30);
        (rho, cond)
    }

    /// Run the joint `(k1, k2)` experiment for one seed at a realistic feature
    /// spread (the pipeline's 8% border margin) plus pixel noise. Returns
    /// `(k1_err, k2_err, |rho|, condition)`.
    fn k2_experiment(seed: u64, noise_px: f64) -> (f64, f64, f64, f64) {
        let t = truth();
        let (k1_true, k2_true) = (0.15, 0.02);
        let synth = camera_params_k2(k1_true, k2_true);
        let base = camera_params_k2(0.0, 0.0);
        let layout = layout_from(&t);
        let layout_params = OptParams::from_5param(&[
            layout.x_ty,
            layout.intersect,
            layout.camera_axis_offset,
            layout.x_rz,
            layout.z_rx,
        ]);

        let plane_points = synthetic_points(&t, 196);
        let mut raw = raw_from_plane_points(&plane_points, &synth);
        assert!(raw.len() >= 24, "not enough in-frame observations");
        add_noise(&mut raw, seed, noise_px);

        let cfg = IntrinsicsConfig::default();
        let (k1, k2) = joint_solve_k2(&raw, &layout_params, &base, &cfg);
        let h = joint_hessian(&raw, &base, &layout_params, cfg.sigma, k1, k2);
        let (rho, cond) = coupling(&h);
        ((k1 - k1_true).abs(), (k2 - k2_true).abs(), rho, cond)
    }

    /// Joint `(k1, k2)` recovery under a small k3/k4 model mismatch between
    /// synthesis and the (fixed) solve values. A well-determined parameter
    /// survives; a near-degenerate one absorbs the mismatch and blows up.
    /// Returns `(k2_err, k1_err)`.
    fn k2_mismatch_experiment(seed: u64) -> (f64, f64) {
        let t = truth();
        let layout = layout_from(&t);
        let layout_params = OptParams::from_5param(&[
            layout.x_ty,
            layout.intersect,
            layout.camera_axis_offset,
            layout.x_rz,
            layout.z_rx,
        ]);
        let cfg = IntrinsicsConfig::default();
        // Truth carries higher-order terms the solve fixes to different values.
        let mut synth = camera_params_k2(0.15, 0.02);
        synth.d[2] = 0.03;
        synth.d[3] = 0.01;
        let base = camera_params_k2(0.0, 0.0); // fixed k3=-0.01, k4=0.005

        let mut raw = raw_from_plane_points(&synthetic_points(&t, 196), &synth);
        add_noise(&mut raw, seed, 0.25);
        let (k1, k2) = joint_solve_k2(&raw, &layout_params, &base, &cfg);
        ((k2 - 0.02).abs(), (k1 - 0.15).abs())
    }

    /// The `k2` observability experiment (INTR-03 / ROADMAP criterion 5): a
    /// joint `(k1, k2)` solve is attempted and its coupling is measured. The
    /// recorded decision (04.2-GO-NO-GO.md) is **DEFER** — `k2` is not
    /// independently observable on this rig — and this test asserts that
    /// outcome, not a wish:
    ///
    /// 1. A self-consistent synthetic model lets a joint solve recover both,
    ///    but the k1/k2 directions are near-perfectly collinear: `|rho| ≈
    ///    0.9955` with a Hessian condition number `≈ 4.6e2` (variance
    ///    inflation `1/(1-rho²) ≈ 110×`). The two parameters are not
    ///    separately identifiable.
    /// 2. Under a realistic k3/k4 model mismatch, the joint `k2` estimate
    ///    absorbs the model error and is off by several times its own value —
    ///    no useful signal-to-noise margin.
    ///
    /// Production therefore keeps `k2` OUT of [`IntrinsicsConfig`]'s free
    /// vector; only `k1` is ever solved.
    #[test]
    fn k2_is_not_independently_observable() {
        let n = 5u64;

        // (1) Coupling on a self-consistent model.
        let (mut k1_sum, mut rho_sum, mut cond_sum) = (0.0, 0.0, 0.0);
        for seed in 1..=n {
            let (k1e, _k2e, rho, cond) = k2_experiment(seed, 0.25);
            k1_sum += k1e;
            rho_sum += rho;
            cond_sum += cond;
        }
        let mean_k1 = k1_sum / n as f64;
        let mean_rho = rho_sum / n as f64;
        let mean_cond = cond_sum / n as f64;
        assert!(
            mean_k1 < 0.02,
            "the joint solve should still recover k1: k1err={mean_k1}"
        );
        assert!(
            mean_rho > 0.95,
            "k1/k2 must be near-collinear (degenerate): |rho|={mean_rho}"
        );
        assert!(
            mean_cond > 100.0,
            "the k1/k2 design must be ill-conditioned: cond={mean_cond}"
        );

        // (2) No useful margin under a realistic model mismatch.
        let (mut k2_mis_sum, mut k1_mis_sum) = (0.0, 0.0);
        for seed in 1..=n {
            let (k2e, k1e) = k2_mismatch_experiment(seed);
            k2_mis_sum += k2e;
            k1_mis_sum += k1e;
        }
        let mean_k2_mis = k2_mis_sum / n as f64;
        let mean_k1_mis = k1_mis_sum / n as f64;
        assert!(
            mean_k2_mis > 0.05,
            "under model mismatch k2 must not be usefully recovered: k2err={mean_k2_mis}"
        );
        assert!(
            mean_k1_mis < 0.06,
            "the same data should still pin k1 to the same order: k1err={mean_k1_mis}"
        );
    }

    // -----------------------------------------------------------------------
    // Alternating layout↔k1 driver + held-out guard (INTR-02)
    // -----------------------------------------------------------------------

    /// A pool of in-frame raw observations synthesized from `t` at `k1`, with
    /// seeded pixel noise. Large enough to slice fit/held-out indices from.
    fn observation_pool(
        t: &OptParams,
        k1: f64,
        n: usize,
        seed: u64,
        noise_px: f64,
    ) -> Vec<RawPixelMatch> {
        let synth = camera_params(k1);
        let mut raw = raw_from_plane_points(&synthetic_points(t, n), &synth);
        add_noise(&mut raw, seed, noise_px);
        raw
    }

    /// The synthetic observation pool clears the conditioning gate, so the
    /// driver's accept/reject tests exercise the held-out guard, not the gate.
    #[test]
    fn synthetic_pool_passes_conditioning() {
        let raw = observation_pool(&truth(), 0.15, 400, 1, 0.25);
        let c = conditioning(
            &raw,
            &layout_from(&truth()),
            &camera_params(0.0),
            RECOMMENDED_MIN_MATCHES,
            RECOMMENDED_MIN_SPREAD,
        );
        assert!(
            c.well_conditioned,
            "synthetic pool must be conditioned: count={}, spread={}",
            c.match_count, c.radial_spread
        );
    }

    /// Test 1 (accepted): a known rig + known `k1` is refined and accepted; the
    /// recovered `k1` is within tolerance and the held-out fit improves.
    #[test]
    fn refine_intrinsics_accepts_a_genuine_improvement() {
        let t = truth();
        let true_k1 = 0.15;
        // The profile starts 0.05 low; the layout is the rig's (near the layout
        // solved at the wrong profile k1) — the real-flow case.
        let base = camera_params(0.10);
        let base_before = base.clone();

        let raw = observation_pool(&t, true_k1, 400, 7, 0.25);
        let cfg = IntrinsicsConfig::default();
        let r = refine_intrinsics(&raw, &layout_from(&t), &base, &cfg).expect("driver should run");

        assert!(r.accepted, "a genuine refinement must be accepted: {r:?}");
        assert_eq!(r.reason, RefinementReason::Accepted);
        assert!(
            (r.k1 - true_k1).abs() <= 0.05,
            "recovered k1 = {} (true {true_k1})",
            r.k1
        );
        assert!(
            r.heldout_refined < r.heldout_baseline,
            "held-out must improve on acceptance: before={} after={}",
            r.heldout_baseline,
            r.heldout_refined
        );
        // The caller's profile is untouched.
        assert_eq!(base.d[0].to_bits(), base_before.d[0].to_bits());
    }

    /// Test 2 (guard rejected): a fit set that improves but a held-out set drawn
    /// from a different `k1` regresses — the overfitting guard rejects it and
    /// returns the baseline `k1` (INTR-02, T-04.2-07).
    #[test]
    fn refine_intrinsics_rejects_held_out_regression() {
        let t = truth();
        let cfg = IntrinsicsConfig::default();
        let n = 120usize;

        // Fit observations from k1 = +0.2; held-out from k1 = −0.2 (a different
        // lens). Refining k1 to fit the fit set must worsen the held-out set.
        let fit_pool = observation_pool(&t, 0.2, 900, 11, 0.1);
        let held_pool = observation_pool(&t, -0.2, 900, 12, 0.1);
        assert!(
            fit_pool.len() >= n && held_pool.len() >= n,
            "pools too small: fit={} held={}",
            fit_pool.len(),
            held_pool.len()
        );

        let (fit_idx, held_idx) = split_indices(n, &cfg);
        let mut points = vec![fit_pool[0]; n];
        for (k, &i) in fit_idx.iter().enumerate() {
            points[i] = fit_pool[k];
        }
        for (k, &i) in held_idx.iter().enumerate() {
            points[i] = held_pool[k];
        }

        let base = camera_params(0.0);
        let r =
            refine_intrinsics(&points, &layout_from(&t), &base, &cfg).expect("driver should run");

        assert!(!r.accepted, "a held-out regression must be rejected: {r:?}");
        assert_eq!(r.reason, RefinementReason::GuardRejected);
        assert_eq!(
            r.k1, base.d[0],
            "a rejected refinement must return the baseline k1"
        );
    }

    /// Test 3 (conditioning): a centre-weighted set is refused with the
    /// ill-conditioned reason and never reaches the solve (INTR-02 / plan 02).
    #[test]
    fn refine_intrinsics_refuses_ill_conditioned_without_solving() {
        let base = camera_params(0.0);
        let points = matches_from(
            |i| {
                let a = i as f64 * 0.7;
                [960.0 + 40.0 * a.cos(), 540.0 + 40.0 * a.sin()]
            },
            60,
        );
        let r = refine_intrinsics(
            &points,
            &layout_from(&truth()),
            &base,
            &IntrinsicsConfig::default(),
        )
        .expect("a conditioning refusal is a typed result, not an error");

        assert!(!r.accepted);
        assert_eq!(r.reason, RefinementReason::NotEnoughSpread);
        assert_eq!(r.k1, base.d[0], "a refusal leaves the profile k1 unchanged");
    }

    /// Test 4 (never-modify): the caller's profile is never mutated by the
    /// engine solve — the result is advisory (INTR-02, T-04.2-09).
    #[test]
    fn refine_intrinsics_never_mutates_the_base_profile() {
        let t = truth();
        let base = camera_params(0.10);
        let before = base.clone();
        let raw = observation_pool(&t, 0.15, 400, 5, 0.25);

        let _ = refine_intrinsics(&raw, &layout_from(&t), &base, &IntrinsicsConfig::default())
            .expect("driver should run");

        assert_eq!(base.d[0].to_bits(), before.d[0].to_bits());
        assert_eq!(base.fx.to_bits(), before.fx.to_bits());
        assert_eq!(base.fy.to_bits(), before.fy.to_bits());
        assert_eq!(base.cx.to_bits(), before.cx.to_bits());
        assert_eq!(base.cy.to_bits(), before.cy.to_bits());
        assert_eq!(base.width, before.width);
        assert_eq!(base.height, before.height);
    }

    /// Test 5 (no-regression): an accepted result never worsens the held-out
    /// residual by less than the epsilon (INTR-02, T-04.2-08).
    #[test]
    fn refine_intrinsics_accepted_result_never_regresses_held_out() {
        let t = truth();
        let base = camera_params(0.10);
        let raw = observation_pool(&t, 0.15, 400, 9, 0.25);
        let cfg = IntrinsicsConfig::default();

        let r = refine_intrinsics(&raw, &layout_from(&t), &base, &cfg).expect("driver should run");

        assert!(r.accepted, "this setup must be accepted: {r:?}");
        assert!(
            r.heldout_refined < r.heldout_baseline - cfg.improvement_epsilon,
            "an accepted result must improve the held-out fit beyond epsilon: \
             before={} after={}",
            r.heldout_baseline,
            r.heldout_refined
        );
    }

    /// Test 6 (no-improvement): a refinement that cannot improve the held-out
    /// fit (the profile is already at the optimum) is a rejection, not an
    /// acceptance (INTR-02 edge probe "no-improvement").
    #[test]
    fn refine_intrinsics_rejects_a_non_improving_refinement() {
        let t = truth();
        let true_k1 = 0.15;
        let base = camera_params(true_k1); // already at the optimum
        let raw = observation_pool(&t, true_k1, 400, 3, 0.0); // clean data

        let r = refine_intrinsics(&raw, &layout_from(&t), &base, &IntrinsicsConfig::default())
            .expect("driver should run");

        assert!(
            !r.accepted,
            "an already-optimal profile must not be 'refined': {r:?}"
        );
        assert_eq!(r.reason, RefinementReason::GuardRejected);
        assert_eq!(r.k1, base.d[0]);
    }
}
