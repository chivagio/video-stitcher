# reco-calibrate Consumer API Friction

Active friction points building `reco-calibrate` (and its consumers) against
its own seams. Per the repo rule ("document friction, don't work around it"), a
gap that a caller would otherwise hack around is recorded here rather than
silently worked around in the consumer.

## Active

### A1. `optimizer::optimize` cannot warm-start the layout solve

**Impact**: Medium. The alternating layout↔`k1` driver
(`intrinsics::refine_intrinsics`, phase 04.2) must re-solve the layout
**warm-started from the previous layout** so the layout stage never wanders
(INTR-02 / T-04.2-08). `optimizer::optimize` always multi-starts from the fixed
`STARTS_5` / `STARTS_4` tables and exposes no starting-point or seed argument,
so a warm start is impossible through the public API. Its bounds
(`BOUNDS_5`) are also private to the module.

**Workaround (this phase)**: `intrinsics.rs` carries a small single-start
Nelder-Mead wrapper (`solve_layout_warm`) that reuses the now-`pub(crate)`
`optimizer::BOUNDS_5` and the same trimmed seam-weighted cost, so it cannot
drift from the production layout solve. It duplicates the cost/simplex plumbing
rather than changing the public `optimize` signature.

**Proposed API**: an additive `optimizer::optimize_from(points, start: &PlaneLayout, config)`
(or an optional `start` on `OptimizerConfig`) that seeds a single start from a
previous layout while leaving `optimize` unchanged.
