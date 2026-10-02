//! Pure translation from a pointer gesture to `reco_control::ControlIntent`
//! (PREV-04, native presenter path).
//!
//! # Why this module is pure and separate from [`super::x11`]
//!
//! Under the native presenter the panorama is an X11 **child window** of the
//! main window, and the WebKitGTK webview is not a separate X window — GTK draws
//! it into the parent's own surface. An X child always composites above its
//! parent's own drawing and takes every pointer event in its area, so the
//! webview's own `onpointerdown` / `onwheel` handlers can never run over the
//! panorama (02-UAT gap 1). The native child therefore **owns** pose input, and
//! this module is where a gesture becomes an intent.
//!
//! The X event plumbing in `x11.rs` is untestable without an X server (CI has
//! none). The translation is not: it is arithmetic on three numbers. Keeping the
//! two apart is what makes the *direction* of the pan assertable in CI at all —
//! which matters because the sign convention is the part that is easy to get
//! backwards and invisible to every other gate.
//!
//! # Sign convention
//!
//! **+yaw looks LEFT and +pitch looks UP** (derived from `view_matrix`, recorded
//! in 02-VERIFICATION and FRICTION A6). The authority is the CLI's keymap in
//! `crates/reco-cli/src/preview.rs:582-598`:
//! `Left = +yaw`, `Right = -yaw`, `Up = +pitch`, `Down = -pitch`.
//!
//! So a drag to the right must carry a **negative** yaw delta (turn the view
//! right) and a drag downward must carry a **negative** pitch delta (look down).
//! Both gestures then agree with the arrow keys and with the CLI, axis for axis.
//!
//! [`YAW_DRAG_SIGN`] and [`PITCH_DRAG_SIGN`] stay named constants rather than
//! being folded into the arithmetic, so the convention is one obvious place to
//! flip — and the frontend's own pair in `PreviewSurface.svelte` must be flipped
//! at the same time.

use reco_control::{ControlIntent, PoseIntent};

/// FOV change, in degrees, per mouse-wheel notch.
///
/// A judgement call with no automated signal for "feels right" — the existing
/// wheel path in the frontend nudges by one step, and `PoseControl`'s own
/// `wheel_fov_per_tick` is 3°. It is one named constant rather than a literal
/// buried in the arithmetic so the choice is reviewable and adjustable in one
/// place.
pub const WHEEL_FOV_STEP_DEG: f32 = 2.0;

/// Sign applied to a horizontal drag: negative, so drag right turns the view
/// right (matching the CLI's `ArrowRight`).
const YAW_DRAG_SIGN: f32 = -1.0;

/// Sign applied to a vertical drag: negative, so drag down looks down (matching
/// the CLI's `ArrowDown`).
const PITCH_DRAG_SIGN: f32 = -1.0;

/// Largest pointer travel, in pixels, folded into one drain's intent.
///
/// Belt-and-braces rather than the primary safety: the per-tick
/// `clamp_via_coverage` in `PoseControl` already keeps the pose inside the
/// covered region, so a pathological delta cannot walk out of bounds. This bound
/// exists so one absurd event (a synthetic flood, a pointer jump across the
/// screen while a button is held) cannot teleport the camera in a single step.
/// Two viewport widths is far past anything a hand produces in one drain.
pub const MAX_DRAG_PX_PER_DRAIN: f32 = 4096.0;

/// Largest wheel travel, in notches, folded into one drain's intent.
///
/// ~15 full flicks of the wheel inside a single worker-loop iteration; more than
/// that is a synthetic flood, not a user.
pub const MAX_WHEEL_NOTCHES_PER_DRAIN: f32 = 32.0;

/// One drain's worth of pointer input on the native child window.
///
/// Accumulated by the presenter between drains, so its magnitude is bounded only
/// by how long the worker's loop took — not by the pixel size of the drag.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct PointerGesture {
    /// Horizontal pointer travel in pixels since the last drain.
    ///
    /// Accumulated only while the primary button is held, so a bare cursor move
    /// across the panorama pans nothing. Positive means the pointer moved right.
    pub drag_dx: f32,
    /// Vertical pointer travel in pixels since the last drain.
    ///
    /// Only while the primary button is held. Positive means the pointer moved
    /// **down** in window coordinates (Y grows downwards in X11).
    pub drag_dy: f32,
    /// Wheel notches since the last drain.
    ///
    /// Positive means scroll up (away from the user), matching the frontend's
    /// `nudgeFov` sign where a negative FOV delta means "zoom in".
    pub wheel_notches: f32,
}

