<!--
  Per-frame match-count table (CALB-08 / E6). A real `<table>` with the five
  locked headers and one row per frame that produced matches. The body scrolls
  vertically inside a bounded height; the empty state names what is missing
  rather than rendering a header row with no data.
-->
<script lang="ts">
  import type { FrameMatchRow } from "../lib/types";

  let { rows }: { rows: FrameMatchRow[] } = $props();
</script>

{#if rows.length === 0}
  <p class="empty">No match data for this run.</p>
{:else}
  <div class="table-scroll">
    <table>
      <thead>
        <tr>
          <th scope="col">Frame</th>
          <th scope="col">Keypoints L/R</th>
          <th scope="col">After ratio</th>
          <th scope="col">After spatial</th>
          <th scope="col">After RANSAC</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as row (row.frame)}
          <tr>
            <td>{row.frame + 1}</td>
            <td>{row.keypoints_left}/{row.keypoints_right}</td>
            <td>{row.post_ratio_test}</td>
            <td>{row.post_spatial_filter}</td>
            <td>{row.post_ransac}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{/if}

<style>
  .table-scroll {
    max-height: 240px;
    overflow-y: auto;
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-family: var(--font-mono);
    font-size: var(--text-body);
  }

  th,
  td {
    padding: var(--space-xs) var(--space-sm);
    text-align: right;
    white-space: nowrap;
  }

  th:first-child,
  td:first-child {
    text-align: left;
  }

  thead th {
    position: sticky;
    top: 0;
    background: var(--color-secondary);
    color: var(--color-log-info);
    font-weight: var(--weight-semibold);
  }

  tbody tr:nth-child(even) {
    background: var(--color-secondary);
  }

  td {
    color: var(--color-body-text);
  }

  .empty {
    margin: 0;
    color: var(--color-log-info);
  }
</style>
