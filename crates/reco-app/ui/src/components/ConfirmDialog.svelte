<!--
  Shared confirmation dialog (UI-SPEC Component Inventory): `role="dialog"
  aria-modal="true"`, labelled by its heading, focus-trapped, initial focus on
  the least-destructive action, `Escape` cancels, max-width 420px. Reserved for
  destructive confirmations only (cancel calibration, profile overwrite).
-->
<script lang="ts">
  import type { Snippet } from "svelte";
  import ActionButton from "./ActionButton.svelte";

  let {
    open,
    heading,
    body,
    confirmLabel,
    cancelLabel,
    destructive = false,
    onConfirm,
    onCancel,
    children,
  }: {
    open: boolean;
    heading: string;
    body?: string;
    confirmLabel: string;
    cancelLabel: string;
    destructive?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
    children?: Snippet;
  } = $props();

  let dialogEl = $state<HTMLDivElement | null>(null);
  let cancelEl = $state<HTMLButtonElement | null>(null);

  // Initial focus on the least-destructive action, and restore focus when the
  // dialog closes (UI-SPEC Accessibility: ConfirmDialog contract).
  $effect(() => {
    if (open) {
      cancelEl?.focus();
    }
  });

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
      onCancel();
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

{#if open}
  <div class="dialog-backdrop" role="presentation">
    <div
      bind:this={dialogEl}
      class="dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="confirm-dialog-heading"
      tabindex="-1"
      onkeydown={handleKeydown}
    >
      <h2 id="confirm-dialog-heading" class="dialog-heading">{heading}</h2>
      {#if body}
        <p class="dialog-body">{body}</p>
      {/if}
      {#if children}
        {@render children()}
      {/if}
      <div class="dialog-actions">
        <button
          bind:this={cancelEl}
          type="button"
          class="dialog-cancel"
          onclick={onCancel}
        >
          {cancelLabel}
        </button>
        <ActionButton
          variant={destructive ? "destructive" : "primary"}
          onClick={onConfirm}
        >
          {confirmLabel}
        </ActionButton>
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
    max-width: 420px;
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
