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
    calibrateReason = "Select two clips to calibrate.",
    previewReason = "Calibrate first.",
  }: {
    active: Screen;
    onNavigate: (screen: Screen) => void;
    logExpanded?: boolean;
    onToggleLog: () => void;
    calibrateEnabled?: boolean;
    previewEnabled?: boolean;
    calibrateReason?: string;
    previewReason?: string;
  } = $props();

  const steps: { id: Screen; label: string }[] = [
    { id: "import", label: "Import" },
    { id: "calibrate", label: "Calibrate" },
    { id: "preview", label: "Preview" },
  ];

  function isEnabled(id: Screen): boolean {
    if (id === "import") return true;
    if (id === "calibrate") return calibrateEnabled;
    return previewEnabled;
  }

  function reasonFor(id: Screen): string {
    if (id === "calibrate" && !calibrateEnabled) return calibrateReason;
    if (id === "preview" && !previewEnabled) return previewReason;
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
  <button
    type="button"
    class="log-toggle"
    aria-pressed={logExpanded}
    onclick={onToggleLog}
  >
    Log
  </button>
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

  .log-toggle[aria-pressed="true"] {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .step:focus-visible,
  .log-toggle:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  @media (prefers-reduced-motion: reduce) {
    .step,
    .log-toggle {
      transition: none;
    }
  }
</style>
