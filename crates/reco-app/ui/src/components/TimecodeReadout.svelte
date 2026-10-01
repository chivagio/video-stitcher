<!--
  Timecode readout (UI-SPEC Component Inventory).
  Monospace current / duration; reflects worker position events.
  Fixed-width monospace bounded by clip length; a 4-hour clip still fits (H:MM:SS).
-->
<script lang="ts">
  let {
    current,
    duration,
  }: {
    current: string;
    duration: string;
  } = $props();
</script>

<span class="timecode" aria-label="{current} of {duration}">
  <span class="timecode-current">{current}</span>
  <span class="timecode-sep"> / </span>
  <span class="timecode-duration">{duration}</span>
</span>

<style>
  .timecode {
    font-family: var(--font-mono);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }

  .timecode-sep {
    color: var(--color-log-info);
  }
</style>
