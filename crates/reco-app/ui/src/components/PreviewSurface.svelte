<!--
  Preview surface (UI-SPEC Component Inventory).
  Mode-aware host: transparent (native) · placeholder (separate window) ·
  readback canvas (degraded).
-->
<script lang="ts">
  import StateOverlay from "./StateOverlay.svelte";
  import type { PresenterKind } from "../lib/types";

  let {
    presenterKind,
    viewMode,
    onAttachReadback,
    onShowPreviewWindow,
  }: {
    presenterKind: PresenterKind;
    viewMode: "source" | "panorama";
    onAttachReadback: (channel: any) => void;
    onShowPreviewWindow: () => void;
  } = $props();

  const isNative = $derived(presenterKind === "native");
  const isSeparateWindow = $derived(presenterKind === "separate_window");
  const isReadback = $derived(presenterKind === "readback");
</script>

<div
  class="preview-surface"
  class:native={isNative}
  class:separate-window={isSeparateWindow}
  class:readback={isReadback}
  tabindex="0"
  role="img"
  aria-label={viewMode === "source" ? "Source" : "Panorama"}
  onwheel={(e) => {
    if (isNative) {
      e.preventDefault();
      const delta = e.deltaY > 0 ? -1 : 1;
      onAttachReadback({ delta } as any);
    }
  }}
>
  {#if isNative}
    <!-- Native mode: the webview leaves the region transparent and unpainted.
         The native panorama renders here. No background, border, or shadow. -->
  {:else if isSeparateWindow}
    <StateOverlay
      state={{
        kind: "empty",
      }}
    />
    <div class="placeholder-content">
      <h2 class="placeholder-heading">Preview is in a separate window</h2>
      <p class="placeholder-body">
        Use "Show preview window" to bring the panorama forward.
      </p>
      <button
        type="button"
        class="placeholder-btn"
        onclick={onShowPreviewWindow}
      >
        Show preview window
      </button>
    </div>
  {:else if isReadback}
    <div class="readback-banner">
      <p class="readback-text">
        Degraded preview (readback) — Frames are copied to the UI and throttled.
        For smooth playback use a separate preview window.
      </p>
    </div>
    <canvas
      class="readback-canvas"
      width={1240}
      height={728}
    ></canvas>
  {/if}
</div>

<style>
  .preview-surface {
    position: fixed;
    left: 0;
    top: 0;
    width: calc(100% - var(--controls-panel-collapsed-width));
    height: calc(100% - var(--transport-bar-height));
    background: transparent;
    border: 0;
    box-shadow: none;
    overflow: hidden;
  }

  .preview-surface.native {
    /* Native mode: fully transparent — the native panorama shows through. */
    background: transparent;
  }

  .preview-surface.separate-window {
    background: var(--color-dominant);
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
  }

  .preview-surface.readback {
    background: var(--color-dominant);
    display: flex;
    flex-direction: column;
  }

  .placeholder-content {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-md);
    padding: var(--space-lg);
    text-align: center;
  }

  .placeholder-heading {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .placeholder-body {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-regular);
    line-height: var(--line-body);
    color: var(--color-log-info);
    max-width: 400px;
  }

  .placeholder-btn {
    padding: var(--space-sm) var(--space-md);
    border: 1px solid var(--color-accent);
    border-radius: var(--space-xs);
    background: var(--color-accent);
    color: var(--color-dominant);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    cursor: pointer;
  }

  .placeholder-btn:hover {
    filter: brightness(1.1);
  }

  .readback-banner {
    padding: var(--space-sm) var(--space-md);
    background: var(--color-log-warn);
    color: var(--color-dominant);
  }

  .readback-text {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
  }

  .readback-canvas {
    flex: 1;
    width: 100%;
    height: 100%;
    object-fit: contain;
  }
</style>
