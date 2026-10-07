/**
 * System Info + structured-log rune store (DIAG-01 / DIAG-02 / DIAG-05).
 *
 * Wraps the typed `WorkerCommand`/`WorkerEvent` protocol for the System screen.
 * The worker is authoritative: this store mirrors the typed `SystemInfo`,
 * `Preflight`, and `LogRecord` events and never probes a GPU, encoder, or
 * prerequisite itself (UI-SPEC Interaction rule 1).
 *
 * The structured records come from the worker's tracing layer as typed events;
 * this store renders them verbatim and never regex-parses log text (DIAG-02
 * prohibition). The record list is capped at the carried 2000-line bound
 * (T-05-12).
 *
 * Pure UI (D-06): it only sends worker commands over Tauri IPC and renders
 * worker events. It never touches engine types or window lifecycle.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { LogRecord, PreflightReport, SystemInfoView, WorkerEventTyped } from "./types";
import { WORKER_EVENT_TYPED } from "./types";
import { formatWorkerError } from "./errors";

/** Maximum number of structured records retained in the DOM (carried bound). */
export const MAX_LOG_RECORDS = 2000;

/** The level filter options (word + colour are the UI's, never colour alone). */
export type LogLevelFilter = "all" | "info" | "warn" | "error";

/**
 * The System screen's rune store (class with `$state` fields, shared).
 */
class SystemStore {
  /** The probed system view, or null before the worker reports. */
  info = $state<SystemInfoView | null>(null);
  /** The runtime preflight report, or null before the worker reports. */
  preflight = $state<PreflightReport | null>(null);
  /** The structured engine log records, newest last (capped). */
  records = $state<LogRecord[]>([]);
  /** Whether a probe/refresh is in flight (the "reading…" state). */
  loading = $state(false);
  /** The typed error text when a refresh command is rejected. */
  error = $state<string | null>(null);
  /** The LogViewer's level filter. */
  levelFilter = $state<LogLevelFilter>("all");

  /** Unlisten function for the typed worker-event listener. */
  #unlisten: (() => void) | null = null;

  /** Start listening for typed worker events. */
  async init(): Promise<void> {
    if (this.#unlisten !== null) return;
    this.#unlisten = await listen<WorkerEventTyped>(WORKER_EVENT_TYPED, (event) => {
      this.#onEvent(event.payload);
    });
  }

  /** Stop listening. */
  destroy(): void {
    if (this.#unlisten !== null) {
      this.#unlisten();
      this.#unlisten = null;
    }
  }

  /** Apply a typed worker event to the store state (worker-authoritative). */
  #onEvent(event: WorkerEventTyped): void {
    switch (event.kind) {
      case "system_info": {
        this.info = event.data.info;
        break;
      }
      case "preflight": {
        this.preflight = event.data.report;
        break;
      }
      case "log_record": {
        this.records = [...this.records, event.data.record];
        if (this.records.length > MAX_LOG_RECORDS) {
          this.records = this.records.slice(-MAX_LOG_RECORDS);
        }
        break;
      }
      default:
        break;
    }
  }

  /**
   * Ask the worker to probe the system and run the preflight (DIAG-01/05).
   *
   * Both commands are posted; the typed events arrive asynchronously and
   * populate the store. A rejected invoke surfaces the typed text.
   */
  async refresh(): Promise<void> {
    this.loading = true;
    this.error = null;
    try {
      await invoke("system_info");
      await invoke("run_preflight");
    } catch (e) {
      this.error = formatWorkerError(e);
    } finally {
      this.loading = false;
    }
  }

  /** Set the LogViewer's level filter. */
  setLevelFilter(filter: LogLevelFilter): void {
    this.levelFilter = filter;
  }

  /** The records matching the current level filter. */
  get filteredRecords(): LogRecord[] {
    const filter = this.levelFilter;
    if (filter === "all") return this.records;
    return this.records.filter((record) => record.level === filter);
  }

  /** Whether the worker has reported any system info yet. */
  get hasInfo(): boolean {
    return this.info !== null;
  }
}

/** The shared system store instance. */
export const systemStore = new SystemStore();
