<!--
  State overlay (UI-SPEC Component Inventory).
  Empty / loading / error / degraded messaging.
  Copy wraps within the preview region and never truncates.
-->
<script lang="ts">
  export type OverlayState =
    | { kind: "empty" }
    | { kind: "loading" }
    | { kind: "error"; message: string; nextStep: string }
    | { kind: "degraded"; message: string };

  let { state }: { state: OverlayState } = $props();
</script>

<div class="state-overlay" role="status">
  {#if state.kind === "empty"}
    <h2 class="overlay-heading">Nothing to preview</h2>
    <p class="overlay-body">
      Load two clips to see the stitched panorama here. (Phase 2 previews the
      already-loaded session; the import UI arrives in a later phase.)
    </p>
  {:else if state.kind === "loading"}
    <p class="overlay-body">Preparing preview…</p>
  {:else if state.kind === "error"}
    <h2 class="overlay-heading">Preview failed: {state.message}.</h2>
    <p class="overlay-body">{state.nextStep}</p>
  {:else if state.kind === "degraded"}
    <p class="overlay-body">{state.message}</p>
  {/if}
</div>

<style>
  .state-overlay {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    width: 100%;
    height: 100%;
    padding: var(--space-lg);
    text-align: center;
    overflow-y: auto;
  }

  .overlay-heading {
    margin: 0 0 var(--space-sm) 0;
    font-family: var(--font-ui);
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .overlay-body {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-regular);
    line-height: var(--line-body);
    color: var(--color-log-info);
    max-width: 600px;
    overflow-wrap: anywhere;
  }
</style>
