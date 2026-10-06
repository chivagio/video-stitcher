<!--
  Profile actions (IMPT-05/06 / UI-SPEC Profile Persistence Contract):
  `Load profile…` / `Save profile…` with an idle → in-flight (Loading…/Saving…)
  → success/error state machine. No action ever leaves the UI busy; failures
  render the typed error copy and re-enable the buttons. The native Save-As
  dialog is the overwrite guard (it prompts when the file already exists).
-->
<script lang="ts">
  import { importStore } from "../lib/import.svelte";
  import ActionButton from "./ActionButton.svelte";
  import Icon from "./Icon.svelte";

  let {
    onReveal,
  }: { onReveal?: (path: string) => void } = $props();

  const busy = $derived(
    importStore.profileStatus === "loading" || importStore.profileStatus === "saving",
  );
  const loadLabel = $derived(
    importStore.profileStatus === "loading" ? "Loading…" : "Load profile…",
  );
  const saveLabel = $derived(
    importStore.profileStatus === "saving" ? "Saving…" : "Save profile…",
  );
</script>

<div class="profile-actions">
  <ActionButton
    variant="secondary"
    disabled={busy}
    onClick={() => void importStore.loadProfile()}
  >
    <Icon name="folder-open" /> {loadLabel}
  </ActionButton>
  <ActionButton
    variant="secondary"
    disabled={busy}
    onClick={() => void importStore.saveProfile()}
  >
    <Icon name="save" /> {saveLabel}
  </ActionButton>
</div>

{#if importStore.profileStatus === "error" && importStore.profileError !== null}
  {#if importStore.profileOp === "save"}
    <p class="profile-line error">
      Couldn't save the profile: {importStore.profileError}. Choose another
      location.
    </p>
  {:else}
    <p class="profile-line error">
      Couldn't load the profile: {importStore.profileError}. Choose a valid
      calibration .json file.
    </p>
  {/if}
{:else if importStore.lastSavedPath !== null}
  <p class="profile-line success">
    Saved profile to <span class="profile-path">{importStore.lastSavedPath}</span>.
    {#if onReveal}
      <button
        type="button"
        class="reveal"
        onclick={() => onReveal?.(importStore.lastSavedPath ?? "")}
      >
        Reveal in folder
      </button>
    {/if}
  </p>
{:else if importStore.profilePath !== null}
  <p class="profile-line success">
    Loaded profile from
    <span class="profile-path">{importStore.profilePath}</span>.
  </p>
{/if}

<style>
  .profile-actions {
    display: flex;
    gap: var(--space-sm);
  }

  .profile-line {
    margin: var(--space-sm) 0 0;
    font-size: var(--text-body);
    overflow-wrap: anywhere;
  }

  .profile-line.success {
    color: var(--color-success);
  }

  .profile-line.error {
    color: var(--color-log-error);
  }

  .profile-path {
    font-family: var(--font-mono);
  }

  .reveal {
    margin-left: var(--space-xs);
    padding: 0;
    border: none;
    background: transparent;
    color: var(--color-accent);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    text-decoration: underline;
    cursor: pointer;
  }
</style>
