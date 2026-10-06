<!--
  Non-blocking notice strip (UI-SPEC Component Inventory): warn/error/info
  inline notices for checks-unavailable, result-invalidated, and save/load
  failures. It never blocks a primary action; it is a polite status region.
-->
<script lang="ts">
  import type { Snippet } from "svelte";

  let {
    level = "warn",
    message,
    onDismiss,
    children,
  }: {
    level?: "info" | "warn" | "error";
    message: string;
    onDismiss?: () => void;
    children?: Snippet;
  } = $props();
</script>

<div class="inline-notice {level}" role="status">
  <span class="notice-text">{message}</span>
  {#if children}
    {@render children()}
  {/if}
  {#if onDismiss}
    <button
      type="button"
      class="notice-dismiss"
      aria-label="Dismiss notice"
      onclick={onDismiss}
    >
      Dismiss
    </button>
  {/if}
</div>

<style>
  .inline-notice {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-sm);
    padding: var(--space-sm) var(--space-md);
    border-left: 3px solid transparent;
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    font-size: var(--text-body);
    overflow-wrap: anywhere;
  }

  .inline-notice.warn {
    border-left-color: var(--color-log-warn);
    color: var(--color-log-warn);
  }

  .inline-notice.error {
    border-left-color: var(--color-log-error);
    color: var(--color-log-error);
  }

  .inline-notice.info {
    border-left-color: var(--color-log-info);
    color: var(--color-log-info);
  }

  .notice-text {
    flex: 1 1 auto;
    min-width: 0;
  }

  .notice-dismiss {
    flex: 0 0 auto;
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid transparent;
    border-radius: var(--space-xs);
    background: transparent;
    color: inherit;
    font-family: var(--font-ui);
    font-size: var(--text-body);
    cursor: pointer;
  }

  .notice-dismiss:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }
</style>
