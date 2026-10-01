/**
 * Event-log rune store (UI-SPEC Event Log Contract).
 *
 * Appends worker event lines to a capped, auto-scrolling log drawer. The
 * DOM is capped at 2000 lines (oldest dropped) to bound memory. Auto-scroll
 * pauses while the user is scrolled up.
 *
 * The frontend renders the text verbatim and never invents error copy
 * (UI-SPEC Error state). The message is taken from the worker payload.
 *
 * Pure UI (CONTEXT D-02/D-06): this store only renders worker events.
 */

import { listen } from "@tauri-apps/api/event";
import type { LogLine, Level } from "./types";
import { WORKER_EVENT } from "./types";

/** Maximum number of log lines to keep in the DOM (UI-SPEC Event Log Contract). */
const MAX_LINES = 2000;

/** A single log line with a timestamp. */
export interface LogEntry {
  /** Local time as [HH:MM:SS]. */
  time: string;
  /** Severity level. */
  level: Level;
  /** Message text. */
  message: string;
}

/** Format a Date as [HH:MM:SS] local time. */
function formatTime(date: Date): string {
  const h = String(date.getHours()).padStart(2, "0");
  const m = String(date.getMinutes()).padStart(2, "0");
  const s = String(date.getSeconds()).padStart(2, "0");
  return `${h}:${m}:${s}`;
}

/** The log rune store. */
class LogStore {
  /** The log lines, newest last. */
  entries = $state<LogEntry[]>([]);
  /** Whether the drawer is expanded. */
  expanded = $state(false);
  /** Whether auto-scroll is paused (user scrolled up). */
  autoScrollPaused = $state(false);

  /** Unlisten function for the worker-event listener. */
  #unlisten: (() => void) | null = null;

  /** Start listening to worker events. */
  async init(): Promise<void> {
    if (this.#unlisten !== null) return;
    this.#unlisten = await listen<LogLine>(WORKER_EVENT, (event) => {
      this.append(event.payload);
    });
  }

  /** Stop listening to worker events. */
  destroy(): void {
    if (this.#unlisten !== null) {
      this.#unlisten();
      this.#unlisten = null;
    }
  }

  /** Append a log line. */
  append(line: LogLine): void {
    const entry: LogEntry = {
      time: formatTime(new Date()),
      level: line.level,
      message: line.message,
    };
    this.entries = [...this.entries, entry];
    // Cap at MAX_LINES, dropping the oldest.
    if (this.entries.length > MAX_LINES) {
      this.entries = this.entries.slice(-MAX_LINES);
    }
  }

  /** Toggle the drawer expanded state. */
  toggle(): void {
    this.expanded = !this.expanded;
  }

  /** Set the auto-scroll paused state. */
  setAutoScrollPaused(paused: boolean): void {
    this.autoScrollPaused = paused;
  }

  /** Whether the drawer has any lines. */
  get hasLines(): boolean {
    return this.entries.length > 0;
  }
}

/** The shared log store instance. */
export const log = new LogStore();
