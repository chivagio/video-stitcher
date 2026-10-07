<!--
  Shared action-button primitive (UI-SPEC Component Inventory).
  Variants: primary (accent fill) · secondary (resting surface) ·
  destructive (error outline). Carries the Phase 2 focus ring.
-->
<script lang="ts">
  import type { Snippet } from "svelte";

  let {
    variant = "secondary",
    disabled = false,
    title,
    ariaLabel,
    onClick,
    children,
  }: {
    variant?: "primary" | "secondary" | "destructive";
    disabled?: boolean;
    title?: string;
    /** Accessible name; overrides the visible label when the action needs more context. */
    ariaLabel?: string | undefined;
    onClick: () => void;
    children: Snippet;
  } = $props();
</script>

<button
  type="button"
  class="action-btn {variant}"
  {disabled}
  title={title ?? ""}
  aria-label={ariaLabel}
  onclick={onClick}
>
  {@render children()}
</button>

<style>
  .action-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: var(--space-sm);
    min-height: 36px;
    padding: 8px 16px;
    border: 1px solid transparent;
    border-radius: var(--space-xs);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    cursor: pointer;
    transition:
      background 150ms ease,
      color 150ms ease,
      border-color 150ms ease;
  }

  .action-btn.primary {
    background: var(--color-accent);
    color: var(--color-dominant);
  }

  .action-btn.secondary {
    background: var(--color-secondary);
    color: var(--color-body-text);
    border-color: var(--color-dominant);
  }

  .action-btn.destructive {
    background: transparent;
    color: var(--color-log-error);
    border-color: var(--color-log-error);
  }

  .action-btn:hover:not(:disabled) {
    filter: brightness(1.1);
  }

  .action-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .action-btn:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  @media (prefers-reduced-motion: reduce) {
    .action-btn {
      transition: none;
    }
  }
</style>
