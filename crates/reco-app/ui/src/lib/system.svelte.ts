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
 * It also drives the one-click diagnostics bundle (DIAG-03): `exportBundle`
 * opens a native save dialog and posts `export_diagnostics_bundle`; the worker
 * writes the redacted, local-only zip and emits `diagnostics_bundle_written`,
 * whose path this store mirrors. No network path exists.
 *
 * Pure UI (D-06): it only sends worker commands over Tauri IPC and renders
 * worker events. It never touches engine types or window lifecycle.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";
import type { LogRecord, PreflightReport, SystemInfoView, WorkerEventTyped } from "./types";
import { WORKER_EVENT_TYPED } from "./types";
import { formatWorkerError } from "./errors";

/** Extension offered in the diagnostics-bundle save dialog (DIAG-03). */
const BUNDLE_EXTENSIONS = ["zip"];

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

  /** The path of the last-written diagnostics bundle, or null (DIAG-03). */
  bundlePath = $state<string | null>(null);
  /** Whether a diagnostics-bundle export is in flight (DIAG-03). */
  bundleExporting = $state(false);
  /** The typed error text when a bundle export fails (DIAG-03). */
  bundleError = $state<string | null>(null);

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
      case "diagnostics_bundle_written": {
        // The worker emits this only after the atomic write succeeded, so the
        // path names a real, redacted, local bundle.
        this.bundlePath = event.data.path;
        this.bundleError = null;
        this.bundleExporting = false;
        break;
      }
      case "failed": {
        // The generic failure carries no operation tag; only claim it when a
        // bundle export is actually in flight (mirrors the project store's
        // in-flight routing).
        if (this.bundleExporting) {
          this.bundleError = formatWorkerError(event.data);
          this.bundleExporting = false;
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

  /**
   * Export the one-click, redacted, local-only diagnostics bundle (DIAG-03).
   *
   * Opens the native save dialog for a `.zip`, then posts
   * `export_diagnostics_bundle` with the chosen path. The worker gathers the
   * retained logs, system info, and calibration artifacts, redacts the home
   * path/account name, and writes a single local zip atomically — nothing
   * touches the network. The success path is the typed
   * `diagnostics_bundle_written` event, which stores the path; a cancelled
   * dialog is a no-op, and a failure surfaces the typed text.
   */
  async exportBundle(): Promise<void> {
    this.bundleError = null;
    try {
      const selected = await save({
        defaultPath: "reco-diagnostics.zip",
        filters: [{ name: "Diagnostics bundle", extensions: BUNDLE_EXTENSIONS }],
      });
      // `null` means the operator cancelled the dialog — a no-op.
      if (typeof selected !== "string") return;
      this.bundleExporting = true;
      await invoke("export_diagnostics_bundle", { path: selected });
    } catch (e) {
      this.bundleExporting = false;
      this.bundleError = formatWorkerError(e);
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
