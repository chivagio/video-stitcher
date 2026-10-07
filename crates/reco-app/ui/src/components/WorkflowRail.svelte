<!--
  Workflow rail (D3-01 / UI-SPEC): a persistent top `<nav aria-label="Workflow">`
  with the three sequential steps (Import · Calibrate · Preview) and a global
  Log toggle. `aria-current="step"` marks the active screen. The rail is the
  single router; the operator can revisit Import at any time.

  Step enablement (UI-SPEC Interaction & State Contract):
    Import    always
    Calibrate both clips valid OR a profile is loaded
    Preview   a valid calibration result exists
  A disabled step carries its reason as a `title` (never a silently dead control).
-->
<script lang="ts">
  import type { Screen } from "../lib/types";

  let {
    active,
    onNavigate,
    logExpanded = false,
    onToggleLog,
    calibrateEnabled = false,
    previewEnabled = false,
    exportEnabled = false,
    exportRunning = false,
    calibrateReason = "Select two clips to calibrate.",
    previewReason = "Calibrate first.",
    exportReason = "Calibrate first.",
    onOpenProject,
    onSaveProject,
  }: {
    active: Screen;
    onNavigate: (screen: Screen) => void;
    logExpanded?: boolean;
    onToggleLog: () => void;
    calibrateEnabled?: boolean;
    previewEnabled?: boolean;
    exportEnabled?: boolean;
    /**
     * Whether an export is in flight. The UI-SPEC modal-export contract disables
     * the rail (steps, project actions, System, Log) so the progress state owns
     * the screen and the operator cannot navigate away mid-run.
     */
    exportRunning?: boolean;
    calibrateReason?: string;
    previewReason?: string;
    exportReason?: string;
    onOpenProject: () => void;
    onSaveProject: () => void;
  } = $props();

  const steps: { id: Screen; label: string }[] = [
    { id: "import", label: "Import" },
    { id: "calibrate", label: "Calibrate" },
    { id: "preview", label: "Preview" },
    { id: "export", label: "Export" },
  ];

  function isEnabled(id: Screen): boolean {
    if (exportRunning) return false;
    if (id === "import") return true;
    if (id === "calibrate") return calibrateEnabled;
    if (id === "preview") return previewEnabled;
    return exportEnabled;
  }

  function reasonFor(id: Screen): string {
    if (exportRunning) return "Export in progress.";
    if (id === "calibrate" && !calibrateEnabled) return calibrateReason;
    if (id === "preview" && !previewEnabled) return previewReason;
    if (id === "export" && !exportEnabled) return exportReason;
    return "";
  }
</script>

<nav class="workflow-rail" aria-label="Workflow">
  <div class="steps">
    {#each steps as step}
      <button
        type="button"
        class="step"
        class:active={active === step.id}
        aria-current={active === step.id ? "step" : undefined}
        disabled={!isEnabled(step.id)}
        title={reasonFor(step.id) || step.label}
        onclick={() => onNavigate(step.id)}
      >
        {step.label}
      </button>
    {/each}
  </div>
  <div class="rail-actions">
    <div class="project-actions" role="group" aria-label="Project">
      <button
        type="button"
        class="project-action"
        title={exportRunning ? "Export in progress." : "Open a .reco project"}
        disabled={exportRunning}
        onclick={onOpenProject}
      >
        Open project
      </button>
      <button
        type="button"
        class="project-action"
        title={exportRunning ? "Export in progress." : "Save the current work as a .reco project"}
        disabled={exportRunning}
        onclick={onSaveProject}
      >
        Save project
      </button>
    </div>
    <button
      type="button"
      class="system-toggle"
      class:active={active === "system"}
      aria-current={active === "system" ? "page" : undefined}
      title={exportRunning ? "Export in progress." : "System info and logs"}
      disabled={exportRunning}
      onclick={() => onNavigate("system")}
    >
      System
    </button>
    <button
      type="button"
      class="log-toggle"
      aria-pressed={logExpanded}
      title={exportRunning ? "Export in progress." : "Toggle the event log"}
      disabled={exportRunning}
      onclick={onToggleLog}
    >
      Log
    </button>
  </div>
</nav>

<style>
  .workflow-rail {
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    height: var(--workflow-rail-height);
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 var(--space-md);
    background: var(--color-dominant);
    z-index: 20;
  }

  .steps {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
    height: 100%;
  }

  .step {
    height: 100%;
    padding: 0 var(--space-sm);
    border: none;
    border-bottom: 2px solid transparent;
    background: transparent;
    color: var(--color-log-info);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-semibold);
    cursor: pointer;
  }

  .step.active {
    color: var(--color-accent);
    border-bottom-color: var(--color-accent);
  }

  .step:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* While an export runs the whole rail is disabled (modal-export contract). */
  .project-action:disabled,
  .system-toggle:disabled,
  .log-toggle:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .log-toggle {
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid transparent;
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    cursor: pointer;
  }

  .rail-actions {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
  }

  .project-actions {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
  }

  .project-action {
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid transparent;
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    cursor: pointer;
  }

  .project-action:hover:not(:disabled) {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .system-toggle {
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid transparent;
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    cursor: pointer;
  }

  .system-toggle.active {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .log-toggle[aria-pressed="true"] {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .step:focus-visible,
  .system-toggle:focus-visible,
  .project-action:focus-visible,
  .log-toggle:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  @media (prefers-reduced-motion: reduce) {
    .step,
    .system-toggle,
    .project-action,
    .log-toggle {
      transition: none;
    }
  }
</style>
