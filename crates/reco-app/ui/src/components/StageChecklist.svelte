<!--
  Stage checklist (E10 / CALB-01): exactly seven rows, always present, driven
  directly by the typed `CalibrationStage` events. Each row is a status glyph +
  fixed stage name + optional detail line. Statuses: pending (grey) → active
  (accent) → done (success check) / failed (error) / skipped (muted). Only the
  active row is accented; the list scrolls if the window is short.
-->
<script lang="ts">
  import { calibration, CALIBRATION_STAGES, STAGE_NAMES, STAGE_STATUS_WORDS } from "../lib/calibration.svelte";
  import Icon from "./Icon.svelte";
</script>

<ol class="stage-checklist" aria-label="Calibration stages">
  {#each CALIBRATION_STAGES as stage (stage)}
    <li class="stage-row {calibration.stages[stage]}">
      <span class="glyph" aria-hidden="true">
        {#if calibration.stages[stage] === "done"}
          <Icon name="check-circle" />
        {:else if calibration.stages[stage] === "failed"}
          <Icon name="close" />
        {:else if calibration.stages[stage] === "active"}
          <span class="dot"></span>
        {:else if calibration.stages[stage] === "skipped"}
          <span class="dash"></span>
        {:else}
          <span class="ring"></span>
        {/if}
      </span>
      <span class="stage-body">
        <span class="stage-name">{STAGE_NAMES[stage]}</span>
        <span class="stage-status">{STAGE_STATUS_WORDS[calibration.stages[stage]]}</span>
        {#if calibration.detail[stage]}
          <span class="stage-detail">{calibration.detail[stage]}</span>
        {/if}
      </span>
    </li>
  {/each}
</ol>

<style>
  .stage-checklist {
    list-style: none;
    margin: 0;
    padding: 0;
    /* 7 fixed rows; the list scrolls if the window is short (E10 overflow). */
    overflow-y: auto;
  }

  .stage-row {
    display: flex;
    align-items: flex-start;
    gap: var(--space-sm);
    min-height: 32px;
    padding: var(--space-xs) var(--space-sm);
    border-left: 2px solid transparent;
  }

  .stage-row.active {
    border-left-color: var(--color-accent);
  }

  .glyph {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 20px;
    flex: 0 0 auto;
    color: var(--color-log-info);
  }

  .stage-row.done .glyph {
    color: var(--color-success);
  }

  .stage-row.failed .glyph {
    color: var(--color-log-error);
  }

  .stage-row.active .glyph {
    color: var(--color-accent);
  }

  .stage-row.skipped .glyph {
    color: var(--color-log-info);
    opacity: 0.6;
  }

  .ring {
    width: 12px;
    height: 12px;
    border: 2px solid currentColor;
    border-radius: 50%;
    opacity: 0.7;
  }

  .dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    background: currentColor;
    animation: stage-pulse 1.4s ease-in-out infinite;
  }

  .dash {
    width: 12px;
    height: 2px;
    background: currentColor;
  }

  .stage-body {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-sm);
    min-width: 0;
  }

  .stage-name {
    color: var(--color-body-text);
    font-weight: var(--weight-regular);
  }

  .stage-row.active .stage-name {
    color: var(--color-accent);
    font-weight: var(--weight-semibold);
  }

  .stage-row.pending .stage-name,
  .stage-row.skipped .stage-name {
    color: var(--color-log-info);
  }

  .stage-status {
    color: var(--color-log-info);
    font-size: var(--text-body);
  }

  .stage-row.done .stage-status {
    color: var(--color-success);
  }

  .stage-row.failed .stage-status {
    color: var(--color-log-error);
  }

  .stage-detail {
    flex: 1 1 100%;
    color: var(--color-log-info);
    font-size: var(--text-body);
    overflow-wrap: anywhere;
  }

  @keyframes stage-pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.4;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .dot {
      animation: none;
    }
  }
</style>
