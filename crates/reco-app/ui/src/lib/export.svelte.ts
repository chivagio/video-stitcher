/**
 * Export rune store (EXPT-01 / EXPT-02 / EXPT-04).
 *
 * Wraps the typed `WorkerCommand`/`WorkerEvent` protocol for the modal Export
 * screen. The worker is authoritative for the encoder list, the per-frame
 * progress, the resolved encoder, and the final path (UI-SPEC Interaction rule
 * 1): this store mirrors the typed `EncoderList` / `ExportProgress` /
 * `ExportFinished` / `ExportCancelled` / `ExportFailed` / `ExportFallback`
 * events and **never derives progress or a path locally**.
 *
 * Cancel (EXPT-04) sets the shared `Arc<AtomicBool>` via the direct
 * `cancel_export` command; the worker responds with a typed `ExportCancelled`.
 *
 * Pure UI (D-06): this store only sends worker commands over Tauri IPC and
 * renders worker events. It never touches engine types or window lifecycle.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  EncoderView,
  ExportPreset,
  ExportSettings,
  ExportVariant,
  InputMetadata,
  WorkerEventTyped,
} from "./types";
import { WORKER_EVENT_TYPED } from "./types";
import { formatWorkerError } from "./errors";

/** The Export screen's state machine (UI-SPEC Interaction & State Contract). */
export type ExportStatus = "idle" | "running" | "done" | "cancelled" | "failed";

/** The locked preset labels (UI-SPEC Copywriting Contract). */
export const PRESET_LABELS: Record<ExportPreset, string> = {
  source_match: "Source match",
  p1080: "1080p",
  p4k: "4K",
  web: "Web",
  custom: "Custom",
};

/** The locked variant labels (UI-SPEC Copywriting Contract). */
export const VARIANT_LABELS: Record<ExportVariant, string> = {
  panorama: "Panorama",
  side_by_side: "Side-by-side",
  stacked: "Stacked",
};

/** The deterministic filename suffix per variant (mirror `ExportVariant::suffix`). */
export const VARIANT_SUFFIX: Record<ExportVariant, string> = {
  panorama: "_panorama",
  side_by_side: "_sbs",
  stacked: "_stacked",
};

/** One preset's derived parameters (EXPT-01). */
interface PresetParams {
  width: number;
  height: number;
  codec: string;
  quality: string;
  /** Whether the resolution follows the probed source resolution. */
  sourceMatch: boolean;
}

/**
 * The built-in preset parameter table (CONTEXT "Exact built-in preset parameter
 * values" is agent discretion; these are the shipped defaults).
 */
export const PRESET_PARAMS: Record<ExportPreset, PresetParams> = {
  source_match: {
    width: 1920,
    height: 1080,
    codec: "h264",
    quality: "high",
    sourceMatch: true,
  },
  p1080: { width: 1920, height: 1080, codec: "h264", quality: "balanced", sourceMatch: false },
  p4k: { width: 3840, height: 2160, codec: "h264", quality: "balanced", sourceMatch: false },
  web: { width: 1920, height: 1080, codec: "h264", quality: "fast", sourceMatch: false },
  custom: { width: 1920, height: 1080, codec: "h264", quality: "balanced", sourceMatch: false },
};

/** Parse a `"1920×1080"`-style resolution string into `[w, h]`, or `null`. */
export function parseResolution(value: string | null): [number, number] | null {
  if (!value) return null;
  const match = value.match(/(\d+)\s*[×x]\s*(\d+)/);
  if (!match) return null;
  const width = Number(match[1]);
  const height = Number(match[2]);
  if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) {
    return null;
  }
  return [width, height];
}

/** The mutable fields a caller may override when deriving settings. */
export type ExportOverrides = Partial<
  Pick<
    ExportSettings,
    "encoder_name" | "variant" | "output_dir" | "start_frame" | "end_frame" | "width" | "height" | "codec" | "quality"
  >
>;

/**
 * Derive the typed settings from a preset and the probed input metadata
 * (EXPT-01). Pure and unit-testable: the preset's parameters are derived here,
 * never invented in a component.
 *
 * "Source match" follows the probed input resolution when it is available and
 * parseable; otherwise it falls back to the preset's default 1920×1080.
 */
