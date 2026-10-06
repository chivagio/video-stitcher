<!--
  Solve-status chip (MANU-03 / UI-SPEC Copywriting Contract).
  Shared by the manual flow chrome: `solving…` while a background solve is in
  flight, `preview shows last solved result` when the preview is stale, and
  `solved` once fresh. Static text plus a subtle affordance; no flashing
  animation (respects `prefers-reduced-motion`).
-->
<script lang="ts">
  import { manual } from "../lib/manual.svelte";

  const label = $derived(
    manual.solving
      ? "solving…"
      : manual.stale
        ? "preview shows last solved result"
        : "solved",
  );
</script>

<span
  class="solve-status"
  class:busy={manual.solving}
  class:stale={manual.stale}
  role="status"
  aria-live="polite"
>
  <span class="dot" aria-hidden="true"></span>
  {label}
</span>

<style>
  .solve-status {
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    color: var(--color-log-info);
  }

  .solve-status.busy {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .solve-status.stale {
    color: var(--color-log-warn);
    border-color: var(--color-log-warn);
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: currentColor;
  }

  .solve-status.busy .dot {
    /* A subtle pulse, disabled under reduced motion below. */
    animation: solve-pulse 1.2s ease-in-out infinite;
  }

  @keyframes solve-pulse {
    0%,
    100% {
      opacity: 0.4;
    }
    50% {
      opacity: 1;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .solve-status.busy .dot {
      animation: none;
      opacity: 1;
    }
  }
</style>
