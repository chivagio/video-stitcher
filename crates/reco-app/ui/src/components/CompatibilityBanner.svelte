<!--
  Readiness banner (CALB-05): a single non-blocking, severity-sorted readiness
  report grouped into Blocking shape / Likely to reduce quality / Informational.
  Severity is stated in each group heading text (never colour alone). The
  operator may always proceed — `Calibrate anyway` stays enabled; a likely-doomed
  pair adds the escalation line. Estimated overlap/exposure values are labelled
  as estimates; an unavailable value is reported honestly, never faked as a
  number (D3-06 / T-04-08: reasons are authored in Rust and rendered verbatim).

  The filename is retained from the Phase-3 `CompatibilityBanner` so its
  importers stay stable; the body is the grouped readiness behavior.
-->
<script lang="ts">
  import type {
    ReadinessReport,
    ReadinessSeverity,
  } from "../lib/types";
  import ActionButton from "./ActionButton.svelte";
  import Icon from "./Icon.svelte";

  let {
    report,
    onCalibrateAnyway,
    onReviewInputs,
  }: {
    report: ReadinessReport;
    onCalibrateAnyway: () => void;
    onReviewInputs: () => void;
  } = $props();

  /** The locked group headings, in severity order (UI-SPEC Copywriting). */
  const GROUP_HEADINGS: Record<ReadinessSeverity, string> = {
    blocking_shape: "Blocking shape",
    likely_quality: "Likely to reduce quality",
    informational: "Informational",
  };

  const heading = $derived(
    report.findings.length === 1
      ? "1 issue found"
      : `${report.findings.length} issues found`,
  );

  /** Only non-empty groups, in the locked severity order. */
  const groups = $derived(
    (
      [
        "blocking_shape",
        "likely_quality",
        "informational",
      ] as ReadinessSeverity[]
    )
      .map((severity) => ({
        severity,
        heading: GROUP_HEADINGS[severity],
        findings: report.findings.filter((f) => f.severity === severity),
      }))
      .filter((group) => group.findings.length > 0),
  );

  /** A blocking-shape finding means the run is likely doomed. */
  const doomed = $derived(
    report.findings.some((f) => f.severity === "blocking_shape"),
  );

  const overlapPct = $derived(
    report.overlap_estimate === null
      ? null
      : Math.round(report.overlap_estimate * 100),
  );
</script>

{#if report.findings.length > 0}
  <section class="compat-banner" aria-label="Readiness warnings">
    <h3 class="banner-heading">
      <Icon name="warning" />
      {heading}
    </h3>

    {#each groups as group (group.severity)}
      <section class="group" aria-label={group.heading}>
        <h4 class="group-heading">
          <Icon name={group.severity === "informational" ? "info" : "alert-triangle"} />
          {group.heading}
        </h4>
        <ul class="reason-list">
          <!-- Key on the index: two lens-override mismatches can share a code,
               and a keyed each with duplicate keys throws at runtime (Svelte 5). -->
          {#each group.findings as finding, i (i)}
            <li class="reason">
              {finding.message}
              {#if finding.estimated}
                <span class="estimated-tag">estimated</span>
              {/if}
            </li>
          {/each}
        </ul>
      </section>
    {/each}

    <div class="estimates">
      {#if overlapPct !== null}
        <p class="estimate">
          Estimated overlap {overlapPct}% — measured from a sampled frame pair.
        </p>
      {:else}
        <p class="estimate">Overlap is unknown until calibration.</p>
      {/if}
      {#if report.exposure_delta_stops !== null}
        <p class="estimate">
          Exposure differs by {Math.abs(report.exposure_delta_stops).toFixed(1)} stops
          between cameras.
          <span class="estimated-tag">estimated</span>
        </p>
      {/if}
    </div>

    {#if doomed}
      <p class="escalation">
        This pair is likely to fail calibration. Try matching resolution/aspect
        first.
      </p>
    {/if}

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

  .group {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .group-heading {
    margin: 0;
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    color: var(--color-body-text);
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

  .estimated-tag {
    margin-left: var(--space-xs);
    color: var(--color-log-info);
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .estimates {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .estimate {
    margin: 0;
    color: var(--color-log-info);
    font-size: 12px;
  }

  .escalation {
    margin: 0;
    color: var(--color-log-error);
    font-weight: var(--weight-semibold);
  }

  .banner-actions {
    display: flex;
    gap: var(--space-sm);
  }
</style>
