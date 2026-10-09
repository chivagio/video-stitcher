<!--
  FOV slider (UI-SPEC Component Inventory).
  <input type="range"> 20°..coverage ceiling, step 1; value mirrors worker pose.
  The floor is the engine's `fov_min_degrees` and the ceiling comes from the
  worker (`max`), because the pose is clamped to it and a wider control would
  silently do nothing above it. Disabled until pose state is available.
-->
<script lang="ts">
  let {
    value,
    max = 150,
    disabled = false,
    onChange,
  }: {
    value: number;
    /**
     * The widest FOV the clip's coverage allows. Bounding the slider to it is
     * what stops the control advertising a range it cannot reach: the pose is
     * clamped to this every tick, so anything above it silently did nothing —
     * on the shipped test clip the slider spanned 40..150 while the real limit
     * was 50.9.
     */
    max?: number;
    disabled?: boolean;
    onChange: (value: number) => void;
  } = $props();

  /**
   * The narrowest FOV the engine accepts, pinned to `PoseControl`'s configured
   * `fov_min_degrees` default (20°, `crates/reco-control/src/pose_control.rs`).
   * `set_target_fov` clamps every request into `[fov_min, fov_max]`, so 20° is
   * genuinely reachable — a floor the engine refuses is a dead part of the
   * control. The old hardcoded 40 advertised a minimum the engine never
   * enforced: the mirror image of the FRICTION A12 ceiling lie (the control
   * spanned 40..150 while `clamp_via_coverage` pinned the pose to 50.87° on
   * the shipped clip). A narrower FOV is inside the coverage boundary by
   * construction, so the floor needs no coverage reconciliation.
   */
  const FLOOR = 20;

  /**
   * `max` from coverage, floored at the engine minimum so the reported ceiling
   * can never produce an empty range (a coverage boundary of 0 would).
   */
  const ceiling = $derived(Math.max(FLOOR, max));

  function handleInput(e: Event): void {
    const v = Math.min(Number((e.target as HTMLInputElement).value), ceiling);
    onChange(v);
  }
</script>

<div class="fov-slider">
  <label for="fov-slider" class="fov-label">FOV</label>
  <input
    id="fov-slider"
    type="range"
    class="fov-input"
    min={FLOOR}
    max={ceiling}
    step="1"
    {value}
    {disabled}
    aria-label="Field of view"
    aria-valuetext="{value} degrees"
    oninput={handleInput}
  />
  <span class="fov-value">{value}°</span>
</div>

<style>
  .fov-slider {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
  }

  .fov-label {
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .fov-input {
    flex: 1;
    height: 4px;
    -webkit-appearance: none;
    appearance: none;
    background: var(--color-dominant);
    border-radius: 2px;
    cursor: pointer;
    outline: none;
  }

  .fov-input:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .fov-input::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--color-accent);
    cursor: pointer;
  }

  .fov-input::-moz-range-thumb {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--color-accent);
    border: none;
    cursor: pointer;
  }

  .fov-input:focus-visible {
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .fov-value {
    font-family: var(--font-mono);
    font-size: var(--text-body);
    font-weight: var(--weight-regular);
    color: var(--color-log-info);
    min-width: 40px;
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
</style>
