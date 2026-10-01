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
    } catch {
      // The worker will emit a Presenter event to correct the state.
    }
  }

  /** Attach the webview readback channel to the readback presenter. */
  async attachReadback(onFrame: Channel<ArrayBuffer>): Promise<void> {
    try {
      await invoke("preview_attach_readback", { onFrame });
    } catch {
      // The worker will emit a Presenter event to correct the state.
    }
  }

  /** Show the separate preview window. */
  async showPreviewWindow(): Promise<void> {
    try {
      await invoke("show_preview_window");
    } catch {
      // The worker will emit a Presenter event to correct the state.
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
