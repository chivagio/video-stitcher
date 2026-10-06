<!--
  File drop slot (IMPT-01 / D3-03): a labelled camera slot with a picker + HTML5
  drop target. Both paths converge on the same validated worker command
  (`set_input`), so validation, metadata probing and warnings are identical.
  The drop zone is keyboard-reachable (a native <button>) with an aria-label
  naming the camera and role (UI-SPEC Accessibility). A multi-file drop is
  forwarded as a list so the screen can fill A then B.
-->
<script lang="ts">
  import type { InputRole } from "../lib/types";
  import type { InputSlot } from "../lib/import.svelte";
  import MetadataTable from "./MetadataTable.svelte";

  let {
    slot,
    onChoose,
    onDropFiles,
  }: {
    slot: InputSlot;
    onChoose: (role: InputRole) => void;
    onDropFiles: (role: InputRole, paths: string[]) => void;
  } = $props();

  let dragging = $state(false);

  const label = $derived(
    slot.role === "left" ? "Camera A (left)" : "Camera B (right)",
  );
  const ariaLabel = $derived(`Choose ${label} file`);
  const filename = $derived(
    slot.path === null ? "file" : (slot.path.split(/[\\/]/).pop() ?? slot.path),
  );

  /** Pull the OS path off a dropped file (Tauri exposes `path`). */
  function pathOf(file: File): string {
    const f = file as File & { path?: string };
    return f.path ?? f.name;
  }

  function handleDrop(e: DragEvent): void {
    e.preventDefault();
    dragging = false;
    const files = e.dataTransfer?.files;
    if (!files || files.length === 0) return;
    const paths = Array.from(files).map(pathOf).filter((p) => p.length > 0);
    if (paths.length > 0) onDropFiles(slot.role, paths);
  }
</script>

<section class="drop-slot" aria-label={label}>
  <h3 class="slot-title">{label}</h3>

  <button
    type="button"
    class="drop-zone"
    class:dragging
    class:has-file={slot.status === "ready"}
    aria-label={ariaLabel}
    onclick={() => onChoose(slot.role)}
    ondragover={(e) => {
      e.preventDefault();
      dragging = true;
    }}
    ondragleave={() => (dragging = false)}
    ondrop={handleDrop}
  >
    {#if slot.status === "loading"}
      <span class="drop-text">Reading file…</span>
    {:else if slot.status === "error"}
      <span class="drop-text error">
        Couldn't read <span class="filename" title={slot.path ?? ""}>{filename}</span>: {slot.error ?? "unknown error"}.
        Choose another file.
      </span>
    {:else if slot.status === "ready"}
      <span class="drop-text filename" title={slot.path ?? ""}>{filename}</span>
      <span class="drop-hint">Choose another file…</span>
    {:else}
      <span class="drop-text">Drop a video file here — or <span class="link">Choose file…</span></span>
    {/if}
  </button>

  {#if slot.status === "ready"}
    <MetadataTable metadata={slot.metadata} />
  {:else if slot.status === "loading"}
    <MetadataTable metadata={null} loading />
  {/if}
</section>

<style>
  .drop-slot {
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
    min-height: 180px;
    padding: var(--space-md);
    background: var(--color-secondary);
    border-radius: var(--space-xs);
  }

  .slot-title {
    margin: 0;
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .drop-zone {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-xs);
    min-height: 120px;
    padding: var(--space-md);
    border: 1px dashed var(--color-dominant);
    border-radius: var(--space-xs);
    background: var(--color-dominant);
    color: var(--color-log-info);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    text-align: center;
    cursor: pointer;
    overflow-wrap: anywhere;
  }

  .drop-zone.dragging {
    border-color: var(--color-accent);
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .drop-zone.has-file {
    border-style: solid;
    align-items: flex-start;
    text-align: left;
  }

  .drop-text.error {
    color: var(--color-log-error);
  }

  .filename {
    font-family: var(--font-mono);
    color: var(--color-body-text);
    overflow-wrap: anywhere;
  }

  .drop-hint {
    font-size: 12px;
    color: var(--color-log-info);
  }

  .link {
    color: var(--color-accent);
  }

  .drop-zone:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  @media (prefers-reduced-motion: reduce) {
    .drop-zone {
      transition: none;
    }
  }
</style>
