<!--
  Lens profile picker (IMPT-04 / D3-07): a searchable dropdown per slot,
  populated from the engine's resolution-aware candidates, with a brand→model
  browse and a `Load profile file…` path. Default is engine auto-detect; picking
  a profile tags the row `overridden` (UI-SPEC Lens Profile Contract).

  The dropdown is an ARIA listbox with a labelled search input and full keyboard
  operation. Long names ellipsize in the trigger (full value via `title`) and
  wrap in the menu.
-->
<script lang="ts">
  import type { InputSlot } from "../lib/import.svelte";
  import { importStore } from "../lib/import.svelte";
  import type { LensCandidate } from "../lib/types";
  import Icon from "./Icon.svelte";
  import ProvenanceTag from "./ProvenanceTag.svelte";

  let { slot }: { slot: InputSlot } = $props();

  let open = $state(false);
  let query = $state("");
  let brand = $state<string | null>(null);

  const currentLabel = $derived(
    slot.lens.value === null
      ? "Auto-detect"
      : `${slot.lens.value.camera} ${slot.lens.value.lens}`,
  );

  const brands = $derived(
    Array.from(
      new Set(slot.candidates.map((c) => c.camera.split(" ")[0])),
    ).sort(),
  );

  const filtered = $derived(
    slot.candidates.filter((c) => {
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
    if (open && slot.candidatesStatus === "idle") {
      void importStore.requestLensCandidates(slot.role);
    }
  }

  function close(): void {
    open = false;
    query = "";
    brand = null;
  }

  function select(candidate: LensCandidate): void {
    void importStore.setLensOverride(slot.role, candidate);
    close();
  }

  function useAutoDetect(): void {
    void importStore.clearLensOverride(slot.role);
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
      {#if slot.lens.tag !== null}
        <ProvenanceTag provenance={slot.lens.tag} />
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

      {#if slot.candidatesStatus === "loading"}
        <p class="menu-note">Searching profiles…</p>
      {:else if slot.candidatesStatus === "error"}
        <p class="menu-note error">
          Couldn't load lens profiles: {slot.candidatesError ?? "unknown error"}.
          Search is local — try Load profile file… below.
        </p>
      {:else if slot.candidates.length === 0}
        <p class="menu-note">
          No matching profiles found — the engine will auto-detect.
        </p>
      {:else if filtered.length === 0}
        <p class="menu-note">No profiles match "{query}".</p>
      {:else}
        <ul class="candidate-list" role="listbox" aria-label="Lens profiles">
          {#each filtered as candidate (candidate.camera + candidate.lens + candidate.width)}
            <li>
              <button
                type="button"
                class="candidate"
                role="option"
                aria-selected={slot.lens.value === candidate}
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
      {/if}

      <button type="button" class="menu-action" onclick={loadFile}>
        <Icon name="folder-open" /> Load profile file…
      </button>
      {#if slot.lens.value !== null}
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