impl PointerGesture {
    /// Whether this gesture carries no motion at all.
    pub fn is_empty(&self) -> bool {
        self.drag_dx == 0.0 && self.drag_dy == 0.0 && self.wheel_notches == 0.0
    }
}

/// Translate a pointer gesture into the intents that move the worker's pose.
///
/// This is the **single** translation point for native pointer input, and it must
/// agree with the CLI keymap (`crates/reco-cli/src/preview.rs:582-598`) — the two
/// doc comments that previously disagreed about this convention are not
/// authorities.
///
/// Sensitivity is zoom-relative: `rad_per_pixel = fov / viewport_width`, so a
/// drag across the full viewport width sweeps exactly one horizontal FOV and the
/// feel is identical at 40° and at 150°. That is deliberately the **same formula**
/// the frontend uses (`pose.fovValue * PI / 180 / clientWidth`), so pan feels the
/// same whether the panorama is native or readback; it is a cross-impl invariant
/// a future edit must not break.
///
/// The two axes are independent and additive, so the result is a `Vec`: drag
/// intents first (`DeltaYawRad`, `DeltaPitchRad`), then the wheel
/// (`DeltaFovDeg`). A zero gesture produces an empty `Vec`.
///
/// # Robustness
///
/// * `viewport_width == 0` or a non-positive FOV yields **no drag intents** — the
///   division is never performed, so no `NaN` can be produced. A `NaN` yaw would
///   poison `PoseControl` and, through `clamp_via_coverage`, could wedge the pose
///   permanently. The wheel axis is independent and still produces its intent.
/// * Each per-drain delta is clamped to [`MAX_DRAG_PX_PER_DRAIN`] /
///   [`MAX_WHEEL_NOTCHES_PER_DRAIN`].
pub fn pointer_gesture_to_intents(
    gesture: PointerGesture,
    fov_degrees: f32,
    viewport_width: u32,
) -> Vec<ControlIntent> {
    let mut intents = Vec::new();

    let dx = gesture
        .drag_dx
        .clamp(-MAX_DRAG_PX_PER_DRAIN, MAX_DRAG_PX_PER_DRAIN);
    let dy = gesture
        .drag_dy
        .clamp(-MAX_DRAG_PX_PER_DRAIN, MAX_DRAG_PX_PER_DRAIN);

    // Guard before dividing. `viewport_width` is a count of pixels and `fov` is
    // an angle; either being zero/negative means the drag axis has no defined
    // scale, so it is dropped rather than converted into a non-finite value.
    if (dx != 0.0 || dy != 0.0) && viewport_width > 0 && fov_degrees > 0.0 {
        let rad_per_pixel = fov_degrees.to_radians() / viewport_width as f32;
        if dx != 0.0 {
            intents.push(ControlIntent::Pose(PoseIntent::DeltaYawRad(
                YAW_DRAG_SIGN * dx * rad_per_pixel,
            )));
        }
        if dy != 0.0 {
            intents.push(ControlIntent::Pose(PoseIntent::DeltaPitchRad(
                PITCH_DRAG_SIGN * dy * rad_per_pixel,
            )));
        }
    }

    let notches = gesture
        .wheel_notches
        .clamp(-MAX_WHEEL_NOTCHES_PER_DRAIN, MAX_WHEEL_NOTCHES_PER_DRAIN);
    if notches != 0.0 {
        intents.push(ControlIntent::Pose(PoseIntent::DeltaFovDeg(
            notches * WHEEL_FOV_STEP_DEG,
        )));
    }

    intents
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull the `PoseIntent` out of an intent, panicking on anything else, so a
    /// test failure names the wrong variant rather than a mismatch error.
    fn pose_intent(intent: &ControlIntent) -> PoseIntent {
        match intent {
            ControlIntent::Pose(p) => *p,
            other => panic!("expected a pose intent, got {other:?}"),
        }
    }

    fn yaw_delta(g: PointerGesture, fov: f32, width: u32) -> f32 {
        let intents = pointer_gesture_to_intents(g, fov, width);
        let found = intents.iter().find_map(|i| match pose_intent(i) {
            PoseIntent::DeltaYawRad(v) => Some(v),
            _ => None,
        });
        found.unwrap_or_else(|| panic!("no yaw intent in {intents:?}"))
    }

    fn pitch_delta(g: PointerGesture, fov: f32, width: u32) -> f32 {
        let intents = pointer_gesture_to_intents(g, fov, width);
        let found = intents.iter().find_map(|i| match pose_intent(i) {
            PoseIntent::DeltaPitchRad(v) => Some(v),
            _ => None,
        });
        found.unwrap_or_else(|| panic!("no pitch intent in {intents:?}"))
    }

    fn fov_delta(g: PointerGesture, fov: f32, width: u32) -> Option<f32> {
        let intents = pointer_gesture_to_intents(g, fov, width);
        intents.iter().find_map(|i| match pose_intent(i) {
            PoseIntent::DeltaFovDeg(v) => Some(v),
            _ => None,
        })
    }

    #[test]
    fn dragging_right_turns_the_view_right() {
        // +yaw looks LEFT (CLI authority), so a rightward pointer drag must
        // carry a NEGATIVE yaw delta.
        let delta = yaw_delta(
            PointerGesture {
                drag_dx: 200.0,
                ..Default::default()
            },
            75.0,
            1000,
        );
        assert!(delta < 0.0, "drag right must yaw negative, got {delta}");
    }

    #[test]
    fn dragging_down_looks_down() {
        // +pitch looks UP (CLI authority), so a downward pointer drag must carry
        // a NEGATIVE pitch delta.
        let delta = pitch_delta(
            PointerGesture {
                drag_dy: 200.0,
                ..Default::default()
            },
            75.0,
            1000,
        );
        assert!(delta < 0.0, "drag down must pitch negative, got {delta}");
    }

    #[test]
    fn a_full_width_drag_sweeps_exactly_one_fov() {
        // The zoom-relative feel: dragging the whole viewport width turns the
        // view by one horizontal FOV, at any FOV.
        for fov in [40.0_f32, 75.0, 150.0] {
            let width = 1000_u32;
            let delta = yaw_delta(
                PointerGesture {
                    drag_dx: width as f32,
                    ..Default::default()
                },
                fov,
                width,
            );
            assert!(
                (delta.abs() - fov.to_radians()).abs() < 1e-4,
                "fov {fov}: |delta| {} != {} rad",
                delta.abs(),
                fov.to_radians()
            );
        }
    }

    #[test]
    fn scroll_up_widens_the_view() {
        // Positive notches = scroll up = wider view, matching `nudgeFov`'s
        // sign in the frontend (a negative FOV delta zooms in).
        let delta = fov_delta(
            PointerGesture {
                wheel_notches: 1.0,
                ..Default::default()
            },
            75.0,
            1000,
        );
        assert_eq!(delta, Some(WHEEL_FOV_STEP_DEG));
    }

    #[test]
    fn three_scroll_downs_narrow_by_three_steps() {
        let delta = fov_delta(
            PointerGesture {
                wheel_notches: -3.0,
                ..Default::default()
            },
            75.0,
            1000,
        );
        assert_eq!(delta, Some(-3.0 * WHEEL_FOV_STEP_DEG));
    }

    #[test]
    fn an_empty_gesture_produces_no_intents() {
        // No division, no NaN, no work for the worker.
        assert!(pointer_gesture_to_intents(PointerGesture::default(), 75.0, 1000).is_empty());
        assert!(PointerGesture::default().is_empty());
        // Also with the degenerate scales a bad event could supply.
        assert!(pointer_gesture_to_intents(PointerGesture::default(), 0.0, 0).is_empty());
    }

    #[test]
    fn a_zero_width_viewport_drops_the_drag_but_keeps_the_wheel() {
        // The axes are independent. A viewer that has not been laid out yet must
        // still zoom on a wheel notch rather than silently dropping both — and
        // must never divide by zero.
        let intents = pointer_gesture_to_intents(
            PointerGesture {
                drag_dx: 500.0,
                drag_dy: -500.0,
                wheel_notches: 2.0,
            },
            75.0,
            0,
        );
        assert_eq!(intents.len(), 1, "{intents:?}");
        assert_eq!(
            pose_intent(&intents[0]),
            PoseIntent::DeltaFovDeg(2.0 * WHEEL_FOV_STEP_DEG)
        );
        assert!(
            intents.iter().all(|i| !format!("{i:?}").contains("NaN")),
            "{intents:?}"
        );
    }

    #[test]
    fn a_non_positive_fov_also_drops_the_drag_only() {
        let intents = pointer_gesture_to_intents(
            PointerGesture {
                drag_dx: 100.0,
                wheel_notches: 1.0,
                ..Default::default()
            },
            0.0,
            1000,
        );
        assert_eq!(intents.len(), 1, "{intents:?}");
        assert_eq!(
            pose_intent(&intents[0]),
            PoseIntent::DeltaFovDeg(WHEEL_FOV_STEP_DEG)
        );
    }

    #[test]
    fn a_drag_and_a_wheel_in_one_drain_produce_both_drag_first() {
        let intents = pointer_gesture_to_intents(
            PointerGesture {
                drag_dx: 120.0,
                drag_dy: 60.0,
                wheel_notches: 1.0,
            },
            75.0,
            1000,
        );
        assert_eq!(intents.len(), 3, "{intents:?}");
        assert!(matches!(
            pose_intent(&intents[0]),
            PoseIntent::DeltaYawRad(_)
        ));
        assert!(matches!(
            pose_intent(&intents[1]),
            PoseIntent::DeltaPitchRad(_)
        ));
        assert_eq!(
            pose_intent(&intents[2]),
            PoseIntent::DeltaFovDeg(WHEEL_FOV_STEP_DEG)
        );
    }

    #[test]
    fn sensitivity_is_zoom_relative() {
        // The same pixel drag covers proportionally less angle when zoomed in.
        // This is what keeps the feel constant, and it is the cross-impl
        // invariant the frontend shares.
        let g = PointerGesture {
            drag_dx: 100.0,
            ..Default::default()
        };
        let narrow = yaw_delta(g, 40.0, 1000);
        let wide = yaw_delta(g, 150.0, 1000);
        assert!((narrow.abs() / wide.abs() - 40.0 / 150.0).abs() < 1e-6);
    }

    #[test]
    fn an_absurd_delta_is_clamped_rather_than_teleporting_the_camera() {
        let clamped = yaw_delta(
            PointerGesture {
                drag_dx: 1.0e9,
                ..Default::default()
            },
            150.0,
            1000,
        );
        let bound = MAX_DRAG_PX_PER_DRAIN * 150.0_f32.to_radians() / 1000.0;
        assert!(
            clamped.abs() <= bound + 1e-6,
            "clamped |delta| {} exceeded bound {bound}",
            clamped.abs()
        );

        let notches = fov_delta(
            PointerGesture {
                wheel_notches: 1.0e6,
                ..Default::default()
            },
            75.0,
            1000,
        );
        assert_eq!(
            notches,
            Some(MAX_WHEEL_NOTCHES_PER_DRAIN * WHEEL_FOV_STEP_DEG)
        );
    }

    #[test]
    fn no_input_produces_a_non_finite_value() {
        // The invariant the whole NaN story hangs on: sweep the degenerate inputs
        // and assert every emitted number is finite.
        let cases = [
            (PointerGesture::default(), 0.0_f32, 0_u32),
            (
                PointerGesture {
                    drag_dx: 10.0,
                    drag_dy: 10.0,
                    wheel_notches: 1.0,
                },
                0.0,
                0,
            ),
            (
                PointerGesture {
                    drag_dx: f32::MAX,
                    drag_dy: f32::MIN,
                    wheel_notches: f32::MIN,
                },
                150.0,
                1,
            ),
            (
                PointerGesture {
                    drag_dx: -10.0,
                    drag_dy: 10.0,
                    wheel_notches: -1.0,
                },
                f32::NAN,
                1000,
            ),
        ];
        for (g, fov, width) in cases {
            for intent in pointer_gesture_to_intents(g, fov, width) {
                let debug = format!("{intent:?}");
                assert!(!debug.contains("NaN"), "non-finite delta in {debug}");
                assert!(!debug.contains("inf"), "non-finite delta in {debug}");
            }
        }
    }
}
