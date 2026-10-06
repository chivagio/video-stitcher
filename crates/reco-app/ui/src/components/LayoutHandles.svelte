<!--
  Constrained layout handles (MANU-06): single-parameter controls for the real
  `PlaneLayout` fields the requirement names — `x_ty`, `x_rz`, `intersect`, and
  `cam_d`. Each control moves exactly one parameter within its travel range
  (x_ty ±0.1, x_rz ±0.3 rad, intersect 0–1, cam_d 0.1–0.30); no free 2-D
  manipulation is offered, because `intersect`/`x_ty`/`z_rx` are ~0.98–1.0
  coupled. Values are clamped before posting; the worker re-clamps and WARNs at
  the boundary. All feedback is the worker's real-parameter preview.
-->
<script lang="ts">
  import {
    manual,
    X_TY_CLAMP,
    X_RZ_CLAMP,
    CAM_D_RANGE,
  } from "../lib/manual.svelte";

  const layout = $derived(manual.layout);

  function clamp(v: number, lo: number, hi: number): number {
    return Math.min(hi, Math.max(lo, v));
  }

  /**
   * Local, optimistic slider values (MANU-06).
   *
   * The worker is authoritative for the real layout parameter — this component
   * never derives it. But a range input is inherently local while the operator
   * drags: the browser moves the thumb on pointer input, and Svelte only writes
   * the DOM back when the bound value *changes*. If the worker's echoed value
   * equals the one it last rendered, Svelte skips the write, leaving the thumb
   * at the drag position while the numeric label shows the (older) worker value
   * — the desync observed after a layout-handle drag. Driving the thumb and the
   * label from one local value keeps them consistent for the whole interaction;
   * the worker's echo re-seeds it once the drag ends (reconciling a clamp).
   */
  let local = $state({ cam_d: 0, intersect: 0, x_ty: 0, x_rz: 0 });
  /** Whether a pointer drag is in progress, so echoes don't fight the thumb. */
  let dragging = $state(false);

  $effect(() => {
    // Re-seed from the worker only between drags; during a drag the thumb is
    // the operator's and a late echo of an earlier move must not pull it back.
    if (layout && !dragging) {
      local = {
        cam_d: layout.camera_axis_offset,
        intersect: layout.intersect,
        x_ty: layout.x_ty,
        x_rz: layout.x_rz,
      };
    }
  });

  /** Post a layout edit with exactly one parameter changed (MANU-06). */
  function emit(patch: {
    cam_d?: number;
    intersect?: number;
    x_ty?: number;
    x_rz?: number;
  }): void {
    if (!layout) return;
    // Unchanged fields come from the local value, which is at least as fresh as
    // the last worker echo: a drag that posts a full layout must never overwrite
    // another handle with a stale value.
    void manual.setLayout({
      cam_d: clamp(patch.cam_d ?? local.cam_d, CAM_D_RANGE[0], CAM_D_RANGE[1]),
      intersect: clamp(patch.intersect ?? local.intersect, 0, 1),
      x_ty: clamp(patch.x_ty ?? local.x_ty, -X_TY_CLAMP, X_TY_CLAMP),
      x_rz: clamp(patch.x_rz ?? local.x_rz, -X_RZ_CLAMP, X_RZ_CLAMP),
    });
  }

  /** Update the local slider value and post the edit (MANU-06). */
  function edit(patch: {
    cam_d?: number;
    intersect?: number;
    x_ty?: number;
    x_rz?: number;
  }): void {
    local = { ...local, ...patch };
    emit(patch);
  }
</script>

<section class="layout-handles" aria-label="Rig alignment handles">
  {#if layout}
    <label class="layout-field">
      <span class="layout-label">Vertical shift (x_ty)</span>
      <input
        type="range"
        min={-X_TY_CLAMP}
        max={X_TY_CLAMP}
        step="0.001"
        value={local.x_ty}
        aria-label="Vertical shift (x_ty), {local.x_ty.toFixed(3)}, clamp ±{X_TY_CLAMP}"
        onfocus={() => manual.snapshotLayout()}
        onpointerdown={() => (dragging = true)}
        onpointerup={() => (dragging = false)}
        onpointercancel={() => (dragging = false)}
        onblur={() => (dragging = false)}
        oninput={(e) => edit({ x_ty: Number(e.currentTarget.value) })}
      />
      <span class="layout-value">{local.x_ty.toFixed(3)}</span>
    </label>

    <label class="layout-field">
      <span class="layout-label">Roll (x_rz)</span>
      <input
        type="range"
        min={-X_RZ_CLAMP}
        max={X_RZ_CLAMP}
        step="0.005"
        value={local.x_rz}
        aria-label="Roll (x_rz), {local.x_rz.toFixed(3)} rad, clamp ±{X_RZ_CLAMP}"
        onfocus={() => manual.snapshotLayout()}
        onpointerdown={() => (dragging = true)}
        onpointerup={() => (dragging = false)}
        onpointercancel={() => (dragging = false)}
        onblur={() => (dragging = false)}
        oninput={(e) => edit({ x_rz: Number(e.currentTarget.value) })}
      />
      <span class="layout-value">{local.x_rz.toFixed(3)} rad</span>
    </label>

    <label class="layout-field">
      <span class="layout-label">Overlap (intersect)</span>
      <input
        type="range"
        min="0"
        max="1"
        step="0.005"
        value={local.intersect}
        aria-label="Overlap (intersect), {local.intersect.toFixed(3)}, clamp 0 to 1"
        onfocus={() => manual.snapshotLayout()}
        onpointerdown={() => (dragging = true)}
        onpointerup={() => (dragging = false)}
        onpointercancel={() => (dragging = false)}
        onblur={() => (dragging = false)}
        oninput={(e) => edit({ intersect: Number(e.currentTarget.value) })}
      />
      <span class="layout-value">{local.intersect.toFixed(3)}</span>
    </label>

    <label class="layout-field">
      <span class="layout-label">Camera distance (cam_d)</span>
      <input
        type="range"
        min={CAM_D_RANGE[0]}
        max={CAM_D_RANGE[1]}
        step="0.005"
        value={local.cam_d}
        aria-label="Camera distance (cam_d), {local.cam_d.toFixed(3)}, clamp {CAM_D_RANGE[0]} to {CAM_D_RANGE[1]}"
        onfocus={() => manual.snapshotLayout()}
        onpointerdown={() => (dragging = true)}
        onpointerup={() => (dragging = false)}
        onpointercancel={() => (dragging = false)}
        onblur={() => (dragging = false)}
        oninput={(e) => edit({ cam_d: Number(e.currentTarget.value) })}
      />
      <span class="layout-value">{local.cam_d.toFixed(3)}</span>
    </label>
  {:else}
    <p class="layout-empty">Rig alignment becomes available once a session opens.</p>
  {/if}
</section>

<style>
  .layout-handles {
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }

  .layout-field {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 2fr) auto;
    align-items: center;
    gap: var(--space-sm);
  }

  .layout-label {
    color: var(--color-body-text);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
  }

  .layout-field input[type="range"] {
    width: 100%;
    accent-color: var(--color-accent);
  }

  .layout-value {
    font-family: var(--font-mono);
    color: var(--color-log-info);
    white-space: nowrap;
  }

  .layout-empty {
    margin: 0;
    color: var(--color-log-info);
  }

  @media (max-width: 720px) {
    .layout-field {
      grid-template-columns: 1fr;
    }
  }
</style>
