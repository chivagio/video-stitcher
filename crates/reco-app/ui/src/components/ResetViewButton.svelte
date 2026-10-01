<!--
  Reset view button (UI-SPEC Component Inventory).
  Sends PoseIntent::Reset via ControlIntent.
-->
<script lang="ts">
  import Icon from "./Icon.svelte";

  let {
    disabled = false,
    onClick,
  }: {
    disabled?: boolean;
    onClick: () => void;
  } = $props();
</script>

<button
  type="button"
  class="reset-btn"
  {disabled}
  aria-label="Reset view"
  title="Reset view"
  onclick={onClick}
>
  <Icon name="reset" size={16} />
  <span class="reset-label">Reset view</span>
</button>

<style>
  .reset-btn {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    min-height: 32px;
    padding: var(--space-sm) var(--space-md);
    border: 1px solid transparent;
    border-radius: var(--space-xs);
    background: var(--color-dominant);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    cursor: pointer;
    transition: background 150ms ease;
  }

  .reset-btn:hover:not(:disabled) {
    background: #333333;
  }

  .reset-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .reset-btn:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  @media (prefers-reduced-motion: reduce) {
    .reset-btn {
      transition: none;
    }
  }
</style>
