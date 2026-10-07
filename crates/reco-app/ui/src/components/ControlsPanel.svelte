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
  import { presenter } from "../lib/presenter.svelte";
  import type { PresenterKind, ViewMode } from "../lib/types";

  let {
    expanded,
    viewMode,
    onToggle,
    onFovChange,
    onResetView,
    onPresenterChange,
  }: {
    expanded: boolean;
    viewMode: ViewMode;
    onToggle: () => void;
    onFovChange: (value: number) => void;
    onResetView: () => void;
    onPresenterChange: (value: PresenterKind | "auto") => void;
  } = $props();

  const poseEnabled = $derived(pose.enabled);
  // Pose controls act on the panorama only: the source render path ignores the
  // pose, so a FOV/reset/arrow change there would be silently invisible. This
  // mirrors `webviewPoseActive`'s view-mode gate in PreviewSurface.svelte (the
  // presenter half of that gate does not apply here: these controls send
  // intents and work in native mode too, where the child owns pointer input).
  const poseControlsEnabled = $derived(poseEnabled && viewMode === "panorama");
  const fovValue = $derived(pose.fovValue);
  const yaw = $derived(pose.pose?.yaw ?? 0);
  const pitch = $derived(pose.pose?.pitch ?? 0);
  const fov = $derived(pose.pose?.fov ?? 75);
  // The coverage ceiling, so the slider stops advertising range it cannot reach.
  const fovMax = $derived(pose.fovMax);
  // The live presenter, so the override select reflects the active one instead
  // of snapping back to "Auto" after a manual override. The badge is the
  // authoritative readout; the select must agree with it.
  const presenterValue = $derived(presenter.kind);
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
          disabled={!poseControlsEnabled}
          onChange={onFovChange}
        />
      </div>

      <div class="panel-section">
        <ResetViewButton
          disabled={!poseControlsEnabled}
          onClick={onResetView}
        />
      </div>

      <div class="panel-section">
        <h3 class="section-heading">Presenter</h3>
        <PresenterSelect
          value={presenterValue}
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
    top: var(--workflow-rail-height);
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
