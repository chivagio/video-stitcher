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
use crate::geometry::{normalize_to_plane, plane_to_pixel};
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

/// Reconstruct natural-order pixel pins from verified optimizer-space matches
/// (MANU-04).
///
/// A verified [`MatchedPoint`] is post-RANSAC and already in plane coordinates
/// from the pipeline's swap-correct path: `.left` holds the **right** camera's
/// plane coordinate and `.right` the **left** camera's (see the module header).
/// This is the inverse of [`pin_to_matched_point`]: it maps each plane
/// coordinate back to the pixel the operator would have clicked, so the seeded
/// pins round-trip through the same swap-correct helper.
///
/// Only geometrically verified matches are ever passed here; raw or rejected
/// candidates must never reach the editor (MANU-04 prohibition).
#[must_use]
pub fn seed_pins_from_verified(
    points: &[MatchedPoint],
    left_wh: (u32, u32),
    right_wh: (u32, u32),
) -> Vec<ManualPin> {
    points
        .iter()
        .map(|p| ManualPin {
            // `.right` is the LEFT camera's plane coord; `.left` is the RIGHT's.
            left_px: plane_to_pixel(p.right, left_wh.0, left_wh.1),
            right_px: plane_to_pixel(p.left, right_wh.0, right_wh.1),
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::OptParams;
    use approx::assert_abs_diff_eq;

    /// Synthetic test-rig resolution (both cameras share it, like a matched pair).
    const LW: u32 = 1920;
    const LH: u32 = 1080;
    const RW: u32 = 1920;
    const RH: u32 = 1080;

    /// A truth rig consistent with the crate's ray-trace generator (rotations
    /// are zero because the generator does not encode them).
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

    /// Ray-trace `n` plane-coordinate pairs exactly consistent with `true_params`.
    ///
    /// Mirrors `optimizer::tests::synthetic_points` (that helper is private to
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

    /// Inverse of [`normalize_to_plane`]: a plane coordinate back to pixels.
    fn plane_to_px(plane: [f64; 2], w: u32, h: u32) -> [f64; 2] {
        let wf = w as f64;
        let hf = h as f64;
        [(plane[0] + 0.5) * wf, (plane[1] * wf / hf + 0.5) * hf]
    }

    /// Reconstruct natural-order pixel pins from optimizer-space points, so the
    /// swap-correct [`pin_to_matched_point`] round-trips back to the same points.
    fn pins_from_points(points: &[MatchedPoint]) -> Vec<ManualPin> {
        points
            .iter()
            .map(|p| ManualPin {
                // `.left` holds the right camera's pixel (x-plane);
                // `.right` holds the left camera's pixel (z-plane).
                right_px: plane_to_px(p.left, RW, RH),
                left_px: plane_to_px(p.right, LW, LH),
            })
            .collect()
    }

    /// Guards the swap: swap-correct pins recover the synthetic truth.
    #[test]
    fn swap_correct_pins_recover_the_synthetic_truth() {
        let t = truth();
        let points = synthetic_points(&t, 64);
        let pins = pins_from_points(&points);

        // Sanity: the helper round-trips the generator's own points.
        for (pin, expected) in pins.iter().zip(points.iter()) {
            let mp = pin_to_matched_point(pin.left_px, pin.right_px, (LW, LH), (RW, RH));
            assert_abs_diff_eq!(mp.left[0], expected.left[0], epsilon = 1e-9);
            assert_abs_diff_eq!(mp.left[1], expected.left[1], epsilon = 1e-9);
            assert_abs_diff_eq!(mp.right[0], expected.right[0], epsilon = 1e-9);
            assert_abs_diff_eq!(mp.right[1], expected.right[1], epsilon = 1e-9);
        }

        let config = CalibrationConfig::default();
        let result = solve_manual_calibration(&pins, &[], (LW, LH), (RW, RH), &config)
            .expect("swap-correct solve should succeed");

        assert_eq!(result.pins_used, pins.len());
        assert_eq!(result.auto_used, 0);
        assert_abs_diff_eq!(result.layout.camera_axis_offset, t.cam_d, epsilon = 0.02);
        assert_abs_diff_eq!(result.layout.intersect, t.intersect, epsilon = 0.05);
        assert_abs_diff_eq!(result.layout.x_ty, t.x_ty, epsilon = 0.01);
    }

    /// Guards the trap: the naive no-swap mapping yields a *different* rig, and
    /// the wrong mapping must not recover the truth.
    ///
    /// Historically the wrong-swap solve also produced a deceptively healthy
    /// residual (the old un-normalized weighted sum could be driven to ~0 by a
    /// bogus rig). The normalized objective no longer collapses that way, so the
    /// wrong swap now reports a large residual instead of hiding itself — the
    /// test asserts both the wrong layout and the non-silent residual.
    #[test]
    fn no_swap_pins_do_not_recover_the_truth_and_no_longer_hide_the_error() {
        let t = truth();
        let points = synthetic_points(&t, 64);
        let pins = pins_from_points(&points);

        // The WRONG mapping: left pixel -> left plane, right pixel -> right plane.
        let naive: Vec<MatchedPoint> = pins
            .iter()
            .map(|p| MatchedPoint {
                left: normalize_to_plane(p.left_px[0], p.left_px[1], LW, LH),
                right: normalize_to_plane(p.right_px[0], p.right_px[1], RW, RH),
                left_pixel_nx: p.left_px[0] / LW as f64,
                right_pixel_nx: p.right_px[0] / RW as f64,
            })
            .collect();

        let config = CalibrationConfig::default();
        let wrong = solve_manual_calibration(&[], &naive, (LW, LH), (RW, RH), &config)
            .expect("the wrong-swap solve still returns a rig — that is the trap");

        // The wrong mapping cannot recover the truth, and — unlike the old
        // objective — it now reports an unhealthy residual rather than hiding.
        let recovers_truth = (wrong.layout.camera_axis_offset - t.cam_d).abs() < 0.02
            && (wrong.layout.intersect - t.intersect).abs() < 0.05
            && (wrong.layout.x_ty - t.x_ty).abs() < 0.01;
        assert!(
            !recovers_truth,
            "wrong swap must not recover the truth: got cam_d={}, intersect={}, x_ty={}",
            wrong.layout.camera_axis_offset, wrong.layout.intersect, wrong.layout.x_ty
        );
        assert!(
            wrong.residual > 1e-3,
            "wrong-swap residual must now surface the mismatch, got {}",
            wrong.residual
        );
    }

    /// Guards empty input: a typed error, never a bogus zero layout.
    #[test]
    fn empty_pin_set_is_a_typed_error_not_a_bogus_solve() {
        let config = CalibrationConfig::default();
        let err = solve_manual_calibration(&[], &[], (LW, LH), (RW, RH), &config)
            .expect_err("an empty set must not solve");

        match err {
            CalibrateError::InsufficientMatches { got, min } => {
                assert_eq!(got, 0);
                assert_eq!(min, 1);
            }
            other => panic!("expected InsufficientMatches, got {other:?}"),
        }
    }

    /// Guards coincident input: all pins at one location is degenerate.
    #[test]
    fn coincident_pins_are_rejected_as_degenerate() {
        let config = CalibrationConfig::default();
        let pins = vec![
            ManualPin {
                left_px: [960.0, 540.0],
                right_px: [960.0, 540.0],
            };
            6
        ];

        let err = solve_manual_calibration(&pins, &[], (LW, LH), (RW, RH), &config)
            .expect_err("coincident pins must not solve");
        assert!(
            matches!(err, CalibrateError::InvalidConfig(_)),
            "expected a degenerate-set error, got {err:?}"
        );
    }

    /// Guards collinear input: pins along a single line are degenerate.
    #[test]
    fn collinear_pins_are_rejected_as_degenerate() {
        let config = CalibrationConfig::default();
        let pins: Vec<ManualPin> = (0..8)
            .map(|i| {
                let x = 200.0 + i as f64 * 150.0;
                ManualPin {
                    left_px: [x, 540.0],
                    right_px: [x, 540.0],
                }
            })
            .collect();

        let err = solve_manual_calibration(&pins, &[], (LW, LH), (RW, RH), &config)
            .expect_err("collinear pins must not solve");
        assert!(
            matches!(err, CalibrateError::InvalidConfig(_)),
            "expected a degenerate-set error, got {err:?}"
        );
    }

    /// Guards ordering: reversing the pin order yields the same layout.
    #[test]
    fn pin_order_does_not_affect_the_solve() {
        let t = truth();
        let points = synthetic_points(&t, 64);
        let mut pins = pins_from_points(&points);
        let config = CalibrationConfig::default();

        let forward = solve_manual_calibration(&pins, &[], (LW, LH), (RW, RH), &config)
            .expect("forward solve should succeed");
        pins.reverse();
        let reversed = solve_manual_calibration(&pins, &[], (LW, LH), (RW, RH), &config)
            .expect("reversed solve should succeed");

        assert_abs_diff_eq!(
            forward.layout.camera_axis_offset,
            reversed.layout.camera_axis_offset,
            epsilon = 1e-6
        );
        assert_abs_diff_eq!(
            forward.layout.intersect,
            reversed.layout.intersect,
            epsilon = 1e-6
        );
        assert_abs_diff_eq!(forward.layout.x_ty, reversed.layout.x_ty, epsilon = 1e-6);
        assert_abs_diff_eq!(forward.layout.x_rz, reversed.layout.x_rz, epsilon = 1e-6);
        assert_abs_diff_eq!(forward.layout.z_rx, reversed.layout.z_rx, epsilon = 1e-6);
    }

    /// Guards MANU-04: pre-swapped auto matches are appended without re-swapping.
    #[test]
    fn prepopulated_auto_matches_are_not_re_swapped() {
        let t = truth();
        let points = synthetic_points(&t, 64);
        let (auto, pin_pts) = points.split_at(32);
        let pins = pins_from_points(pin_pts);

        let config = CalibrationConfig::default();
        let result = solve_manual_calibration(&pins, auto, (LW, LH), (RW, RH), &config)
            .expect("auto + pins solve should succeed");

        assert_eq!(result.pins_used, pins.len());
        assert_eq!(result.auto_used, auto.len());
        assert_abs_diff_eq!(result.layout.camera_axis_offset, t.cam_d, epsilon = 0.02);
        assert_abs_diff_eq!(result.layout.intersect, t.intersect, epsilon = 0.05);
        assert_abs_diff_eq!(result.layout.x_ty, t.x_ty, epsilon = 0.01);
    }

    /// MANU-04: verified (post-RANSAC) plane points convert back to natural-order
    /// pixel pins whose `.left`/`.right` match the original swap, and re-applying
    /// the swap-correct helper recovers the source point.
    #[test]
    fn seed_pins_from_verified_recovers_the_swap_correct_pixels() {
        let t = truth();
        let points = synthetic_points(&t, 16);
        let pins = seed_pins_from_verified(&points, (LW, LH), (RW, RH));
        assert_eq!(pins.len(), points.len());

        for (pin, point) in pins.iter().zip(points.iter()) {
            // `.left` (right camera plane) -> left_px on the LEFT frame.
            let expected_left = plane_to_px(point.right, LW, LH);
            assert_abs_diff_eq!(pin.left_px[0], expected_left[0], epsilon = 1e-9);
            assert_abs_diff_eq!(pin.left_px[1], expected_left[1], epsilon = 1e-9);
            // `.right` (left camera plane) -> right_px on the RIGHT frame.
            let expected_right = plane_to_px(point.left, RW, RH);
            assert_abs_diff_eq!(pin.right_px[0], expected_right[0], epsilon = 1e-9);
            assert_abs_diff_eq!(pin.right_px[1], expected_right[1], epsilon = 1e-9);

            // The seeded pin round-trips through the single swap-correct helper.
            let mp = pin_to_matched_point(pin.left_px, pin.right_px, (LW, LH), (RW, RH));
            assert_abs_diff_eq!(mp.left[0], point.left[0], epsilon = 1e-9);
            assert_abs_diff_eq!(mp.left[1], point.left[1], epsilon = 1e-9);
            assert_abs_diff_eq!(mp.right[0], point.right[0], epsilon = 1e-9);
            assert_abs_diff_eq!(mp.right[1], point.right[1], epsilon = 1e-9);
        }
    }

    /// MANU-04: an empty verified set yields an empty pin set (no panic, no
    /// fabricated pins).
    #[test]
    fn empty_verified_matches_yield_no_pins() {
        let pins = seed_pins_from_verified(&[], (LW, LH), (RW, RH));
        assert!(pins.is_empty());
    }
}
