<!--
  Export screen (EXPT-01 / EXPT-02 / EXPT-03 / EXPT-05 / EXPT-06): a form
  (preset, encoder, variant, trim summary, naming preview) plus the MODAL
  progress state (percent bar, frames done/total, elapsed, ETA, Cancel) and the
  completion / cancelled / failed states.

  The variant picker offers the three locked compositions — Panorama,
  Side-by-side, Stacked — whose labels are the `VARIANT_LABELS` copy authored
  once in `lib/export.svelte.ts`. The trim summary mirrors the timeline's in/out
  (EXPT-03), and the output naming preview renders the worker-resolved path
  (`ExportPathPreview`) verbatim, with a collision note when a suffix was added
  (EXPT-06).

  The worker is authoritative: progress, the resolved encoder, and the final
  path are the worker's typed values rendered verbatim — the screen never
  fabricates a value or claims a path the worker did not resolve (UI-SPEC
  Real-values rule / T-05-02).

  Export is modal (CONTEXT decision): while the run is in flight the form is
  replaced by the progress surface and the rail is disabled by the parent.
-->
<script lang="ts">
  import {
    exportStore,
    PRESET_LABELS,
    VARIANT_LABELS,
  } from "../lib/export.svelte";
  import type { ExportPreset, ExportVariant } from "../lib/types";
  import ActionButton from "./ActionButton.svelte";
  import InlineNotice from "./InlineNotice.svelte";

  let {
    formatTimecode,
  }: {
    /**
     * Format a frame index as the transport's exact timecode (EXPT-03). The
     * Preview timeline and this form must render the same trim window the same
     * way, so the form borrows the transport formatter rather than showing raw
     * frame indices.
     */
    formatTimecode?: (frame: number) => string;
  } = $props();

  const presets = Object.keys(PRESET_LABELS) as ExportPreset[];
  const variants = Object.keys(VARIANT_LABELS) as ExportVariant[];

  const running = $derived(exportStore.status === "running");
  const done = $derived(exportStore.status === "done" && exportStore.result !== null);
  const cancelled = $derived(exportStore.status === "cancelled");
  const failed = $derived(exportStore.status === "failed");

  // The webview never constructs an output path (T-05-08): until the worker's
  // `ExportPathPreview` lands, show a neutral resolving state rather than a
  // locally-derived name that could differ from the worker's sanitized,
  // collision-suffixed path (IN-06).
  const outputPath = $derived(exportStore.pathPreview?.path ?? "Resolving…");
  const percent = $derived(Math.round(exportStore.progress?.percent ?? 0));
  const encoderOverride = $derived(exportStore.settings.encoder_name ?? "");

  // The worker has no reveal/open-location capability, so the completion
  // affordance is the least-privilege one: copy the final path to the
  // clipboard (the path stays selectable as a fallback).
  let copied = $state(false);

  async function copyPath(): Promise<void> {
    const path = exportStore.result?.path;
    if (!path) return;
    try {
      await navigator.clipboard.writeText(path);
    } catch {
      // Webview clipboard access may be unavailable; fall back to selecting a
      // hidden textarea so the operator can still copy manually.
      const area = document.createElement("textarea");
      area.value = path;
      area.setAttribute("readonly", "");
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      document.execCommand("copy");
      document.body.removeChild(area);
    }
    copied = true;
  }

  function handlePreset(e: Event): void {
    exportStore.setPreset((e.currentTarget as HTMLSelectElement).value as ExportPreset);
  }

  function handleEncoder(e: Event): void {
    const value = (e.currentTarget as HTMLSelectElement).value;
    exportStore.setEncoderOverride(value === "" ? null : value);
  }

  function handleVariant(e: Event): void {
    exportStore.setVariant((e.currentTarget as HTMLSelectElement).value as ExportVariant);
  }
</script>

