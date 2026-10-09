/**
 * Presenter rune store (PREV-05).
 *
 * Wraps the typed WorkerCommand protocol for presenter selection: badge,
 * manual override, readback channel attach, and "Show preview window".
 * Mirrors the worker's authoritative presenter state.
 *
 * The presenter is selected at startup by a capability probe and can be
 * overridden in the controls panel. The active one is always visible via
 * the badge. A fallback is never silent: a WARN line carries the reason and
 * remediation.
 *
 * Pure UI (CONTEXT D-02/D-06): this store only sends worker commands over
 * Tauri IPC and renders worker events.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Channel } from "@tauri-apps/api/core";
import type { LogLine, PresenterKind } from "./types";
import { WORKER_EVENT } from "./types";
import { log } from "./log.svelte";

/**
 * Record a rejected presenter IPC call on the event log.
 *
 * The old code was `catch {}` with the comment "The worker will emit a Presenter
 * event to correct the state". That premise only holds when the command is
 * *accepted*: when the invoke itself fails (unknown command, argument
 * deserialization, managed-state extraction, a closed channel) the worker never
 * sees it and no event is ever emitted. Swallowing the rejection there turned
 * every one of those into an indistinguishable "the button does nothing" — which
 * is exactly what the inert Windows Preview screen reported, with no log line on
 * either surface.
 *
 * The log store is the event-log contract's surface (UI-SPEC Event Log), so a
 * rejected command lands where the operator already reads. `console.error`
 * mirrors it to devtools. Returns the message so the caller can also expose it.
 */
function reportIpcFailure(action: string, error: unknown): string {
  const reason = error instanceof Error ? error.message : String(error);
  const message = `${action} failed: ${reason}`;
  log.append({ level: "error", message });
  console.error(`[presenter] ${message}`);
  return message;
}

/** The presenter's authoritative state. */
export interface PresenterState {
  /** Which presenter in the chain is active. */
  kind: PresenterKind;
  /** Why the active presenter was chosen, when it was a fallback. */
  reason: string | null;
}

/**
 * Parse a "presenter: <Kind>" log message.
 * Returns the presenter kind or null when the message is not a presenter line.
 */
function parsePresenter(message: string): PresenterKind | null {
  const m = message.match(/^presenter: (Native|Separate window|Readback)$/);
  if (!m) return null;
  switch (m[1]) {
    case "Native":
      return "native";
    case "Separate window":
      return "separate_window";
    case "Readback":
      return "readback";
    default:
      return null;
  }
}

/**
 * Parse a "Presenter fallback to <Kind>: <reason>. <remediation>." log message.
 * Returns `[kind, reason]` or null when the message is not a fallback line.
 */
function parseFallback(message: string): [PresenterKind, string] | null {
  const m = message.match(
    /^Presenter fallback to (Separate window|Readback): (.+?)\. .+$/,
  );
  if (!m) return null;
  const kind: PresenterKind = m[1] === "Separate window" ? "separate_window" : "readback";
  return [kind, m[2]];
}

/** The presenter rune store. */
class PresenterStore {
  /** The active presenter kind. */
  kind = $state<PresenterKind>("native");
  /** The fallback reason, when the activation was a fallback. */
  reason = $state<string | null>(null);
  /** Whether the readback presenter is active (for the degraded banner). */
  get isReadback(): boolean {
    return this.kind === "readback";
  }
  /** Whether the separate-window presenter is active (for the placeholder). */
  get isSeparateWindow(): boolean {
    return this.kind === "separate_window";
  }
  /**
   * The last presenter IPC failure, recorded so the UI is never inert without a
   * reason. `null` once the last action was accepted. */
  lastError = $state<string | null>(null);

  /** Unlisten function for the worker-event listener. */
  #unlisten: (() => void) | null = null;

  /** Start listening to worker events. */
  async init(): Promise<void> {
    if (this.#unlisten !== null) return;
    this.#unlisten = await listen<LogLine>(WORKER_EVENT, (event) => {
      this.#onEvent(event.payload);
    });
  }

  /** Stop listening to worker events. */
  destroy(): void {
    if (this.#unlisten !== null) {
      this.#unlisten();
      this.#unlisten = null;
    }
  }

  /** Handle a worker event (projected to a LogLine). */
  #onEvent(line: LogLine): void {
    const kind = parsePresenter(line.message);
    if (kind !== null) {
      this.kind = kind;
      this.reason = null;
      return;
    }

    const fallback = parseFallback(line.message);
    if (fallback !== null) {
      const [kind, reason] = fallback;
      this.kind = kind;
      this.reason = reason;
    }
  }

  /** Request a presenter override. */
  async setPresenter(kind: PresenterKind): Promise<void> {
    try {
      await invoke("set_presenter", { kind });
      this.lastError = null;
    } catch (error) {
      // A rejected invoke means the worker never saw the command, so no
      // Presenter event will arrive to correct the state. Report it rather than
      // leaving the badge silently wrong.
      this.lastError = reportIpcFailure(`presenter override to ${kind}`, error);
    }
  }

  /** Attach the webview readback channel to the readback presenter. */
  async attachReadback(onFrame: Channel<ArrayBuffer>): Promise<void> {
    try {
      await invoke("preview_attach_readback", { onFrame });
      this.lastError = null;
    } catch (error) {
      // The readback channel is what carries frames, so a rejected attach is a
      // silent black canvas otherwise.
      this.lastError = reportIpcFailure("readback channel attach", error);
    }
  }

  /** Show the separate preview window. */
  async showPreviewWindow(): Promise<void> {
    try {
      await invoke("show_preview_window");
      this.lastError = null;
    } catch (error) {
      // The action's whole observable contract is a window appearing; a
      // rejection must be visible, not swallowed.
      this.lastError = reportIpcFailure("show preview window", error);
    }
  }

  /** The locked badge label for the active presenter. */
  get badgeLabel(): string {
    switch (this.kind) {
      case "native":
        return "Presenter: Native";
      case "separate_window":
        return "Presenter: Separate window";
      case "readback":
        return "Presenter: Readback";
    }
  }
}

/** The shared presenter store instance. */
export const presenter = new PresenterStore();
