<!--
  Relocate dialog (PROJ-01, UI-SPEC Project relocate). Shown when opening a
  `.reco` project whose referenced inputs are missing: a focus-trapped
  `role="dialog"` `aria-modal` listing each missing input as a row
  (`<role> — <path>`) with a **Relocate** picker and a **Skip** action.

  Closing without relocating leaves the app unchanged (the open never partially
  restores). Relocating one input re-runs the restore; the dialog closes only
  when the worker reports the project opened (or the operator skips).
-->
<script lang="ts">
  import type { InputRole, MissingInput } from "../lib/types";
  import ActionButton from "./ActionButton.svelte";

  let {
    missing,
    onRelocate,
    onSkip,
  }: {
    missing: MissingInput[];
    onRelocate: (role: InputRole) => void;
    onSkip: () => void;
  } = $props();

  let dialogEl = $state<HTMLDivElement | null>(null);
  let skipEl = $state<HTMLButtonElement | null>(null);

  // Focus the least-destructive action on open (UI-SPEC Accessibility).
  $effect(() => {
    if (missing.length > 0) {
      skipEl?.focus();
    }
  });

  function roleLabel(role: InputRole): string {
    return role === "left" ? "Camera A (left)" : "Camera B (right)";
  }

  function focusables(): HTMLElement[] {
    if (!dialogEl) return [];
    return Array.from(
      dialogEl.querySelectorAll<HTMLElement>(
        'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
      ),
    ).filter((el) => !el.hasAttribute("disabled"));
  }

  function handleKeydown(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      e.preventDefault();
      onSkip();
      return;
    }
    if (e.key === "Tab") {
      // Focus trap: keep Tab inside the dialog (UI-SPEC Accessibility).
      const els = focusables();
      if (els.length === 0) return;
      const first = els[0];
      const last = els[els.length - 1];
      const active = document.activeElement as HTMLElement | null;
      if (e.shiftKey && (active === first || !dialogEl?.contains(active))) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && active === last) {
        e.preventDefault();
        first.focus();
      }
    }
  }
</script>

{#if missing.length > 0}
  <div class="dialog-backdrop" role="presentation">
    <div
      bind:this={dialogEl}
      class="dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="relocate-dialog-heading"
      tabindex="-1"
      onkeydown={handleKeydown}
    >
      <h2 id="relocate-dialog-heading" class="dialog-heading">
        Some project inputs are missing
      </h2>
      <p class="dialog-body">
        The project references files that could not be found. Relocate each input
        to restore the project, or skip to leave the app unchanged.
      </p>
      <ul class="missing-list">
        {#each missing as item (item.role)}
          <li class="missing-row">
            <span class="missing-label">
              Missing: {roleLabel(item.role)} — <span class="missing-path"
                >{item.path}</span
              >
            </span>
            <ActionButton
              variant="secondary"
              ariaLabel="Relocate {roleLabel(item.role)}"
              onClick={() => onRelocate(item.role)}
            >
              Relocate
            </ActionButton>
          </li>
        {/each}
      </ul>
      <div class="dialog-actions">
        <button bind:this={skipEl} type="button" class="dialog-cancel" onclick={onSkip}>
          Skip
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .dialog-backdrop {
    position: fixed;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgba(0, 0, 0, 0.55);
    z-index: 40;
  }

  .dialog {
    max-width: 480px;
    width: calc(100% - var(--space-xl));
    padding: var(--space-lg);
    background: var(--color-secondary);
    border-radius: var(--space-xs);
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.5);
  }

  .dialog-heading {
    margin: 0;
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
    overflow-wrap: anywhere;
  }

  .dialog-body {
    margin: var(--space-sm) 0 0;
    color: var(--color-log-info);
    overflow-wrap: anywhere;
  }

  .missing-list {
    margin: var(--space-md) 0 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
    max-height: 40vh;
    overflow-y: auto;
  }

  .missing-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-sm);
    padding: var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
  }

  .missing-label {
    color: var(--color-body-text);
    font-size: var(--text-body);
    overflow-wrap: anywhere;
  }

  .missing-path {
    font-family: var(--font-mono);
    color: var(--color-log-info);
    overflow-wrap: anywhere;
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-sm);
    margin-top: var(--space-lg);
  }

  .dialog-cancel {
    min-height: 36px;
    padding: 8px 16px;
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-semibold);
    cursor: pointer;
  }

  .dialog-cancel:hover {
    filter: brightness(1.1);
  }

  .dialog-cancel:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  @media (prefers-reduced-motion: reduce) {
    .dialog-cancel {
      transition: none;
    }
  }
</style>
