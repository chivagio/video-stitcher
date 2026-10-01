// Phase 1 walking-skeleton frontend entry (UI-SPEC Interaction + Event Log
// contracts).
//
// Pure UI (CONTEXT D-02/D-06/D-09): this module only sends worker commands
// over Tauri IPC and renders worker events. It never touches window lifecycle,
// raw handles, or any engine type — all engine interplay goes through the
// worker's command channel, and events arrive as typed `WorkerEvent`s already
// projected by Rust into `{ level, message }` (see `crates/reco-app/src/events.rs`).

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** Event name the Rust bridge emits under (`main.rs::install_event_bridge`). */
const WORKER_EVENT = "worker-event";

/** Maximum number of DOM log lines retained; oldest are dropped beyond this. */
const MAX_LOG_LINES = 2000;

/** The three UI-SPEC log levels and their CSS modifier suffixes. */
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

const logPane = requireElement<HTMLDivElement>("log");
const logEmpty = requireElement<HTMLDivElement>("log-empty");
const logLines = requireElement<HTMLDivElement>("log-lines");

const buttons = new Map<Control["id"], HTMLButtonElement>();

function requireElement<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (el === null) {
    throw new Error(`reco-app UI: required element #${id} is missing`);
  }
  return el as T;
}

// ---------------------------------------------------------------- event log

/** Whether the log view is pinned to the newest line. */
let followTail = true;

/**
 * Append one level-coloured line: `[HH:MM:SS] LEVEL  message`.
 *
 * The message is taken verbatim from the worker payload; JS never invents
 * error text (UI-SPEC Error state). Level text carries the severity colour,
 * the message stays body grey.
 */
function appendLine(line: LogLine): void {
  const level: Level = line.level;
  const row = document.createElement("div");
  row.className = `log-line log-line--${level}`;

  const levelSpan = document.createElement("span");
  levelSpan.className = "log-level";
  levelSpan.textContent = `${formatTime(new Date())} ${level.toUpperCase()}`;

  const text = document.createElement("span");
  text.className = "log-message";
  // textContent, not innerHTML: long messages wrap and nothing is interpreted
  // as markup.
  text.textContent = `  ${line.message}`;

  row.append(levelSpan, text);
  logLines.append(row);

  // Reveal the populated view and hide the empty state.
  if (logEmpty.hidden === false) {
    logEmpty.hidden = true;
  }

  // Bound the DOM: drop the oldest lines beyond the cap to bound memory.
  while (logLines.childElementCount > MAX_LOG_LINES) {
    logLines.firstElementChild?.remove();
  }

  // Auto-scroll to newest unless the user has scrolled up (followTail false).
  if (followTail) {
    logPane.scrollTop = logPane.scrollHeight;
  }
}

/** Format a Date as zero-padded `HH:MM:SS` in local time. */
function formatTime(now: Date): string {
  const pad = (n: number): string => String(n).padStart(2, "0");
  return `[${pad(now.getHours())}:${pad(now.getMinutes())}:${pad(now.getSeconds())}]`;
}

/**
 * Pause auto-scroll when the user scrolls away from the bottom; resume when
 * they return (UI-SPEC Event Log Contract: prevents yanking during an export).
 */
function trackScroll(): void {
  const atBottom =
    logPane.scrollTop + logPane.clientHeight >= logPane.scrollHeight - 4;
  followTail = atBottom;
}

// ------------------------------------------------------------- control row

/** The control whose command is currently in flight, if any. */
let busy: Control["id"] | null = null;

/**
 * Enter the busy state for `control`: show its gerund label and disable every
 * button (UI-SPEC Interaction Contract — this is how the single-owner worker
 * boundary is proven visually: overlapping commands cannot be issued).
 *
 * "Start preview" stays in the running state until the worker reports
 * completion/failure; the busy flag is cleared by the worker's terminal event,
 * not by an optimistic local timer.
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

/**
 * Return the row to its prior (idle) state: all buttons enabled with their
 * resting labels. Called when a command is rejected or when the worker reports
 * the command finished.
 */
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

/** Render the typed worker error with the UI-SPEC Error-state copy. */
function renderFailure(line: LogLine): void {
  // The message is the worker's typed error text; JS adds only the fixed
  // framing copy from the UI-SPEC Error state (never invented per-error text).
  const operation = busy ?? "command";
  const message =
    `${operation} failed: ${line.message}. Check the log below, then try again.`;
  appendLine({ level: "error", message });
}

// -------------------------------------------------------------------- wiring

async function runCommand(control: Control): Promise<void> {
  if (busy !== null) return; // overlapping commands are impossible from the UI
  setBusy(control.id);
  try {
    await invoke(control.id);
  } catch (error) {
    // The worker channel is closed, or the command was rejected before the
    // worker could post an event. Return the row to idle and surface the typed
    // error text verbatim.
    setIdle();
    renderFailure({ level: "error", message: stringifyError(error) });
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
    if (line.level === "error") {
      // ERROR lines use the Error-state copy; the worker's message is rendered
      // verbatim inside it.
      renderFailure(line);
    } else {
      appendLine(line);
    }
    // A worker event is the authoritative signal that the in-flight command
    // reached a terminal state; return the row to idle. (Preview lines such as
    // "preview started" also arrive mid-run, so only clear on terminal-ish
    // events: keep it simple for the skeleton — the row is re-enabled after
    // any event that is not itself a busy transition. The worker emits
    // "…finished"/"preview stopped" at the end of each job.)
    if (busy !== null && isTerminal(line)) {
      setIdle();
    }
  });
}

/**
 * Whether a worker line marks the end of an in-flight command.
 *
 * Phase 1 keeps this deliberately small: the worker's `preview stopped`,
 * `import finished`, and `export finished` lines are the terminal signals its
 * backend emits after each job (see `worker.rs`). Anything else leaves the busy
 * row in place so "Start preview" stays disabled while the live loop runs.
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
  logPane.addEventListener("scroll", trackScroll, { passive: true });
  // The empty state is visible until the first line arrives.
  void wireEvents().catch((error: unknown) => {
    appendLine({
      level: "error",
      message: `event listener failed: ${stringifyError(error)}`,
    });
  });
}

main();
