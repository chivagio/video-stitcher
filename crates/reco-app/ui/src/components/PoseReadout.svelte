<!--
  Pose readout (UI-SPEC Component Inventory).
  Yaw / pitch / FOV numeric readout from worker pose events.
-->
<script lang="ts">
  let {
    yaw,
    pitch,
    fov,
  }: {
    yaw: number;
    pitch: number;
    fov: number;
  } = $props();
</script>

<div class="pose-readout" aria-label="Pose readout">
  <div class="pose-row">
    <span class="pose-label">Yaw</span>
    <span class="pose-value">{yaw.toFixed(3)}</span>
  </div>
  <div class="pose-row">
    <span class="pose-label">Pitch</span>
    <span class="pose-value">{pitch.toFixed(3)}</span>
  </div>
  <div class="pose-row">
    <span class="pose-label">FOV</span>
    <span class="pose-value">{fov.toFixed(1)}°</span>
  </div>
</div>

<style>
  .pose-readout {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .pose-row {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
  }

  .pose-label {
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .pose-value {
    font-family: var(--font-mono);
    font-size: var(--text-body);
    font-weight: var(--weight-regular);
    color: var(--color-log-info);
    font-variant-numeric: tabular-nums;
  }
</style>
