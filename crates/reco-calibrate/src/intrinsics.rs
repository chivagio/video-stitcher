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

    /// The gate: recover a known `k1` from synthesized raw pixels.
    ///
    /// Returns the absolute recovery error `|recovered - true|`.
    fn recover_k1_error(seed: u64, noise_px: f64, true_k1: f64) -> f64 {
        let t = truth();
        let synth = camera_params(true_k1);
        // The solver starts from a deliberately wrong k1 (0.0) so recovery is
        // a genuine search, not a no-op.
        let base = camera_params(0.0);

        let plane_points = synthetic_points(&t, 196);
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

        let layout = PlaneLayout {
            camera_axis_offset: t.cam_d,
            intersect: t.intersect,
            x_ty: t.x_ty,
            x_rz: t.x_rz,
            z_rx: t.z_rx,
            x_rx: 0.0,
            z_rz: 0.0,
        };

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
}
