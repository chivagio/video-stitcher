/**
 * Transport rune store (PREV-02).
 *
 * Wraps the typed WorkerCommand protocol for playback: play / pause / seek /
 * step_frame / set_loop. Listens to `worker-event` and parses the projected
 * log lines to update the authoritative transport state.
 *
 * The worker is authoritative for playback position and pose (UI-SPEC
 * Interaction rule 1). The frontend mirrors the worker's value on every
 * Position/Transport event; while dragging the timeline the UI renders the
 * drag position immediately (affordance) and sends seek intents.
 *
 * Pure UI (CONTEXT D-02/D-06): this store only sends worker commands over
 * Tauri IPC and renders worker events. It never touches window lifecycle,
 * raw handles, or any engine type.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { LogLine, TransportState } from "./types";
import { WORKER_EVENT } from "./types";

/** The transport's authoritative state. */
export type TransportStatus = "empty" | "loading" | "ready" | "playing" | "ended";

/**
 * Parse a position log message, with or without the frame-rate rational:
 *   "position: frame X/Y"      "position: frame X"
 *   "position: frame X/Y @ n/d" "position: frame X @ n/d"
 *
 * Returns `[frame, total, fps]`; `total` and `fps` are null when unknown. The
 * rational is what lets the timecode be exact instead of assuming 30 fps.
 */
function parsePosition(
  message: string,
): [number, number | null, number | null] | null {
  const m = message.match(
    /^position: frame (\d+)(?:\/(\d+))?(?: @ (\d+)\/(\d+))?$/,
  );
  if (!m) return null;
  const frame = Number(m[1]);
  const total = m[2] !== undefined ? Number(m[2]) : null;
  // Guard a zero/negative denominator rather than producing Infinity.
  const den = m[4] !== undefined ? Number(m[4]) : NaN;
  const fps = m[3] !== undefined && den > 0 ? Number(m[3]) / den : null;
  return [frame, total, fps];
}

/**
 * Parse a "transport: <State>, loop on|off" log message.
 * Returns `[state, loopEnabled]` or null when the message is not a transport line.
 */
function parseTransport(message: string): [TransportState, boolean] | null {
  const m = message.match(/^transport: (Playing|Paused|Ended), loop (on|off)$/);
  if (!m) return null;
  const state = m[1].toLowerCase() as TransportState;
  const loop = m[2] === "on";
  return [state, loop];
}

/**
 * The transport rune store.
 *
 * A class with `$state` fields — Svelte 5 rune store pattern. Components
 * import and use it directly; the state is shared across all consumers.
 */
class TransportStore {
  /** Current transport status (empty = no session yet). */
  status = $state<TransportStatus>("empty");
  /** Current frame index (0-based). */
  frame = $state(0);
  /** Total frames in the source, if known. */
  total = $state<number | null>(null);
  /** Whether full-clip looping is enabled. */
  loop = $state(false);
  /** Frame rate in fps, if known. */
  fps = $state<number | null>(null);
  /** Whether a seek is in flight (for UI affordance). */
  seeking = $state(false);

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
    const pos = parsePosition(line.message);
    if (pos !== null) {
      const [frame, total, fps] = pos;
      this.frame = frame;
      if (total !== null) this.total = total;
      if (fps !== null) this.fps = fps;
      this.seeking = false;
      // A position event implies the session is active.
      if (this.status === "empty" || this.status === "loading") {
        this.status = "ready";
      }
      return;
    }

    const tr = parseTransport(line.message);
    if (tr !== null) {
      const [state, loop] = tr;
      this.loop = loop;
      if (state === "playing") {
        this.status = "playing";
      } else if (state === "paused") {
        if (this.status === "playing" || this.status === "empty" || this.status === "loading") {
          this.status = "ready";
        }
      } else if (state === "ended") {
        this.status = "ended";
      }
      return;
    }
  }

  /** Begin/continue playing. */
  async play(): Promise<void> {
    if (this.status === "empty" || this.status === "loading") return;
    this.status = "playing";
    try {
      await invoke("play");
    } catch {
      // The worker will emit a Transport event to correct the state.
    }
  }

  /** Pause at the current position. */
  async pause(): Promise<void> {
    if (this.status !== "playing") return;
    this.status = "ready";
    try {
      await invoke("pause");
    } catch {
      // The worker will emit a Transport event to correct the state.
    }
  }

  /** Seek to an absolute frame index. */
  async seek(frame: number): Promise<void> {
    if (this.status === "empty" || this.status === "loading") return;
    this.seeking = true;
    this.frame = frame;
    try {
      await invoke("seek", { frame });
    } catch {
      this.seeking = false;
    }
  }

  /** Step exactly ±1 frame. */
  async step(direction: 1 | -1): Promise<void> {
    if (this.status === "empty" || this.status === "loading") return;
    try {
      await invoke("step_frame", { direction });
    } catch {
      // The worker will emit a Position event to correct the state.
    }
  }

  /** Enable/disable full-clip looping. */
  async setLoop(enabled: boolean): Promise<void> {
    if (this.status === "empty" || this.status === "loading") return;
    this.loop = enabled;
    try {
      await invoke("set_loop", { enabled });
    } catch {
      // The worker will emit a Transport event to correct the state.
    }
  }

  /**
   * Format a frame index as a timecode string (H:MM:SS).
   *
   * Falls back to a nominal 30 fps only when the source reported no rational.
   * A real rational arrives on every position event, so non-30 material is
   * converted exactly rather than drifting.
   */
  formatTimecode(frame: number): string {
    const fps = this.fps ?? 30;
    const totalSeconds = Math.floor(frame / fps);
    const hours = Math.floor(totalSeconds / 3600);
    const minutes = Math.floor((totalSeconds % 3600) / 60);
    const seconds = totalSeconds % 60;
    if (hours > 0) {
      return `${hours}:${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
    }
    return `${minutes}:${String(seconds).padStart(2, "0")}`;
  }

  /** The duration timecode string, or "0:00" when unknown. */
  get durationTimecode(): string {
    if (this.total === null) return "0:00";
    return this.formatTimecode(this.total);
  }

  /** The current timecode string. */
  get currentTimecode(): string {
    return this.formatTimecode(this.frame);
  }

  /** Whether the transport has an active session. */
  get hasSession(): boolean {
    return this.status !== "empty" && this.status !== "loading";
  }

  /** Whether the transport controls should be enabled. */
  get controlsEnabled(): boolean {
    return this.status === "ready" || this.status === "playing" || this.status === "ended";
  }
}

/** The shared transport store instance. */
export const transport = new TransportStore();
