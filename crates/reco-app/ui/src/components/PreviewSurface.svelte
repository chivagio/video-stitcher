<!--
  Preview surface (UI-SPEC Component Inventory).
  Mode-aware host: transparent (native) · placeholder (separate window) ·
  readback canvas (degraded).
-->
<script lang="ts">
  import { Channel } from "@tauri-apps/api/core";
  import { pose } from "../lib/pose.svelte";
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
    onAttachReadback: (channel: Channel<ArrayBuffer>) => void;
    onShowPreviewWindow: () => void;
  } = $props();

  const isNative = $derived(presenterKind === "native");
  const isSeparateWindow = $derived(presenterKind === "separate_window");
  const isReadback = $derived(presenterKind === "readback");

  let canvasEl = $state<HTMLCanvasElement | null>(null);

  function paintFrame(buffer: ArrayBuffer): void {
    if (!canvasEl) return;
    const ctx = canvasEl.getContext("2d");
    if (!ctx) return;
    const w = canvasEl.width;
    const h = canvasEl.height;
    const imageData = new ImageData(new Uint8ClampedArray(buffer), w, h);
    ctx.putImageData(imageData, 0, 0);
  }

  // `$effect` keyed on `isReadback`, NOT `onMount`. The session starts on the
  // native presenter, so `onMount` saw `isReadback === false` and bailed — and
  // because the presenter is swappable at runtime, the readback arm would have
  // rendered its canvas with no Channel ever created, leaving a blank frame
  // under the degraded banner. Re-running on the transition attaches the Channel
  // exactly when the readback arm becomes active.
  //
  // Cleanup only clears the JS handler: `preview_attach_readback` takes a
  // non-nullable channel, so there is no detach to send. A later attach replaces
  // the stored channel anyway.
  $effect(() => {
    if (!isReadback) return;
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = (buffer: ArrayBuffer) => {
      paintFrame(buffer);
    };
    onAttachReadback(channel);
    return () => {
      // A no-op rather than `undefined`: Tauri's Channel types the handler as
      // required, and detaching is what we mean.
      channel.onmessage = () => {};
    };
  });
</script>

<div
  class="preview-surface"
  class:native={isNative}
  class:separate-window={isSeparateWindow}
  class:readback={isReadback}
  role="img"
  aria-label={viewMode === "source" ? "Source" : "Panorama"}
  onwheel={(e) => {
    if (isNative) {
      e.preventDefault();
      const delta = e.deltaY > 0 ? -1 : 1;
      void pose.nudgeFov(delta);
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
      bind:this={canvasEl}
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
