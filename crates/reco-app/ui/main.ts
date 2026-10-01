// Phase 2 probe frontend entry (UI-SPEC Interaction + Event Log contracts).
//
// Pure UI (CONTEXT D-02/D-06/D-09): this module only sends worker commands
// over Tauri IPC and renders worker events. It never touches window lifecycle,
// raw handles, or any engine type — all engine interplay goes through the
// worker's command channel, and events arrive as typed `WorkerEvent`s already
// projected by Rust into `{ level, message }` (see `crates/reco-app/src/events.rs`).
//
// NOTE: this is the minimal probe-era shell (task 02-01). The real transport
// bar, timeline, controls panel, log drawer and presenter badge are built in
// plans 02-04/02-05. Here we keep the Import / Start preview / Export buttons
// so the z-order/compositing probe can drive the engine end to end, and we
// forward worker events to the console/log so the probe can assert on them.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** Event name the Rust bridge emits under (`main.rs::install_event_bridge`). */
const WORKER_EVENT = "worker-event";

/** The three UI-SPEC log levels. */
type Level = "info" | "warn" | "error";

/** The flat shape the Rust bridge emits (`events::LogLine`). */
interface LogLine {
  level: Level;
  message: string;
}

/** A control button and the gerund label it shows while its command is busy. */
interface Control {
  readonly id: "import" | "preview" | "export";
  readonly label: string;
  readonly busyLabel: string;
}

const CONTROLS: readonly Control[] = [
  { id: "import", label: "Import", busyLabel: "Importing…" },
  { id: "preview", label: "Start preview", busyLabel: "Starting preview…" },
  { id: "export", label: "Export", busyLabel: "Exporting…" },
];

const buttons = new Map<Control["id"], HTMLButtonElement>();

function requireElement<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (el === null) {
    throw new Error(`reco-app UI: required element #${id} is missing`);
  }
  return el as T;
}

/**
 * Append one level-coloured console line: `[LEVEL] message`.
 *
 * The message is taken verbatim from the worker payload; JS never invents
 * error text (UI-SPEC Error state). The DOM log drawer is plan 02-05; the probe
 * asserts on the process log, so a console projection is what it needs here.
 */
function appendLine(line: LogLine): void {
  const prefix = `[${line.level.toUpperCase()}]`;
  if (line.level === "error") {
    console.error(prefix, line.message);
  } else if (line.level === "warn") {
    console.warn(prefix, line.message);
  } else {
    console.info(prefix, line.message);
  }
}

/** The control whose command is currently in flight, if any. */
let busy: Control["id"] | null = null;

/**
 * Enter the busy state for `control`: show its gerund label and disable every
 * button (UI-SPEC Interaction Contract — overlapping commands cannot be
 * issued).
 */
function setBusy(control: Control["id"]): void {
  busy = control;
  for (const c of CONTROLS) {
    const button = buttons.get(c.id);
    if (button === undefined) continue;
    button.disabled = true;
    button.textContent = c.id === control ? c.busyLabel : c.label;
    button.classList.toggle("is-active", c.id === control);
  }
}

/** Return the row to its prior (idle) state. */
function setIdle(): void {
  busy = null;
  for (const c of CONTROLS) {
    const button = buttons.get(c.id);
    if (button === undefined) continue;
    button.disabled = false;
    button.textContent = c.label;
    button.classList.remove("is-active");
  }
}

async function runCommand(control: Control): Promise<void> {
  if (busy !== null) return; // overlapping commands are impossible from the UI
  setBusy(control.id);
  try {
    await invoke(control.id);
  } catch (error) {
    setIdle();
    appendLine({ level: "error", message: stringifyError(error) });
  }
}

/** Coerce an IPC rejection into the typed error's message text. */
function stringifyError(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return String(error);
}

function wireControls(): void {
  for (const control of CONTROLS) {
    const button = requireElement<HTMLButtonElement>(control.id);
    buttons.set(control.id, button);
    button.addEventListener("click", () => {
      void runCommand(control);
    });
  }
}

async function wireEvents(): Promise<void> {
  await listen<LogLine>(WORKER_EVENT, (event) => {
    const line = event.payload;
    appendLine(line);
    // A worker event is the authoritative signal that the in-flight command
    // reached a terminal state; return the row to idle.
    if (busy !== null && isTerminal(line)) {
      setIdle();
    }
  });
}

/**
 * Whether a worker line marks the end of an in-flight command.
 *
 * Phase 1/2 skeleton: the worker's `preview stopped`, `import finished`, and
 * `export finished` lines are the terminal signals its backend emits after each
 * job (see `worker.rs`). Anything else leaves the busy row in place.
 */
function isTerminal(line: LogLine): boolean {
  if (line.level === "error") return true;
  const m = line.message;
  return (
    m === "import finished" ||
    m === "export finished" ||
    m === "preview stopped"
  );
}

function main(): void {
  wireControls();
  void wireEvents().catch((error: unknown) => {
    appendLine({
      level: "error",
      message: `event listener failed: ${stringifyError(error)}`,
    });
  });
}

main();
