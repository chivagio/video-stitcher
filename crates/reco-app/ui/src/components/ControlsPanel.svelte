<!--
  Controls panel (UI-SPEC Component Inventory).
  Collapsible panel shell (aria-expanded on toggle).
  Right edge; 280px expanded / 40px rail collapsed.
-->
<script lang="ts">
  import Icon from "./Icon.svelte";
  import FovSlider from "./FovSlider.svelte";
  import PoseReadout from "./PoseReadout.svelte";
  import ResetViewButton from "./ResetViewButton.svelte";
  import PresenterSelect from "./PresenterSelect.svelte";
  import { pose } from "../lib/pose.svelte";
  import type { PresenterKind } from "../lib/types";

  let {
    expanded,
    onToggle,
    onFovChange,
    onResetView,
    onPresenterChange,
  }: {
    expanded: boolean;
    onToggle: () => void;
    onFovChange: (value: number) => void;
    onResetView: () => void;
    onPresenterChange: (value: PresenterKind | "auto") => void;
  } = $props();

  const poseEnabled = $derived(pose.enabled);
  const fovValue = $derived(pose.fovValue);
  const yaw = $derived(pose.pose?.yaw ?? 0);
  const pitch = $derived(pose.pose?.pitch ?? 0);
  const fov = $derived(pose.pose?.fov ?? 75);
  // The coverage ceiling, so the slider stops advertising range it cannot reach.
  const fovMax = $derived(pose.fovMax);
</script>

<div class="controls-panel" class:expanded>
  <button
    type="button"
    class="panel-toggle"
    aria-label={expanded ? "Collapse controls" : "Expand controls"}
    title={expanded ? "Collapse controls" : "Expand controls"}
    aria-expanded={expanded}
    aria-controls="controls-panel-content"
    onclick={onToggle}
  >
    <Icon name="controls" size={20} />
  </button>

  {#if expanded}
    <div id="controls-panel-content" class="panel-content">
      <div class="panel-section">
        <h3 class="section-heading">Pose</h3>
        <PoseReadout {yaw} {pitch} {fov} />
      </div>

      <div class="panel-section">
        <h3 class="section-heading">Field of view</h3>
        <FovSlider
          value={fovValue}
          max={fovMax}
          disabled={!poseEnabled}
          onChange={onFovChange}
        />
      </div>

      <div class="panel-section">
        <ResetViewButton
          disabled={!poseEnabled}
          onClick={onResetView}
        />
      </div>

      <div class="panel-section">
        <h3 class="section-heading">Presenter</h3>
        <PresenterSelect
          value="auto"
          disabled={false}
          onChange={onPresenterChange}
        />
      </div>
    </div>
  {/if}
</div>

<style>
  .controls-panel {
    position: fixed;
    right: 0;
    top: 0;
    bottom: var(--transport-bar-height);
    width: var(--controls-panel-collapsed-width);
    background: var(--color-secondary);
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: var(--space-sm);
    transition: width 200ms ease;
    z-index: 10;
  }

  .controls-panel.expanded {
    width: var(--controls-panel-width);
    align-items: stretch;
    padding: var(--space-md);
  }

  .panel-toggle {
    display: flex;
    align-items: center;
    justify-content: center;
    min-width: 32px;
    min-height: 32px;
    padding: var(--space-sm);
    border: none;
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-body-text);
    cursor: pointer;
  }

  .panel-toggle:hover {
    background: var(--color-dominant);
  }

  .panel-toggle:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .panel-content {
    display: flex;
    flex-direction: column;
    gap: var(--space-lg);
    overflow-y: auto;
    flex: 1;
  }

  .panel-section {
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }

  .section-heading {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  @media (prefers-reduced-motion: reduce) {
    .controls-panel {
      transition: none;
    }
  }
</style>