export function deriveSettings(
  preset: ExportPreset,
  inputMeta: InputMetadata | null,
  overrides: ExportOverrides = {},
): ExportSettings {
  const params = PRESET_PARAMS[preset];
  let { width, height } = params;
  if (params.sourceMatch) {
    const parsed = parseResolution(inputMeta?.resolution.value ?? null);
    if (parsed) {
      [width, height] = parsed;
    }
  }
  return {
    preset,
    width,
    height,
    codec: params.codec,
    quality: params.quality,
    bitrate_kbps: null,
    encoder_name: null,
    start_frame: null,
    end_frame: null,
    variant: "panorama",
    output_dir: null,
    ...overrides,
  };
}

/** A monospace-safe filename preview (never the worker-resolved absolute path). */
export function outputNamePreview(
  inputPath: string | null,
  variant: ExportVariant,
): string {
  const base = inputPath ? inputPath.split(/[\\/]/).pop() ?? inputPath : "clip.mp4";
  const stem = base.replace(/\.[^.]+$/, "") || "clip";
  return `${stem}${VARIANT_SUFFIX[variant]}.mp4`;
}

/** Format a millisecond duration as `M:SS` (bounded; never `H:MM:SS`). */
export function formatDuration(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
}

/** The typed progress the store mirrors (EXPT-04). */
export interface ExportProgressView {
  percent: number;
  framesCompleted: number;
  total: number | null;
  elapsedMs: number;
  etaMs: number | null;
}

/** The typed completion the store mirrors (EXPT-04). */
export interface ExportResultView {
  path: string;
  encoder: string;
  hardware: boolean;
  variant: ExportVariant;
}

/**
 * The export rune store (class with `$state` fields, shared across components).
 */
class ExportStore {
  /** The chosen preset. */
  preset = $state<ExportPreset>("source_match");
  /** The typed settings sent to the worker. */
  settings = $state<ExportSettings>(deriveSettings("source_match", null));
  /** The probed encoders, hardware first (from typed `EncoderList`). */
  encoders = $state<EncoderView[]>([]);
  /** The auto-selected encoder (the list head), or null before a probe. */
  auto = $state<EncoderView | null>(null);
  /** Whether the auto encoder is hardware-accelerated, or null before a probe. */
  autoHardware = $state<boolean | null>(null);
  /** The typed fallback pair when an override was unavailable (EXPT-02). */
  fallback = $state<{
    requested: string;
    used: string;
    used_hardware: boolean;
  } | null>(null);
  /** The live per-frame progress, or null outside a run. */
  progress = $state<ExportProgressView | null>(null);
  /** The screen state machine. */
  status = $state<ExportStatus>("idle");
  /** The completed run's result, or null. */
  result = $state<ExportResultView | null>(null);
  /** The typed failure text when `status === "failed"`. */
  error = $state<string | null>(null);
  /** Whether an encoder probe is in flight. */
  probing = $state(false);
  /**
   * The worker-resolved output path preview (EXPT-06).
   *
   * The worker owns the directory + stem + variant + collision suffix; this is
   * its typed `ExportPathPreview` value rendered verbatim. The webview never
   * constructs an absolute path itself.
   */
  pathPreview = $state<{ path: string } | null>(null);
  /** Whether a path-preview request is in flight. */
  pathPreviewing = $state(false);

