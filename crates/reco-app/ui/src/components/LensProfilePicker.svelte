<!--
  Lens profile picker (IMPT-04 / D3-07): a searchable dropdown per slot,
  populated from the engine's resolution-aware candidates, with a brand→model
  browse and a `Load profile file…` path. Default is engine auto-detect; picking
  a profile tags the row `overridden` (UI-SPEC Lens Profile Contract).

  The dropdown is an ARIA listbox with a labelled search input and full keyboard
  operation. Long names ellipsize in the trigger (full value via `title`) and
  wrap in the menu.

  Data is read from the live store slot (`importStore.inputs[role]`), not the
  `slot` prop: the prop can be a stale object across an import/replace, and the
  candidate status/array updated on the store did not reach a stale prop (the
  menu stayed on "Searching profiles…"). `role` is stable, so the derived slot is
  always the authoritative one.
-->
<script lang="ts">
  import { importStore } from "../lib/import.svelte";
  import type { InputSlot } from "../lib/import.svelte";
  import type { LensCandidate } from "../lib/types";
  import Icon from "./Icon.svelte";
  import ProvenanceTag from "./ProvenanceTag.svelte";

  let { slot }: { slot: InputSlot } = $props();

  // The authoritative slot: index the live store by the stable role.
  const role = $derived(slot.role);
  const live = $derived(importStore.inputs[role]);
  // The candidate catalog is a top-level store record so the dropdown re-renders
  // when the async response lands (see `LensCatalog`).
  const catalog = $derived(importStore.lensCatalog[role]);

  let open = $state(false);
  let query = $state("");
  let brand = $state<string | null>(null);

  const currentLabel = $derived(
    live.lens.value === null
      ? "Auto-detect"
      : `${live.lens.value.camera} ${live.lens.value.lens}`,
  );

  const brands = $derived(
    Array.from(new Set(catalog.candidates.map((c) => c.camera.split(" ")[0]))).sort(),
  );

  /**
   * Whether `candidate` is the slot's current selection (IN-02).
   *
   * The selected value arrives in a `lens_override_applied` payload and the
   * candidate in a separate `lens_candidates` payload, so object identity
   * (`live.lens.value === candidate`) is always false. Compare stable fields.
   */
  function isSelected(candidate: LensCandidate): boolean {
    const v = live.lens.value;
    return (
      v !== null &&
      v.camera === candidate.camera &&
      v.lens === candidate.lens &&
      v.width === candidate.width &&
      v.height === candidate.height
    );
  }

  const filtered = $derived(
    catalog.candidates.filter((c) => {
      if (brand !== null && !c.camera.startsWith(brand)) return false;
      if (query.length > 0) {
        const hay = `${c.camera} ${c.lens}`.toLowerCase();
        if (!hay.includes(query.toLowerCase())) return false;
      }
      return true;
    }),
  );

  function toggle(): void {
    open = !open;
    // Retry whenever the list is not already loaded for this slot. The request
    // is one-shot per open only while it stays "idle": a response that never
    // lands (or a request made while the worker was mid-import) would otherwise
    // wedge the menu on "Searching profiles…" forever, since reopening never
    // asked again.
    if (open && !(catalog.status === "ready" && catalog.candidates.length > 0)) {
      void importStore.requestLensCandidates(role);
    }
  }

  // Explicit visibility deriveds instead of an `{#if}` chain: the structural
  // block failed to re-run when the candidate status changed asynchronously
  // (the menu stayed on "Searching profiles…"), while derived values bound to
  // attributes update reliably.
  const showEmpty = $derived(
    catalog.status === "ready" && catalog.candidates.length === 0,
  );
  const showNoMatch = $derived(
    catalog.status === "ready" &&
      catalog.candidates.length > 0 &&
      filtered.length === 0,
  );
  const showList = $derived(
    catalog.status === "ready" && filtered.length > 0,
  );

  function close(): void {
    open = false;
    query = "";
    brand = null;
  }

  function select(candidate: LensCandidate): void {
    void importStore.setLensOverride(role, candidate);
    close();
  }

  function useAutoDetect(): void {
    void importStore.clearLensOverride(role);
    close();
  }

  function loadFile(): void {
    void importStore.loadProfile();
    close();
  }

  function handleKeydown(e: KeyboardEvent): void {
    if (!open) return;
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  }
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="lens-picker">
  <div class="lens-row">
    <span class="lens-label">Lens profile</span>
    <button
      type="button"
      class="lens-trigger"
      aria-haspopup="listbox"
      aria-expanded={open}
      title={currentLabel}
      onclick={toggle}
    >
      <span class="lens-value">{currentLabel}</span>
      {#if live.lens.tag !== null}
        <ProvenanceTag provenance={live.lens.tag} />
      {/if}
      <Icon name="chevron-down" />
    </button>
  </div>

  {#if open}
    <div class="lens-menu">
      <input
        class="lens-search"
        type="search"
        placeholder="Search profiles…"
        aria-label="Search lens profiles"
        bind:value={query}
      />
      <div class="brand-chips" role="group" aria-label="Browse by brand">
        <button
          type="button"
          class="chip"
          class:active={brand === null}
          onclick={() => (brand = null)}
        >
          All
        </button>
        {#each brands as b (b)}
          <button
            type="button"
            class="chip"
            class:active={brand === b}
            onclick={() => (brand = b)}
          >
            {b}
          </button>
        {/each}
      </div>

      <p class="menu-note" hidden={catalog.status !== "loading"}>
        Searching profiles…
      </p>
      <p class="menu-note error" hidden={catalog.status !== "error"}>
        Couldn't load lens profiles: {catalog.error ?? "unknown error"}. Search is
        local — try Load profile file… below.
      </p>
      <p class="menu-note" hidden={!showEmpty}>
        No matching profiles found — the engine will auto-detect.
      </p>
      <p class="menu-note" hidden={!showNoMatch}>No profiles match "{query}".</p>
      <ul
        class="candidate-list"
        role="listbox"
        aria-label="Lens profiles"
        hidden={!showList}
      >
        {#each filtered as candidate (candidate.camera + candidate.lens + candidate.width)}
          <li>
            <button
              type="button"
              class="candidate"
              role="option"
              aria-selected={isSelected(candidate)}
              title={`${candidate.camera} ${candidate.lens}`}
              onclick={() => select(candidate)}
            >
              <span class="candidate-name"
                >{candidate.camera} {candidate.lens}</span
              >
              <span class="candidate-res"
                >{candidate.width}×{candidate.height}</span
              >
            </button>
          </li>
        {/each}
      </ul>

      <button type="button" class="menu-action" onclick={loadFile}>
        <Icon name="folder-open" /> Load profile file…
      </button>
      {#if live.lens.value !== null}
        <button type="button" class="menu-action" onclick={useAutoDetect}>
          Use auto-detect
        </button>
      {/if}
    </div>
  {/if}
</div>

<style>
  .lens-picker {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .lens-row {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
    min-height: 28px;
  }

  .lens-label {
    color: var(--color-log-info);
    font-size: var(--text-body);
    flex: 0 0 96px;
  }

  .lens-trigger {
    flex: 1 1 auto;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: var(--color-dominant);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    cursor: pointer;
  }

  .lens-value {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    text-align: left;
  }

  .lens-menu {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    padding: var(--space-sm);
    border: 1px solid var(--color-accent);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
  }

  .lens-search {
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: var(--color-dominant);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
  }

  .lens-search:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .brand-chips {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-xs);
  }

  .chip {
    padding: 0 var(--space-xs);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-log-info);
    font-family: var(--font-ui);
    font-size: 12px;
    cursor: pointer;
  }

  .chip.active {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .candidate-list {
    margin: 0;
    padding: 0;
    list-style: none;
    max-height: 200px;
    overflow-y: auto;
  }

  .candidate {
    width: 100%;
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-sm);
    padding: var(--space-xs) var(--space-sm);
    border: none;
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    text-align: left;
    cursor: pointer;
  }

  .candidate:hover {
    background: var(--color-dominant);
  }

  .candidate[aria-selected="true"] {
    background: var(--color-dominant);
    color: var(--color-accent);
  }

  .candidate-name {
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .candidate-res {
    flex: 0 0 auto;
    font-family: var(--font-mono);
    color: var(--color-log-info);
  }

  .menu-note {
    margin: 0;
    padding: var(--space-xs) var(--space-sm);
    color: var(--color-log-info);
    font-size: 12px;
    overflow-wrap: anywhere;
  }

  .menu-note.error {
    color: var(--color-log-error);
  }

  .menu-action {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    padding: var(--space-xs) var(--space-sm);
    border: none;
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-accent);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    text-align: left;
    cursor: pointer;
  }

  .lens-trigger:focus-visible,
  .candidate:focus-visible,
  .menu-action:focus-visible,
  .chip:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }
</style>
