<!--
  View toggle (UI-SPEC Component Inventory).
  Single button switching preview between source and panorama.
  Label states the view it switches TO. Uses aria-pressed.
-->
<script lang="ts">
  import Icon from "./Icon.svelte";

  let {
    mode,
    disabled = false,
    onToggle,
  }: {
    mode: "source" | "panorama";
    disabled?: boolean;
    onToggle: () => void;
  } = $props();

  const isSource = $derived(mode === "source");
  const label = $derived(isSource ? "Show panorama" : "Show source");
  const icon = $derived(isSource ? "panorama" : "source");
</script>

<button
  type="button"
  class="view-toggle"
  class:active={isSource}
  {disabled}
  aria-label={label}
  title={label}
  aria-pressed={isSource}
  onclick={onToggle}
>
  <Icon name={icon} size={20} />
  <span class="view-toggle-label">{label}</span>
</button>

<style>
  .view-toggle {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    min-height: 32px;
    padding: var(--space-sm);
    border: 1px solid transparent;
    border-radius: var(--space-xs);
    background: var(--color-dominant);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    cursor: pointer;
    transition:
      background 150ms ease,
      color 150ms ease;
  }

  .view-toggle:hover:not(:disabled) {
    background: #333333;
  }

  .view-toggle:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .view-toggle.active {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .view-toggle:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  @media (prefers-reduced-motion: reduce) {
    .view-toggle {
      transition: none;
    }
  }
</style>