  /** The probed left-input metadata, for the "Source match" derivation. */
  #inputMeta: InputMetadata | null = null;
  /** Unlisten function for the typed worker-event listener. */
  #unlisten: (() => void) | null = null;
  /** The debounce timer for the worker path preview. */
  #previewTimer: ReturnType<typeof setTimeout> | null = null;

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
      case "encoder_list": {
        const { encoders, auto, auto_hardware } = event.data;
        this.encoders = encoders;
        this.auto = auto;
        this.autoHardware = auto_hardware;
        break;
      }
      case "export_fallback": {
        // EXPT-02: the explicit, never-silent fallback. The banner renders the
        // locked copy from the typed pair (including the actual encoder class).
        this.fallback = event.data;
        break;
      }
      case "export_progress": {
        const { frames_completed, total, elapsed_ms, eta_ms, percent } = event.data;
        this.progress = {
          percent,
          framesCompleted: frames_completed,
          total,
          elapsedMs: elapsed_ms,
          etaMs: eta_ms,
        };
        break;
      }
      case "export_finished": {
        const { path, encoder, hardware, variant } = event.data;
        this.result = { path, encoder, hardware, variant };
        this.status = "done";
        this.progress = null;
        this.error = null;
        break;
      }
      case "export_cancelled": {
        this.status = "cancelled";
        this.progress = null;
        this.result = null;
        break;
      }
      case "export_failed": {
        this.error = event.data.message;
        this.status = "failed";
        this.progress = null;
        this.result = null;
        break;
      }
      case "export_path_preview": {
        // EXPT-06: the worker-resolved path, rendered verbatim.
        this.pathPreview = { path: event.data.path };
        break;
      }
      default:
        break;
    }
  }

  /** Seed the "Source match" derivation from the probed left-input metadata. */
  setInputMetadata(meta: InputMetadata | null): void {
    this.#inputMeta = meta;
    this.settings = deriveSettings(this.preset, meta, {
      encoder_name: this.settings.encoder_name,
      variant: this.settings.variant,
      output_dir: this.settings.output_dir,
      start_frame: this.settings.start_frame,
      end_frame: this.settings.end_frame,
    });
    this.schedulePathPreview();
  }

  /** Choose a preset and re-derive its parameters (EXPT-01). */
  setPreset(preset: ExportPreset): void {
    this.preset = preset;
    this.settings = deriveSettings(preset, this.#inputMeta, {
      encoder_name: this.settings.encoder_name,
      variant: this.settings.variant,
      output_dir: this.settings.output_dir,
      start_frame: this.settings.start_frame,
      end_frame: this.settings.end_frame,
    });
    this.schedulePathPreview();
  }

  /** Set the encoder override, or `null` for Auto (EXPT-02). */
  setEncoderOverride(name: string | null): void {
    this.settings = { ...this.settings, encoder_name: name };
  }

  /** Choose the output variant (EXPT-05). */
  setVariant(variant: ExportVariant): void {
    this.settings = { ...this.settings, variant };
    this.schedulePathPreview();
  }

  /** Set the output directory, or `null` for the default. */
  setOutputDir(dir: string | null): void {
    this.settings = { ...this.settings, output_dir: dir };
    this.schedulePathPreview();
  }

  /**
   * Set the trim window (EXPT-03).
   *
   * `null` on a side means "the clip edge" — the same representation the
   * worker's `ExportSettings` uses, so a full clip is `(null, null)`.
   */
  setTrim(inFrame: number | null, outFrame: number | null): void {
    this.settings = { ...this.settings, start_frame: inFrame, end_frame: outFrame };
  }

  /**
   * A monospace trim summary for the export form (EXPT-03).
   *
   * The worker clamps an invalid window at export time; this is the operator's
   * requested window, shown so the form and the timeline agree.
   */
  deriveTrimSummary(): string {
    const { start_frame, end_frame } = this.settings;
    if (start_frame === null && end_frame === null) return "Full clip";
    const start = start_frame === null ? "start" : `${start_frame}`;
    const end = end_frame === null ? "end" : `${end_frame}`;
    return `${start} → ${end}`;
  }

  /**
   * Debounce a worker-side path preview for the current settings (EXPT-06).
   *
   * A preset/variant/output-dir change re-arms this window; one preview fires
   * once the window elapses, so a burst of changes does not spam the worker.
   */
  schedulePathPreview(): void {
    if (this.#previewTimer !== null) clearTimeout(this.#previewTimer);
    this.#previewTimer = setTimeout(() => {
      this.#previewTimer = null;
      void this.previewPath();
    }, 200);
  }

  /**
   * Resolve the deterministic output path via the worker (EXPT-06).
   *
   * The worker owns the path; this only asks it to resolve and emit the typed
   * `ExportPathPreview`. A preview failure is non-fatal: the form keeps its last
   * path rather than surfacing a modal error.
   */
  async previewPath(): Promise<void> {
    this.pathPreviewing = true;
    try {
      await invoke("preview_export_path", { settings: this.settings });
    } catch {
      // Non-fatal: keep the last resolved path.
    } finally {
      this.pathPreviewing = false;
    }
  }

  /** Probe the available encoders for the current codec (EXPT-02). */
  async probeEncoders(): Promise<void> {
    this.probing = true;
    this.error = null;
    try {
      await invoke("probe_encoders", { codec: this.settings.codec });
    } catch (e) {
      this.error = formatWorkerError(e);
    } finally {
      this.probing = false;
    }
  }

  /** Start the export with the current typed settings (EXPT-01). */
  async startExport(): Promise<void> {
    this.progress = null;
    this.result = null;
    this.error = null;
    this.fallback = null;
    this.status = "running";
    try {
      await invoke("export_with", { settings: this.settings });
    } catch (e) {
      // The command boundary rejected the request; surface the typed text.
      this.error = formatWorkerError(e);
      this.status = "failed";
    }
  }

  /**
   * Request cancellation of the running export (EXPT-04).
   *
   * Flips the shared cancel flag; the worker responds with a typed
   * `ExportCancelled`. `cancel_export` never fails, so a rejected invoke is
   * swallowed and the run stays cancelling until the worker's event arrives.
   */
  async cancelExport(): Promise<void> {
    if (this.status !== "running") return;
    try {
      await invoke("cancel_export");
    } catch {
      // Never fails; the typed event remains the terminal signal.
    }
  }

  /** Reset the screen back to the ready form. */
  reset(): void {
    this.status = "idle";
    this.progress = null;
    this.result = null;
    this.error = null;
    this.fallback = null;
  }

  /** Whether hardware is unavailable and software will run (EXPT-02). */
  get softwareFallback(): boolean {
    if (this.fallback !== null) return true;
    if (this.result !== null) return !this.result.hardware;
    return this.autoHardware === false;
  }

  /** The encoder name shown in the fallback banner (resolved or auto). */
  get fallbackEncoderName(): string {
    if (this.fallback !== null) return this.fallback.used;
    if (this.result !== null) return this.result.encoder;
    return this.auto?.name ?? "software";
  }

  /**
   * The fallback banner copy (EXPT-02).
   *
   * Keeps the locked `<reason> — exporting with <class> encoding (<encoder>).`
   * structure, but reports the class of the encoder that will ACTUALLY run: an
   * unavailable override falls back to the auto pick, which may itself be
   * hardware, so the class is never hardcoded to software.
   */
  get fallbackMessage(): string {
    const classWord =
      this.fallback?.used_hardware === true ? "hardware" : "software";
    const reason =
      this.fallback !== null && this.fallback.used_hardware
        ? `${this.fallback.requested} unavailable`
        : "Hardware encoding unavailable";
    return `${reason} — exporting with ${classWord} encoding (${this.fallbackEncoderName}).`;
  }

  /**
   * Whether the worker's resolved preview path carries a collision suffix
   * (EXPT-06).
   *
   * True when the basename does not end with the plain `<suffix>.mp4` pattern,
   * i.e. the worker appended `_1`/`_2` to avoid overwriting an existing file.
   * The form announces this rather than silently overwriting.
   */
  get pathPreviewCollision(): boolean {
    const path = this.pathPreview?.path;
    if (!path) return false;
    const base = path.split(/[\\/]/).pop() ?? path;
    return !base.endsWith(`${VARIANT_SUFFIX[this.settings.variant]}.mp4`);
  }

  /** The monospace progress line (UI-SPEC Copywriting Contract). */
  get progressText(): string {
    const p = this.progress;
    if (p === null) return "";
    const pct = Math.round(p.percent);
    const frames =
      p.total === null ? `${p.framesCompleted} frames` : `${p.framesCompleted}/${p.total} frames`;
    const eta = p.etaMs === null ? "Not reported" : formatDuration(p.etaMs);
    return `${pct}% · ${frames} · ${formatDuration(p.elapsedMs)} · ETA ${eta}`;
  }
}

/** The shared export store instance. */
export const exportStore = new ExportStore();
