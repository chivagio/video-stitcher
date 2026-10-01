<!--
  Event-log drawer (UI-SPEC Component Inventory).
  role="log" + aria-live="polite"; reuses Phase 1 Event Log Contract.
  Fixed 240px scroll region; auto-scroll pauses while the user is scrolled up.
  DOM capped at 2000 lines.
-->
<script lang="ts">
  import { log } from "../lib/log.svelte";

  let {
    expanded,
    onToggle,
  }: {
    expanded: boolean;
    onToggle: () => void;
  } = $props();

  let scrollEl = $state<HTMLDivElement | null>(null);

  $effect(() => {
    if (scrollEl && !log.autoScrollPaused) {
      scrollEl.scrollTop = scrollEl.scrollHeight;
    }
  });

  function handleScroll(): void {
    if (!scrollEl) return;
    const atBottom =
      scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight < 40;
    log.setAutoScrollPaused(!atBottom);
  }

  const entries = $derived(log.entries);
</script>

<div class="log-drawer" class:expanded>
  <button
    type="button"
    class="log-toggle"
    aria-label={expanded ? "Hide log" : "Show log"}
    title={expanded ? "Hide log" : "Show log"}
    aria-expanded={expanded}
    aria-controls="log-drawer-content"
    onclick={onToggle}
  >
    <span class="log-toggle-label">Log</span>
  </button>

  {#if expanded}
    <div
      id="log-drawer-content"
      class="log-content"
      role="log"
      aria-live="polite"
      bind:this={scrollEl}
      onscroll={handleScroll}
    >
      {#if entries.length === 0}
        <div class="log-empty">
          <h3 class="log-empty-heading">No events yet</h3>
          <p class="log-empty-body">Worker events will appear here.</p>
        </div>
      {:else}
        {#each entries as entry (entry.time + entry.message)}
          <div class="log-line level-{entry.level}">
            <span class="log-time">[{entry.time}]</span>
            <span class="log-level">{entry.level.toUpperCase()}</span>
            <span class="log-message">{entry.message}</span>
          </div>
        {/each}
      {/if}
    </div>
  {/if}
</div>

<style>
  .log-drawer {
    position: fixed;
    left: 0;
    bottom: var(--transport-bar-height);
    width: calc(100% - var(--controls-panel-collapsed-width));
    height: 0;
    background: var(--color-dominant);
    transition: height 200ms ease;
    z-index: 5;
    display: flex;
    flex-direction: column;
  }

  .log-drawer.expanded {
    height: var(--log-drawer-height);
  }

  .log-toggle {
    display: flex;
    align-items: center;
    justify-content: flex-start;
    min-height: 32px;
    padding: var(--space-sm) var(--space-md);
    border: none;
    background: transparent;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    cursor: pointer;
  }

  .log-toggle:hover {
    background: var(--color-secondary);
  }

  .log-toggle:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .log-content {
    flex: 1;
    overflow-y: auto;
    padding: 0 var(--space-md) var(--space-md);
    font-family: var(--font-mono);
    font-size: var(--text-body);
    line-height: var(--line-body);
  }

  .log-empty {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    padding: var(--space-md) 0;
  }

  .log-empty-heading {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .log-empty-body {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-body);
    color: var(--color-log-info);
  }

  .log-line {
    display: flex;
    gap: var(--space-sm);
    padding: 2px 0;
    overflow-wrap: anywhere;
  }

  .log-time {
    color: var(--color-log-info);
    flex-shrink: 0;
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

  .log-message {
    color: var(--color-log-info);
  }

  @media (prefers-reduced-motion: reduce) {
    .log-drawer {
      transition: none;
    }
  }
</style>
