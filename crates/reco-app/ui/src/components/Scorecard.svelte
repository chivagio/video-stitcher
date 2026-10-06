<!--
  Result scorecard (E13 / CALB-03): exactly the locked field set as a `<dl>` in
  a `secondary` card. Missing fields render "Not reported"; no sync renders
  "None" honestly. Confidence is conveyed by colour PLUS the band word. Long
  profile names ellipsize with the full value in `title`. The card scrolls
  within the screen if the window is short.

  Actions: Save profile… (the profile-save path via the import store) and
  Re-run calibration (owned by the screen, since the advanced options live there).
-->
<script lang="ts">
  import type { ConfidenceBand, Scorecard } from "../lib/types";
  import { importStore } from "../lib/import.svelte";
  import ActionButton from "./ActionButton.svelte";
  import Icon from "./Icon.svelte";

  let {
    scorecard,
    onRerun,
  }: { scorecard: Scorecard; onRerun: () => void } = $props();

  const BAND_WORD: Record<ConfidenceBand, string> = {
    high: "High",
    medium: "Medium",
    low: "Low",
  };
  const SYNC_WORD = {
    imu: "IMU",
    audio: "Audio",
    manual: "Manual",
    none: "None",
  } as const;

  const confidencePct = $derived(Math.round(scorecard.confidence * 100));
  const residual = $derived(scorecard.residual_error.toFixed(2));
  const perFrame = $derived(scorecard.per_frame_matches.toFixed(1));
  const saving = $derived(importStore.profileStatus === "saving");

  const syncText = $derived.by(() => {
    if (scorecard.sync.method === "none") return "None";
    const confidence = scorecard.sync.confidence;
    const label = SYNC_WORD[scorecard.sync.method];
    return confidence === null
      ? `${label} · Not reported`
      : `${label} · ${Math.round(confidence * 100)}%`;
  });
</script>

<section class="scorecard-card">
  <dl class="scorecard">
    <div class="row confidence">
      <dt class="label">Confidence</dt>
      <dd class="value band {scorecard.confidence_band}">
        <span class="confidence-value">{confidencePct}%</span>
        <span class="band-word">{BAND_WORD[scorecard.confidence_band]}</span>
      </dd>
    </div>

    <div class="row">
      <dt class="label">Residual error</dt>
      <dd class="value mono">{residual} px</dd>
    </div>

    <div class="row">
      <dt class="label">Matches</dt>
      <dd class="value mono">
        {scorecard.total_matches} total · {perFrame} per frame
      </dd>
    </div>

    <div class="row">
      <dt class="label">Frames used</dt>
      <dd class="value mono">{scorecard.frames_used}</dd>
    </div>

    <div class="row">
      <dt class="label">Lens profile</dt>
      <dd class="value">
        {#if scorecard.lens_profile !== null}
          <span
            class="ellipsis"
            title={`${scorecard.lens_profile.name} (${scorecard.lens_profile.source})`}
          >
            {scorecard.lens_profile.name} ({scorecard.lens_profile.source})
          </span>
        {:else}
          Not reported
        {/if}
      </dd>
    </div>

    <div class="row">
      <dt class="label">Sync</dt>
      <dd class="value">{syncText}</dd>
    </div>
  </dl>

  <div class="scorecard-actions">
    <ActionButton
      variant="primary"
      disabled={saving}
      onClick={() => void importStore.saveProfile()}
    >
      <Icon name="save" /> {saving ? "Saving…" : "Save profile…"}
    </ActionButton>
    <ActionButton variant="secondary" onClick={onRerun}>
      <Icon name="refresh" /> Re-run calibration
    </ActionButton>
  </div>

  {#if importStore.profileStatus === "error" && importStore.profileError !== null}
    <p class="save-line error">
      Couldn't save the profile: {importStore.profileError}. Choose another
      location.
    </p>
  {:else if importStore.lastSavedPath !== null}
    <p class="save-line success">
      Saved profile to <span class="mono">{importStore.lastSavedPath}</span>.
    </p>
  {/if}
</section>

<style>
  .scorecard-card {
    padding: var(--space-lg);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
  }

  .scorecard {
    margin: 0;
    /* Fixed two-column rows; the list scrolls if the window is short. */
    overflow-y: auto;
  }

  .row {
    display: grid;
    grid-template-columns: minmax(120px, 200px) 1fr;
    gap: var(--space-md);
    align-items: baseline;
    min-height: 28px;
    padding: var(--space-xs) 0;
  }

  .label {
    color: var(--color-log-info);
    font-weight: var(--weight-semibold);
  }

  .value {
    margin: 0;
    color: var(--color-body-text);
    min-width: 0;
  }

  .mono {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }

  .confidence .value {
    display: inline-flex;
    align-items: baseline;
    gap: var(--space-sm);
  }

  .confidence-value {
    font-size: var(--text-display);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
  }

  .band.high .confidence-value,
  .band.high .band-word {
    color: var(--color-success);
  }

  .band.medium .confidence-value,
  .band.medium .band-word {
    color: var(--color-log-warn);
  }

  .band.low .confidence-value,
  .band.low .band-word {
    color: var(--color-log-error);
  }

  .ellipsis {
    display: inline-block;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    vertical-align: bottom;
  }

  .scorecard-actions {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
    margin-top: var(--space-lg);
  }

  .save-line {
    margin: var(--space-sm) 0 0;
    font-size: var(--text-body);
    overflow-wrap: anywhere;
  }

  .save-line.success {
    color: var(--color-success);
  }

  .save-line.error {
    color: var(--color-log-error);
  }
</style>
