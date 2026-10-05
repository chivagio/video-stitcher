<!--
  Provenance tag (UI-SPEC): fixed tags `probed` / `estimated` (and `overridden`
  reserved for the lens picker). Estimated values are visibly tagged; nothing
  derived is presented as authoritative (IMPT-02 / D-05).
-->
<script lang="ts">
  import type { Provenance } from "../lib/types";

  let { provenance }: { provenance: Provenance } = $props();
</script>

<span class="tag {provenance}">{provenance}</span>

<style>
  .tag {
    display: inline-block;
    padding: 0 var(--space-xs);
    border: 1px solid transparent;
    border-radius: var(--space-xs);
    font-family: var(--font-mono);
    font-size: 12px;
    line-height: 1.6;
    text-transform: lowercase;
    white-space: nowrap;
  }

  .tag.probed {
    color: var(--color-log-info);
    background: var(--color-dominant);
  }

  .tag.estimated {
    color: var(--color-log-warn);
    border-color: var(--color-log-warn);
  }
</style>
