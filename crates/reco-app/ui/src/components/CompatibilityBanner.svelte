<!--
  Compatibility warning banner (IMPT-03 / D3-05): a single non-blocking warn
  banner listing the failed advisory checks with a `Calibrate anyway` confirm.
  It is never a hard block — the operator may know the clips better than the
  heuristic. The passing remainder collapses to "Other checks passed.", the
  reasons list scrolls inside a 160px max-height, and overlap is honestly
  reported as unknown until calibration (D3-06).
-->
<script lang="ts">
  import type { CompatibilityIssue } from "../lib/types";
  import ActionButton from "./ActionButton.svelte";
  import Icon from "./Icon.svelte";

  let {
    issues,
    onCalibrateAnyway,
    onReviewInputs,
  }: {
    issues: CompatibilityIssue[];
    onCalibrateAnyway: () => void;
    onReviewInputs: () => void;
  } = $props();

  const heading = $derived(
    issues.length === 1 ? "1 issue found" : `${issues.length} issues found`,
  );
</script>

{#if issues.length > 0}
  <section class="compat-banner" aria-label="Compatibility warnings">
    <h3 class="banner-heading">
      <Icon name="warning" />
      {heading}
    </h3>
    <ul class="reason-list">
      {#each issues as issue (issue.code)}
        <li class="reason">{issue.message}</li>
      {/each}
    </ul>
    <p class="banner-note">
      Other checks passed. Overlap is unknown until calibration.
    </p>
    <div class="banner-actions">
      <ActionButton variant="primary" onClick={onCalibrateAnyway}>
        Calibrate anyway
      </ActionButton>
      <ActionButton variant="secondary" onClick={onReviewInputs}>
        Review inputs
      </ActionButton>
    </div>
  </section>
{/if}

<style>
  .compat-banner {
    padding: var(--space-md);
    border: 1px solid var(--color-log-warn);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }

  .banner-heading {
    margin: 0;
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    color: var(--color-log-warn);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
  }

  .reason-list {
    margin: 0;
    padding: 0 0 0 var(--space-md);
    max-height: 160px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .reason {
    color: var(--color-body-text);
    overflow-wrap: anywhere;
  }

  .banner-note {
    margin: 0;
    color: var(--color-log-info);
    font-size: 12px;
  }

  .banner-actions {
    display: flex;
    gap: var(--space-sm);
  }
</style>
