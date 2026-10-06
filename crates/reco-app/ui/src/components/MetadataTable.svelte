<!--
  Metadata table (IMPT-02): Resolution / Frame rate / Duration / Codec rows,
  each carrying a ProvenanceTag. A missing value renders an em-dash, never `0`.
  While probing, skeleton rows hold the layout (UI-SPEC metadata-table loading).
  Fixed two-column rows; numeric values use the tabular monospace family and
  long values ellipsize with `title` (UI-SPEC metadata-table overflow/long-text).
-->
<script lang="ts">
  import type { InputMetadata, MetadataField } from "../lib/types";
  import ProvenanceTag from "./ProvenanceTag.svelte";

  let {
    metadata,
    loading = false,
  }: { metadata: InputMetadata | null; loading?: boolean } = $props();

  const labels = ["Resolution", "Frame rate", "Duration", "Codec"];

  const rows = $derived(
    metadata === null
      ? []
      : [
          { label: "Resolution", field: metadata.resolution },
          { label: "Frame rate", field: metadata.fps },
          { label: "Duration", field: metadata.duration },
          { label: "Codec", field: metadata.codec },
        ],
  );

  function display(field: MetadataField): string {
    return field.value ?? "—";
  }
</script>

{#if loading}
  <dl class="metadata-table" aria-busy="true" aria-label="Reading metadata">
    {#each labels as label}
      <div class="metadata-row">
        <dt class="metadata-label">{label}</dt>
        <dd class="metadata-value">
          <span class="skeleton" aria-hidden="true"></span>
        </dd>
      </div>
    {/each}
  </dl>
{:else if metadata !== null}
  <dl class="metadata-table">
    {#each rows as row}
      <div class="metadata-row">
        <dt class="metadata-label">{row.label}</dt>
        <dd class="metadata-value">
          <span class="value" title={row.field.value ?? ""}>{display(row.field)}</span>
          <ProvenanceTag provenance={row.field.provenance} />
        </dd>
      </div>
    {/each}
  </dl>
{/if}

<style>
  .metadata-table {
    margin: 0;
    padding: var(--space-sm) 0 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .metadata-row {
    display: grid;
    grid-template-columns: 96px 1fr;
    align-items: baseline;
    gap: var(--space-sm);
    min-height: 28px;
  }

  .metadata-label {
    color: var(--color-log-info);
    font-size: var(--text-body);
  }

  .metadata-value {
    margin: 0;
    display: flex;
    align-items: center;
    gap: var(--space-sm);
    min-width: 0;
  }

  .value {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-body-text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .skeleton {
    display: inline-block;
    width: 120px;
    height: 12px;
    border-radius: var(--space-xs);
    background: var(--color-dominant);
    opacity: 0.7;
  }
</style>
