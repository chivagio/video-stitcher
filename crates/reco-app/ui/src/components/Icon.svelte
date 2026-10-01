<!--
  Inline SVG icon (UI-SPEC Design System: no icon library).
  One shared snippet, stroke="currentColor", 16/20px.
-->
<script lang="ts">
  export type IconName =
    | "play"
    | "pause"
    | "step-back"
    | "step-forward"
    | "loop"
    | "source"
    | "panorama"
    | "log"
    | "controls"
    | "reset"
    | "window";

  let { name, size = 16 }: { name: IconName; size?: number } = $props();
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width="2"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === "play"}
    <polygon points="6 3 20 12 6 21 6 3" fill="currentColor" stroke="none" />
  {:else if name === "pause"}
    <rect x="6" y="4" width="4" height="16" fill="currentColor" stroke="none" />
    <rect x="14" y="4" width="4" height="16" fill="currentColor" stroke="none" />
  {:else if name === "step-back"}
    <polygon points="19 20 9 12 19 4 19 20" fill="currentColor" stroke="none" />
    <line x1="5" y1="19" x2="5" y2="5" />
  {:else if name === "step-forward"}
    <polygon points="5 4 15 12 5 20 5 4" fill="currentColor" stroke="none" />
    <line x1="19" y1="5" x2="19" y2="19" />
  {:else if name === "loop"}
    <polyline points="17 1 21 5 17 9" />
    <path d="M3 11V9a4 4 0 0 1 4-4h14" />
    <polyline points="7 23 3 19 7 15" />
    <path d="M21 13v2a4 4 0 0 1-4 4H3" />
  {:else if name === "source"}
    <rect x="3" y="3" width="7" height="18" rx="1" />
    <rect x="14" y="3" width="7" height="18" rx="1" />
  {:else if name === "panorama"}
    <rect x="3" y="5" width="18" height="14" rx="1" />
    <circle cx="12" cy="12" r="3" />
  {:else if name === "log"}
    <path d="M8 6h13" />
    <path d="M8 12h13" />
    <path d="M8 18h13" />
    <path d="M3 6h.01" />
    <path d="M3 12h.01" />
    <path d="M3 18h.01" />
  {:else if name === "controls"}
    <rect x="3" y="3" width="18" height="18" rx="2" />
    <path d="M9 3v18" />
  {:else if name === "reset"}
    <path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" />
    <path d="M3 3v5h5" />
  {:else if name === "window"}
    <rect x="3" y="3" width="18" height="18" rx="2" />
    <path d="M3 9h18" />
  {/if}
</svg>
