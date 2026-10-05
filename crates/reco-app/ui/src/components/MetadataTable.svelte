<!--
  Metadata table (IMPT-02): Resolution / Frame rate / Duration / Codec rows,
  each carrying a ProvenanceTag. A missing value renders an em-dash, never `0`.
-->
<script lang="ts">
  import type { InputMetadata, MetadataField } from "../lib/types";
  import ProvenanceTag from "./ProvenanceTag.svelte";

  let { metadata }: { metadata: InputMetadata | null } = $props();

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

{#if metadata !== null}
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
    color: var(--color-body-text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
