<!--
  Transport icon button (UI-SPEC Component Inventory).
  Every icon-only button carries aria-label and a title tooltip.
-->
<script lang="ts">
  import Icon, { type IconName } from "./Icon.svelte";

  let {
    icon,
    label,
    active = false,
    disabled = false,
    onClick,
  }: {
    icon: IconName;
    label: string;
    active?: boolean;
    disabled?: boolean;
    onClick: () => void;
  } = $props();
</script>

<button
  type="button"
  class="transport-btn"
  class:active
  {disabled}
  aria-label={label}
  title={label}
  aria-pressed={active}
  onclick={onClick}
>
  <Icon name={icon} size={20} />
</button>

<style>
  .transport-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    min-width: 32px;
    min-height: 32px;
    padding: var(--space-sm);
    border: 1px solid transparent;
    border-radius: var(--space-xs);
    background: var(--color-dominant);
    color: var(--color-body-text);
    cursor: pointer;
    transition:
      background 150ms ease,
      color 150ms ease;
  }

  .transport-btn:hover:not(:disabled) {
    background: #333333;
  }

  .transport-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .transport-btn.active {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .transport-btn:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  @media (prefers-reduced-motion: reduce) {
    .transport-btn {
      transition: none;
    }
  }
</style>
