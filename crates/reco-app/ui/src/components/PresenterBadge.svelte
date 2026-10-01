<!--
  Presenter badge (UI-SPEC Component Inventory).
  role="status" badge: Native / Separate window / Readback.
  Native is neutral; Separate window and Readback use the warn colour.
  Max width 220px, single line; text overflows with ellipsis while the
  full label stays available via title and aria-label.
-->
<script lang="ts">
  let {
    label,
    isWarning = false,
  }: {
    label: string;
    isWarning?: boolean;
  } = $props();
</script>

<span
  class="presenter-badge"
  class:warning={isWarning}
  role="status"
  aria-live="polite"
  title={label}
  aria-label={label}
>
  {label}
</span>

<style>
  .presenter-badge {
    display: inline-block;
    max-width: 220px;
    padding: var(--space-xs) var(--space-sm);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .presenter-badge.warning {
    color: var(--color-log-warn);
  }
</style>
