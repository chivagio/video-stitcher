<!--
  Provenance tag (UI-SPEC): fixed tags `probed` / `estimated` for metadata and
  `detected` / `estimated` / `overridden` for lens rows. Estimated values are
  visibly tagged; nothing derived is presented as authoritative (IMPT-02 / D-05).
  The fixed vocabulary carries no user text and never overflows.
-->
<script lang="ts">
  import type { Provenance } from "../lib/types";

  /** The fixed tag vocabulary (UI-SPEC Provenance tags + Lens Profile Contract). */
  export type TagKind = Provenance | "detected" | "overridden";

  let { provenance }: { provenance: TagKind } = $props();
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

  .tag.probed,
  .tag.detected {
    color: var(--color-log-info);
    background: var(--color-dominant);
  }

  .tag.estimated {
    color: var(--color-log-warn);
    border-color: var(--color-log-warn);
  }

  /* The operator's own lens choice: the reserved accent tag (UI-SPEC Color
     reserved list item 5). */
  .tag.overridden {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }
</style>
