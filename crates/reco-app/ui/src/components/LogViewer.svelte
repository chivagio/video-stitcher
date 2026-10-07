<!--
  Structured LogViewer (DIAG-02): renders the engine's typed log records as
  `level · target · message` with a monospace timestamp and a level filter.

  The records arrive as typed `LogRecord` events from the worker's tracing
  layer; this component renders them verbatim and never regex-parses log text
  (DIAG-02 prohibition). The list is capped at the carried 2000-line bound
  (T-05-12) by the store.
-->
<script lang="ts">
  import { MAX_LOG_RECORDS, systemStore } from "../lib/system.svelte";
  import type { LogLevelFilter } from "../lib/system.svelte";

  const filters: { id: LogLevelFilter; label: string }[] = [
    { id: "all", label: "All" },
    { id: "info", label: "Info" },
    { id: "warn", label: "Warn" },
    { id: "error", label: "Error" },
  ];

  /** Format an epoch-millisecond timestamp as `HH:MM:SS.mmm` local time. */
  function formatTimestamp(ms: number): string {
    if (ms === 0) return "—";
    const date = new Date(ms);
    const h = String(date.getHours()).padStart(2, "0");
    const m = String(date.getMinutes()).padStart(2, "0");
    const s = String(date.getSeconds()).padStart(2, "0");
    const millis = String(date.getMilliseconds()).padStart(3, "0");
    return `${h}:${m}:${s}.${millis}`;
  }

  const records = $derived(systemStore.filteredRecords);
  const total = $derived(systemStore.records.length);
  // The store evicts only when the count exceeds the cap, so at exactly
  // MAX_LOG_RECORDS nothing has been dropped yet (IN-07).
  const capped = $derived(total > MAX_LOG_RECORDS);
</script>

<div class="log-viewer">
  <div class="log-toolbar" role="group" aria-label="Log level filter">
    {#each filters as filter (filter.id)}
      <button
        type="button"
        class="filter-btn"
        class:active={systemStore.levelFilter === filter.id}
        aria-pressed={systemStore.levelFilter === filter.id}
        onclick={() => systemStore.setLevelFilter(filter.id)}
      >
        {filter.label}
      </button>
    {/each}
    {#if capped}
      <span class="cap-note">
        showing the last {MAX_LOG_RECORDS} records
      </span>
    {/if}
  </div>

  {#if records.length === 0}
    <p class="log-empty">No engine records yet.</p>
  {:else}
    <div class="log-list" role="log" aria-live="polite">
      {#each records as record, i (`${record.timestamp_ms}-${i}`)}
        <div class="log-line level-{record.level}">
          <span class="log-time mono">{formatTimestamp(record.timestamp_ms)}</span>
          <span class="log-level">{record.level.toUpperCase()}</span>
          <span class="log-target">{record.target}</span>
          <span class="log-message">{record.message}</span>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .log-viewer {
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }

  .log-toolbar {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
  }

  .filter-btn {
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    cursor: pointer;
  }

  .filter-btn.active {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .filter-btn:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .cap-note {
    margin-left: auto;
    color: var(--color-log-info);
    font-size: var(--text-label);
  }

  .log-empty {
    margin: 0;
    color: var(--color-log-info);
    font-size: var(--text-body);
  }

  .log-list {
    max-height: 320px;
    overflow-y: auto;
    padding: var(--space-sm);
    border-radius: var(--space-xs);
    background: var(--color-dominant);
    font-family: var(--font-mono);
    font-size: var(--text-body);
    line-height: var(--line-body);
  }

  .log-line {
    display: flex;
    gap: var(--space-sm);
    padding: 2px 0;
    overflow-wrap: anywhere;
  }

  .log-time {
    flex-shrink: 0;
    color: var(--color-log-info);
  }

  .log-level {
    flex-shrink: 0;
    font-weight: var(--weight-semibold);
  }

  .log-line.level-info .log-level {
    color: var(--color-log-info);
  }

  .log-line.level-warn .log-level {
    color: var(--color-log-warn);
  }

  .log-line.level-error .log-level {
    color: var(--color-log-error);
  }

  .log-target {
    flex-shrink: 0;
    color: var(--color-log-info);
  }

  .log-message {
    color: var(--color-body-text);
  }
</style>