<div class="export-screen">
  <div class="export-column">
    <header class="screen-header">
      <h2 class="screen-title">Export</h2>
      <p class="screen-subtitle">
        Choose a preset and encoder, then export the stitched result.
      </p>
    </header>

    {#if running}
      <!-- Modal progress state: owns the screen while the job runs. -->
      <section class="modal-progress" aria-labelledby="export-progress-heading">
        <h3 id="export-progress-heading" class="progress-heading">Exporting…</h3>

        <div
          class="progress-bar"
          role="progressbar"
          aria-valuemin="0"
          aria-valuemax="100"
          aria-valuenow={percent}
          aria-valuetext={exportStore.progressText || "Starting export"}
        >
          <div class="fill" style={`width: ${percent}%`}></div>
        </div>

        <p class="progress-line" role="status">{exportStore.progressText || "Starting export…"}</p>

        <ActionButton
          variant="destructive"
          ariaLabel="Cancel export"
          onClick={() => void exportStore.cancelExport()}
        >
          Cancel export
        </ActionButton>
      </section>
    {:else if done && exportStore.result}
      <section class="result" aria-labelledby="export-done-heading">
        <h3 id="export-done-heading" class="progress-heading">Export complete</h3>
        <p class="path-line">
          Saved to <span class="mono path-value">{exportStore.result.path}</span>
        </p>
        <p class="meta-line">
          Encoder: <span class="mono">{exportStore.result.encoder}</span>
          ({exportStore.result.hardware ? "hardware" : "software"})
        </p>
        <div class="actions">
          <ActionButton
            variant="secondary"
            ariaLabel="Copy the exported file path"
            onClick={() => void copyPath()}
          >
            {copied ? "Copied" : "Copy path"}
          </ActionButton>
          <ActionButton
            variant="secondary"
            onClick={() => {
              copied = false;
              exportStore.reset();
            }}
          >
            Export again
          </ActionButton>
        </div>
        <p class="copy-status" role="status" aria-live="polite">
          {copied ? "Path copied to the clipboard." : ""}
        </p>
      </section>
    {:else if cancelled}
      <InlineNotice level="warn" message="Export cancelled — no file was written." />
      <div class="actions">
        <ActionButton variant="secondary" onClick={() => exportStore.reset()}>
          Export again
        </ActionButton>
      </div>
    {:else}
      <!-- Ready / failed form. -->
      {#if failed && exportStore.error !== null}
        <InlineNotice level="error" message={exportStore.error} />
      {/if}

      {#if exportStore.softwareFallback}
        <InlineNotice level="warn" message={exportStore.fallbackMessage} />
      {/if}

      <section class="form">
        <label class="field">
          <span class="field-label">Preset</span>
          <span class="select-wrap">
            <select class="field-input" aria-label="Preset" value={exportStore.preset} onchange={handlePreset}>
              {#each presets as preset}
                <option value={preset}>{PRESET_LABELS[preset]}</option>
              {/each}
            </select>
            <span class="select-caret" aria-hidden="true">
              <svg
                width="16"
                height="16"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
              >
                <polyline points="6 9 12 15 18 9" />
              </svg>
            </span>
          </span>
        </label>

        <label class="field">
          <span class="field-label">Encoder</span>
          <span class="select-wrap">
            <select class="field-input" aria-label="Encoder" value={encoderOverride} onchange={handleEncoder}>
              <option value="">
                Auto{exportStore.auto ? ` (${exportStore.auto.name})` : ""}
              </option>
              {#each exportStore.encoders as enc (enc.name)}
                <option value={enc.name}>
                  {enc.name} ({enc.is_hardware ? "hardware" : "software"})
                </option>
              {/each}
            </select>
            <span class="select-caret" aria-hidden="true">
              <svg
                width="16"
                height="16"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
              >
                <polyline points="6 9 12 15 18 9" />
              </svg>
            </span>
          </span>
        </label>

        <label class="field">
          <span class="field-label">Variant</span>
          <span class="select-wrap">
            <select class="field-input" aria-label="Variant" value={exportStore.settings.variant} onchange={handleVariant}>
              {#each variants as variant}
                <option value={variant}>{VARIANT_LABELS[variant]}</option>
              {/each}
            </select>
            <span class="select-caret" aria-hidden="true">
              <svg
                width="16"
                height="16"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
              >
                <polyline points="6 9 12 15 18 9" />
              </svg>
            </span>
          </span>
        </label>
      </section>

      <p class="trim-line">
        Trim: <span class="mono">{exportStore.deriveTrimSummary(formatTimecode)}</span>
      </p>
      <p class="path-line">
        Output: <span class="mono">{outputPath}</span>
      </p>
      {#if exportStore.pathPreviewCollision}
        <InlineNotice
          level="warn"
          message={`Name in use — saving as ${outputPath}`}
        />
      {/if}
      <p class="meta-line">
        <span class="mono">{exportStore.settings.width}×{exportStore.settings.height}</span>
        · <span class="mono">{exportStore.settings.codec}</span>
        · <span class="mono">{exportStore.settings.quality}</span>
      </p>

      <div class="actions">
        <ActionButton variant="primary" onClick={() => void exportStore.startExport()}>
          Export
        </ActionButton>
        <ActionButton
          variant="secondary"
          disabled={exportStore.probing}
          onClick={() => void exportStore.probeEncoders()}
        >
          {exportStore.probing ? "Probing…" : "Probe encoders"}
        </ActionButton>
      </div>
    {/if}
  </div>
</div>

<style>
  .export-screen {
    position: fixed;
    top: var(--workflow-rail-height);
    left: 0;
    right: 0;
    bottom: 0;
    /* Opaque dominant: the native preview surface must not show through
       (UI-SPEC Screen Router). Rust suspends the native view on this screen. */
    background: var(--color-dominant);
    overflow-y: auto;
  }

  .export-column {
    max-width: 720px;
    margin: 0 auto;
    padding: var(--space-3xl) var(--space-lg);
    display: flex;
    flex-direction: column;
    gap: var(--space-xl);
  }

  .screen-title {
    margin: 0;
    font-size: var(--text-display);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .screen-subtitle {
    margin: var(--space-sm) 0 0;
    color: var(--color-log-info);
    max-width: 60ch;
  }

  .form {
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    min-width: 0;
  }

  .field-label {
    color: var(--color-log-info);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
  }

  .field-input {
    flex: 1;
    min-height: 36px;
    padding: var(--space-xs) var(--space-sm);
    /* Leave room for the 16px caret at the right edge plus a gap, so the
       selected value never sits under it. */
    padding-right: calc(var(--space-sm) + 16px + var(--space-xs));
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
    /* Drop the native widget appearance: under WebKitGTK an unstyled select is
       painted by the GTK menulist (near-white), which bypasses the authored
       background below and leaves light-on-light text (Phase 5 UI review
       BLOCKER). Mirrors the Phase 2 presenter-select fix. */
    -webkit-appearance: none;
    appearance: none;
    /* background: shorthand first, then explicitly clear any UA gradient/image
       so nothing paints behind the authored colour. */
    background: var(--color-secondary);
    background-image: none;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
  }

  /* Wraps the select + caret so the caret positions against the control row. */
  .select-wrap {
    position: relative;
    display: flex;
  }

  .select-caret {
    position: absolute;
    right: var(--space-sm);
    top: 50%;
    transform: translateY(-50%);
    display: flex;
    align-items: center;
    color: var(--color-body-text);
    /* Never intercept the click that opens the menu. */
    pointer-events: none;
  }

  .field-input:focus-visible {
    outline: none;
    border-color: var(--color-accent);
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .trim-line,
  .path-line,
  .meta-line {
    margin: 0;
    color: var(--color-log-info);
    font-size: var(--text-body);
    overflow-wrap: anywhere;
  }

  .mono {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-body-text);
  }

  /* The final path stays selectable so it can be copied by hand even when the
     clipboard API is unavailable (least-privilege Reveal substitute). */
  .path-value {
    user-select: text;
  }

  .copy-status {
    min-height: var(--text-body);
    margin: 0;
    color: var(--color-success);
    font-size: var(--text-body);
  }

  .modal-progress,
  .result {
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
    padding: var(--space-lg);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
  }

  .progress-heading {
    margin: 0;
    font-size: var(--text-display);
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .progress-bar {
    height: 8px;
    border-radius: 4px;
    background: var(--color-dominant);
    overflow: hidden;
  }

  .fill {
    height: 100%;
    /* Guarantee a visible sliver from the first frame: at 0-1% a plain width
       fill is nearly imperceptible at the start of a long run (Phase 5 UI
       review). The accent fill reads against the dominant track. */
    min-width: 4px;
    background: var(--color-accent);
    transition: width 250ms ease;
  }

  .progress-line {
    margin: 0;
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-body-text);
    overflow-wrap: anywhere;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }

  @media (prefers-reduced-motion: reduce) {
    .fill {
      transition: none;
    }
  }
</style>
