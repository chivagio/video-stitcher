/**
 * Manual calibration rune store (MANU-01 / MANU-03).
 *
 * Wraps the typed `WorkerCommand`/`WorkerEvent` protocol for the manual
 * calibration flow. The worker is authoritative for the session, the retained
 * reference frame, and the rendered preview (UI-SPEC Interaction rule 1): this
 * store mirrors the typed `ManualSessionStarted` / `ManualValidationFrame` /
 * `ManualSolveState` events and **never derives them locally**.
 *
 * The preview/validation RGBA is streamed over a binary
 * `Channel<ArrayBuffer>` (the `manual_attach_preview` command), NOT the JSON
 * `worker-event-typed` bridge: a bounded frame is ~2 MB, which serializes to
 * ~8 MB of JSON numbers and made the webview parse millions of numbers per
 * frame (Phase 04.1 OOM). The bytes are the RGBA the worker produced by the GPU
 * undistort under real `CameraParams`; the frontend paints them on a canvas and
 * never touches the GPU (D-06 / T-04.1-04).
 *
 * Pure UI: this store only sends worker commands over Tauri IPC and renders
 * worker events. It never touches engine types or window lifecycle.
 */

import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  CameraParamsView,
  ManualPinView,
  ManualSide,
  PlaneLayoutView,
  SyncMethod,
  ValidationVerdict,
  WorkerEventTyped,
} from "./types";
import { WORKER_EVENT_TYPED } from "./types";
import { formatWorkerError } from "./errors";

/**
 * Handle travel clamps (MANU-05 / MANU-06), mirroring the worker's constants
 * (`crates/reco-app/src/worker.rs`). The UI clamps before posting so a drag can
 * never request an out-of-range value; the worker re-clamps and WARNs on the
 * boundary (T-04.1-13).
 */
/** First-order distortion coefficient travel (rim handle). */
export const K1_CLAMP = 0.3;
/** Fraction of the frame width/height a center-handle drag may travel. */
export const CENTER_CLAMP_FRAC = 0.1;
/** Fraction of the baseline focal length a scale-handle drag may travel. */
export const FX_CLAMP_FRAC = 0.15;
/** Layout `x_ty` travel. */
export const X_TY_CLAMP = 0.1;
/** Layout `x_rz` travel in radians. */
export const X_RZ_CLAMP = 0.3;
/** Layout `camera_axis_offset` range. */
export const CAM_D_RANGE: [number, number] = [0.1, 0.3];

/** The locked 5-step flow order (UI-SPEC Copywriting Contract). */
export type ManualStep = "time-align" | "frame" | "pin" | "bend" | "validate";

/**
 * The audio-sync confidence below which the estimate is treated as low
 * (MANU-02). Mirrors the backend's `events::AUDIO_SYNC_CONFIDENCE_FLOOR`
 * (itself the scorecard's Low band boundary, CALB-03): a low estimate renders
 * the same "set the offset manually" gate as an absent one, with the numeric
 * confidence still shown.
 */
export const AUDIO_CONFIDENCE_FLOOR = 0.5;

/** The step order, single source for the nav and index math. */
export const MANUAL_STEPS: ManualStep[] = [
  "time-align",
  "frame",
  "pin",
  "bend",
  "validate",
];

/**
 * One camera's rendered preview frame (MANU-03).
 *
 * The pixels arrive over the binary manual-frame channel; `rgba` is the
 * RGBA bytes (`width * height * 4`) the worker produced.
 */
export interface ManualPreview {
  /** RGBA bytes (`width * height * 4`). */
  rgba: Uint8ClampedArray;
  width: number;
  height: number;
}

/**
 * One stitched validation comparison (MANU-07). Carries the validation frame's
 * stitched RGBA plus the calibration frame's reference for the blink/blend
 * comparison. The residual/verdict/geometry arrive on the metadata-only
 * `manual_validation_frame` event; the two RGBA buffers arrive over the binary
 * manual-frame channel.
 */
export interface ValidationFrame {
  /** The validated frame index (0-based). */
  frame: number;
  /** The validation frame's stitched RGBA (`width * height * 4`). */
  rgba: Uint8ClampedArray;
  width: number;
  height: number;
  /** Per-frame residual (px). */
  residual: number;
  /** The advisory verdict (never a gate). */
  verdict: ValidationVerdict;
  /** The calibration frame's stitched RGBA for the blink comparison. */
  referenceRgba: Uint8ClampedArray;
  referenceWidth: number;
  referenceHeight: number;
}

/**
 * The binary manual-frame wire format (mirrors `worker::MANUAL_FRAME_HEADER_LEN`
 * and `worker::ManualFrameKind`): `[kind: u8][width: u32 LE][height: u32 LE][RGBA]`.
 * The length guard fails closed (no paint) if the two ever drift.
 */
const MANUAL_FRAME_HEADER_LEN = 9;
/** Manual-frame kind tags (mirror `worker::ManualFrameKind`). */
const MANUAL_KIND_LEFT = 0;
const MANUAL_KIND_RIGHT = 1;
const MANUAL_KIND_VALIDATION = 2;
const MANUAL_KIND_REFERENCE = 3;

/** One parsed binary manual frame. */
interface ManualBinaryFrame {
  kind: number;
  rgba: Uint8ClampedArray;
  width: number;
  height: number;
}

/**
 * Parse one binary manual frame, or `null` when it is malformed (short header,
 * zero dimension, or a payload length that does not match the header). Fails
 * closed so a bad frame cannot throw inside the uncaught Channel handler.
 */
function parseManualFrame(buffer: ArrayBuffer): ManualBinaryFrame | null {
  if (buffer.byteLength < MANUAL_FRAME_HEADER_LEN) return null;
  const view = new DataView(buffer);
  const kind = view.getUint8(0);
  const width = view.getUint32(1, true);
  const height = view.getUint32(5, true);
  if (width === 0 || height === 0) return null;
  if (buffer.byteLength !== MANUAL_FRAME_HEADER_LEN + width * height * 4) {
    return null;
  }
  const rgba = new Uint8ClampedArray(
    buffer,
    MANUAL_FRAME_HEADER_LEN,
    width * height * 4,
  );
  return { kind, rgba, width, height };
}

/**
 * The manual calibration rune store (class with `$state` fields, shared across
 * components).
 */
class ManualStore {
  /** Whether the manual flow surface is open. */
  open = $state(false);
  /** The active step in the guided flow. */
  step = $state<ManualStep>("time-align");
  /** The current reference frame index (worker-authoritative). */
  frame = $state(0);
  /** Total frames in the reference clip (from `manual_session_started`). */
  framesTotal = $state(0);
  /** The reference clip's frame rate (from `manual_session_started`). */
  fps = $state(0);
  /** The left camera's rendered preview, or null before the first frame. */
  previewLeft = $state<ManualPreview | null>(null);
  /** The right camera's rendered preview, or null before the first frame. */
  previewRight = $state<ManualPreview | null>(null);
  /** Whether a background solve is in flight. */
  solving = $state(false);
  /** Whether the preview shows the last solved (stale) result. */
  stale = $state(false);
  /**
   * Whether the last completed solve was rejected as degenerate (coincident or
   * collinear pins). Worker-authoritative: the editor drives its "spread the
   * pins" warning from this precise flag, never by inferring it from `stale`
   * (which also covers a debounce gap or a failed re-solve).
   */
  degenerate = $state(false);
  /** The last typed rejection, or null. */
  error = $state<string | null>(null);

  /** The current correspondence pins (worker-authoritative, MANU-03). */
  pins = $state<ManualPinView[]>([]);
  /** Whether the pins were seeded from verified automatic matches (MANU-04). */
  seeded = $state(false);
  /** The last landed solve result, or null (MANU-03). */
  solveResult = $state<{
    layout: PlaneLayoutView;
    residual: number;
    pins_used: number;
    auto_used: number;
  } | null>(null);

  /**
   * The edited real parameters (MANU-05 / MANU-06), or null before the first
   * `manual_params`. The handles seed from this typed payload; the store never
   * derives a parameter locally.
   */
  params = $state<{ left: CameraParamsView; right: CameraParamsView } | null>(
    null,
  );
  /**
   * The profile-baseline parameters captured on the first `manual_params` of a
   * session (MANU-05). The handles clamp against it so their travel range stays
   * anchored to the profile, not to the moving current value.
   */
  baselineParams = $state<{
    left: CameraParamsView;
    right: CameraParamsView;
  } | null>(null);
  /** The layout currently in effect (MANU-06), or null before the first event. */
  layout = $state<PlaneLayoutView | null>(null);
  /** The last background re-solve's layout delta, or null (MANU-05). */
  layoutDelta = $state<{
    cam_d: number;
    intersect: number;
    x_ty: number;
    x_rz: number;
  } | null>(null);
  /** The number of handle edits on the undo stack (drives the Undo control). */
  undoDepth = $state(0);

  /**
   * The validation frames received this session, keyed by frame index (MANU-07).
   * The worker is authoritative: this store mirrors the typed
   * `manual_validation_frame` payloads and never derives a residual locally.
   */
  validationFrames = $state<Record<number, ValidationFrame>>({});
  /** The last saved profile path, or null before a save (MANU-07). */
  savedPath = $state<string | null>(null);

  /**
   * The handle-edit undo stack (MANU-05). One entry per committed handle edit,
   * most recent last; `undoHandle` re-posts the prior value. A local stack is
   * sufficient because every entry round-trips through the worker's typed
   * `manual_params`.
   */
  #undoStack: Array<
    | { kind: "lens"; side: ManualSide; params: CameraParamsView }
    | { kind: "layout"; layout: PlaneLayoutView }
  > = [];

  /** The audio auto-sync confidence, or null when unavailable (MANU-02). */
  audioConfidence = $state<number | null>(null);
  /** The chosen temporal sync offset in frames (signed; 0 by default). */
  syncOffset = $state(0);
  /** Which sync path produced `syncOffset` (mirror `events::SyncMethod`). */
  syncMethod = $state<SyncMethod>("none");
  /** Whether the operator has confirmed the offset for this session. */
  syncConfirmed = $state(false);
  /**
   * The fixed offset-semantics sentence, bound from the worker's typed event
   * (`SYNC_OFFSET_SEMANTICS`). Never re-typed in a component (T-04.1-07).
   */
  offsetSemantics = $state("");

  /** Unlisten function for the typed worker-event listener. */
  #unlisten: (() => void) | null = null;

  /**
   * The binary manual-frame channel (MANU-03). Created and attached in
   * `begin()`; the pixels arrive here, never on the JSON event stream.
   */
  #frameChannel: Channel<ArrayBuffer> | null = null;

  /** The metadata of the in-flight validation frame, awaiting its buffers. */
  #pendingValidationMeta: {
    frame: number;
    residual: number;
    verdict: ValidationVerdict;
    referenceWidth: number;
    referenceHeight: number;
  } | null = null;
  /** The validation frame's stitched buffer, awaiting the reference/metadata. */
  #pendingValidation: {
    rgba: Uint8ClampedArray;
    width: number;
    height: number;
  } | null = null;
  /** The calibration reference buffer, awaiting the validation/metadata. */
  #pendingReference: {
    rgba: Uint8ClampedArray;
    width: number;
    height: number;
  } | null = null;

  /** Start listening for typed worker events (idempotent). */
  async init(): Promise<void> {
    if (this.#unlisten !== null) return;
    this.#unlisten = await listen<WorkerEventTyped>(
      WORKER_EVENT_TYPED,
      (event) => {
        this.#onEvent(event.payload);
      },
    );
  }

  /** Stop listening. */
  destroy(): void {
    if (this.#unlisten !== null) {
      this.#unlisten();
      this.#unlisten = null;
    }
    this.#detachFrameChannel();
  }

  /**
   * Create the binary manual-frame channel and attach it to the worker
   * (MANU-03). Idempotent within a session: a re-begin replaces the channel.
   *
   * The RGBA frames stream here instead of the JSON event bridge, so the
   * handler parses the `[kind][width][height][RGBA]` header and routes the
   * pixels by kind. Mirroring `PreviewSurface.svelte`'s readback path, a later
   * attach replaces the stored channel; cleanup only clears the JS handler.
   */
  async attachFrameChannel(): Promise<void> {
    this.#detachFrameChannel();
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = (buffer: ArrayBuffer) => {
      const frame = parseManualFrame(buffer);
      if (frame === null) return;
      switch (frame.kind) {
        case MANUAL_KIND_LEFT:
          this.previewLeft = {
            rgba: frame.rgba,
            width: frame.width,
            height: frame.height,
          };
          break;
        case MANUAL_KIND_RIGHT:
          this.previewRight = {
            rgba: frame.rgba,
            width: frame.width,
            height: frame.height,
          };
          break;
        case MANUAL_KIND_VALIDATION:
          this.#pendingValidation = {
            rgba: frame.rgba,
            width: frame.width,
            height: frame.height,
          };
          this.#maybeAssembleValidation();
          break;
        case MANUAL_KIND_REFERENCE:
          this.#pendingReference = {
            rgba: frame.rgba,
            width: frame.width,
            height: frame.height,
          };
          this.#maybeAssembleValidation();
          break;
        default:
          break;
      }
    };
    this.#frameChannel = channel;
    try {
      await invoke("manual_attach_preview", { onFrame: channel });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Clear the JS handler on the current binary channel, if any. */
  #detachFrameChannel(): void {
    if (this.#frameChannel !== null) {
      this.#frameChannel.onmessage = () => {};
      this.#frameChannel = null;
    }
  }

  /**
   * Assemble a `ValidationFrame` once its metadata and both RGBA buffers have
   * arrived (they ride two transports, so either may land first). Replaces the
   * entry for the frame rather than accumulating.
   */
  #maybeAssembleValidation(): void {
    const meta = this.#pendingValidationMeta;
    const validation = this.#pendingValidation;
    const reference = this.#pendingReference;
    if (meta === null || validation === null || reference === null) return;
    this.validationFrames = {
      ...this.validationFrames,
      [meta.frame]: {
        frame: meta.frame,
        rgba: validation.rgba,
        width: validation.width,
        height: validation.height,
        residual: meta.residual,
        verdict: meta.verdict,
        referenceRgba: reference.rgba,
        referenceWidth: reference.width,
        referenceHeight: reference.height,
      },
    };
    this.#pendingValidationMeta = null;
    this.#pendingValidation = null;
    this.#pendingReference = null;
  }

  /** Apply a typed worker event to the store state (worker-authoritative). */
  #onEvent(event: WorkerEventTyped): void {
    switch (event.kind) {
      case "manual_session_started": {
        const { frame, fps, frames_total } = event.data;
        this.frame = frame;
        this.fps = fps;
        this.framesTotal = frames_total;
        break;
      }
      case "manual_solve_state": {
        this.solving = event.data.busy;
        this.stale = event.data.stale;
        this.degenerate = event.data.degenerate;
        break;
      }
      case "manual_pins": {
        this.pins = event.data.pins;
        // `seeded` is true only on the first emit of a pre-populated session.
        // Once the operator edits, later emits carry false — but the notice
        // should persist for the session, so never clear a true back to false.
        if (event.data.seeded) this.seeded = true;
        break;
      }
      case "manual_solve_result": {
        this.solveResult = event.data;
        break;
      }
      case "manual_params": {
        const { left, right, layout } = event.data;
        this.params = { left, right };
        this.layout = layout;
        // The first emit of a session carries the profile baseline; the handles
        // clamp against it (MANU-05). Later emits carry edited values.
        if (this.baselineParams === null) this.baselineParams = { left, right };
        break;
      }
      case "manual_layout_delta": {
        this.layoutDelta = event.data;
        break;
      }
      case "audio_sync_result": {
        const { offset_frames, confidence, offset_semantics } = event.data;
        // The confidence is always surfaced for display, but a sub-floor
        // estimate is never applied as the offset: the flow defaults to 0 and
        // requires an explicit confirmation (MANU-02 / WR-02). This mirrors the
        // worker's `applied_audio_offset`, so the readout and the session agree.
        const confident =
          confidence !== null && confidence >= AUDIO_CONFIDENCE_FLOOR;
        this.audioConfidence = confidence;
        this.syncOffset = confident ? offset_frames : 0;
        // A confident estimate is the audio path; an absent or low one is
        // "none". A manual nudge later overrides this with "manual".
        this.syncMethod = confident ? "audio" : "none";
        this.offsetSemantics = offset_semantics;
        // A fresh estimate invalidates any prior confirmation.
        this.syncConfirmed = false;
        break;
      }
      case "manual_sync_set": {
        const { offset_frames, method, offset_semantics } = event.data;
        this.syncOffset = offset_frames;
        this.syncMethod = method;
        this.offsetSemantics = offset_semantics;
        break;
      }
      case "manual_validation_frame": {
        // Metadata only: the two RGBA buffers stream over the binary channel.
        // Store the metadata and assemble once the buffers arrive (either order).
        const { frame, residual, verdict, reference_width, reference_height } =
          event.data;
        this.#pendingValidationMeta = {
          frame,
          residual,
          verdict,
          referenceWidth: reference_width,
          referenceHeight: reference_height,
        };
        this.#maybeAssembleValidation();
        break;
      }
      case "manual_saved": {
        this.savedPath = event.data.path;
        break;
      }
      case "failed": {
        if (this.open) {
          this.error = formatWorkerError(event.data);
        }
        break;
      }
      default:
        break;
    }
  }

  /**
   * Open a manual session at `frame` (MANU-01).
   *
   * Subscribes and attaches the binary frame channel first, then posts
   * `manual_begin`, so the worker's `manual_session_started` event and the
   * session's first preview frames are never dropped by a not-yet-registered
   * listener/channel (the Tauri bridge is fire-and-forget).
   */
  async begin(frame = 0): Promise<void> {
    await this.init();
    // Attach the binary frame channel BEFORE posting `manual_begin`, so the
    // session's first preview pair is streamed rather than dropped.
    await this.attachFrameChannel();
    this.open = true;
    this.error = null;
    this.solving = false;
    this.stale = false;
    this.degenerate = false;
    this.previewLeft = null;
    this.previewRight = null;
    this.#pendingValidationMeta = null;
    this.#pendingValidation = null;
    this.#pendingReference = null;
    this.audioConfidence = null;
    this.syncOffset = 0;
    this.syncMethod = "none";
    this.syncConfirmed = false;
    this.pins = [];
    this.seeded = false;
    this.solveResult = null;
    this.params = null;
    this.baselineParams = null;
    this.layout = null;
    this.layoutDelta = null;
    this.validationFrames = {};
    this.savedPath = null;
    this.#undoStack = [];
    this.undoDepth = 0;
    try {
      await invoke("manual_begin", { frame });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Run the audio auto-sync on entry to the Time-align step (MANU-02).
   *
   * Subscribes first (idempotent) so the typed `audio_sync_result` is never
   * dropped, then posts `manual_detect_sync`. The worker mirrors the estimate
   * back; this store never derives a confidence locally.
   */
  async detectSync(): Promise<void> {
    await this.init();
    this.audioConfidence = null;
    this.syncConfirmed = false;
    this.error = null;
    try {
      await invoke("manual_detect_sync");
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Record the operator's manual sync offset (MANU-02).
   *
   * Resets confirmation: a nudge is a new choice that must be re-confirmed
   * before proceeding when the audio estimate is low or absent. The worker
   * records `SyncMethod::Manual` provenance.
   */
  async setSync(offset: number): Promise<void> {
    this.syncOffset = offset;
    this.syncConfirmed = false;
    try {
      await invoke("manual_set_sync", { offset_frames: offset });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Confirm the current offset, satisfying the low/no-confidence gate. */
  confirmSync(): void {
    this.syncConfirmed = true;
  }

  /** Change the reference frame (debounced by the caller if scrubbing). */
  async setFrame(frame: number): Promise<void> {
    this.frame = frame;
    try {
      await invoke("manual_set_frame", { frame });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Add one correspondence pin (MANU-03).
   *
   * Posts `manual_add_pin`; the worker clamps the points, mirrors the updated
   * pin list, renders an instant preview, and arms the debounced solve. The
   * store never derives the pin list or runs a solve itself.
   */
  async addPin(left: [number, number], right: [number, number]): Promise<void> {
    try {
      await invoke("manual_add_pin", { left_px: left, right_px: right });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Move one side of an existing pin (MANU-03). */
  async movePin(id: number, side: ManualSide, px: [number, number]): Promise<void> {
    try {
      await invoke("manual_move_pin", { id, side, px });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Remove one pin (MANU-03). */
  async removePin(id: number): Promise<void> {
    try {
      await invoke("manual_remove_pin", { id });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Remove every pin (MANU-03). An empty set never triggers a solve. */
  async clearPins(): Promise<void> {
    try {
      await invoke("manual_clear_pins");
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Validate the manual result on one additional frame (MANU-07).
   *
   * Posts `manual_validate`; the worker extracts the frame, renders the stitched
   * comparison under the current parameters, runs the engine for a per-frame
   * residual, and emits a typed `manual_validation_frame`. The store never
   * derives a residual locally, and validation is advisory (never a gate).
   */
  async validate(frame: number): Promise<void> {
    this.error = null;
    try {
      await invoke("manual_validate", { frame });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Save the manual result as a normal calibration profile (MANU-07).
   *
   * Posts `manual_save`; the worker assembles the profile, validates it, writes
   * it through the existing `.json` path, and emits `manual_saved`. A repeated
   * save overwrites the target.
   */
  async save(path: string): Promise<void> {
    this.error = null;
    try {
      await invoke("manual_save", { path });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Post a lens edit without touching the undo stack. */
  async #postLens(
    side: ManualSide,
    values: { fx: number; cx: number; cy: number; k1: number },
  ): Promise<void> {
    try {
      await invoke("manual_set_lens", {
        side,
        fx: values.fx,
        cx: values.cx,
        cy: values.cy,
        k1: values.k1,
      });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Post a layout edit without touching the undo stack. */
  async #postLayout(values: {
    cam_d: number;
    intersect: number;
    x_ty: number;
    x_rz: number;
  }): Promise<void> {
    try {
      await invoke("manual_set_layout", values);
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Snapshot the current lens value for one camera before a handle interaction
   * (MANU-05). Called once at the start of a drag so one drag is one undo step.
   */
  snapshotLens(side: ManualSide): void {
    if (!this.params) return;
    this.#pushUndo({
      kind: "lens",
      side,
      params: { ...(side === "left" ? this.params.left : this.params.right) },
    });
  }

  /** Snapshot the current layout before a handle interaction (MANU-06). */
  snapshotLayout(): void {
    if (!this.layout) return;
    this.#pushUndo({ kind: "layout", layout: { ...this.layout } });
  }

  /**
   * Apply an on-image lens-handle edit to one camera (MANU-05).
   *
   * Posts `manual_set_lens`; the worker clamps, enforces `fy = fx`, persists the
   * edited `CameraParams`, re-renders the instant preview without re-solving,
   * and arms the debounced background re-solve. The store never derives a
   * parameter locally. Undo is snapshotted separately at drag start
   * (`snapshotLens`), so an intermediate move does not flood the stack.
   */
  async setLens(
    side: ManualSide,
    values: { fx: number; cx: number; cy: number; k1: number },
  ): Promise<void> {
    await this.#postLens(side, values);
  }

  /**
   * Apply a constrained layout-handle edit (MANU-06).
   *
   * Posts `manual_set_layout`. One handle moves one parameter; no free 2-D
   * manipulation. Undo is snapshotted separately at interaction start
   * (`snapshotLayout`).
   */
  async setLayout(values: {
    cam_d: number;
    intersect: number;
    x_ty: number;
    x_rz: number;
  }): Promise<void> {
    await this.#postLayout(values);
  }

  /** Restore the lens intrinsics captured at session open (MANU-05). */
  async resetLens(): Promise<void> {
    this.#undoStack = [];
    this.undoDepth = 0;
    try {
      await invoke("manual_reset_lens");
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Restore the rig layout captured at session open (MANU-06). */
  async resetRig(): Promise<void> {
    this.#undoStack = [];
    this.undoDepth = 0;
    try {
      await invoke("manual_reset_rig");
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Push a handle edit onto the undo stack (bounded to the last 32). */
  #pushUndo(
    entry:
      | { kind: "lens"; side: ManualSide; params: CameraParamsView }
      | { kind: "layout"; layout: PlaneLayoutView },
  ): void {
    this.#undoStack.push(entry);
    if (this.#undoStack.length > 32) this.#undoStack.shift();
    this.undoDepth = this.#undoStack.length;
  }

  /** Undo the most recent handle edit (MANU-05). No-op on an empty stack. */
  undoHandle(): void {
    const entry = this.#undoStack.pop();
    this.undoDepth = this.#undoStack.length;
    if (!entry) return;
    if (entry.kind === "lens") {
      void this.#postLens(entry.side, {
        fx: entry.params.fx,
        cx: entry.params.cx,
        cy: entry.params.cy,
        k1: entry.params.k1,
      });
    } else {
      void this.#postLayout({
        cam_d: entry.layout.camera_axis_offset,
        intersect: entry.layout.intersect,
        x_ty: entry.layout.x_ty,
        x_rz: entry.layout.x_rz,
      });
    }
  }

  /**
   * Close the manual session (MANU-01).
   *
   * Clears the local preview and posts `manual_exit`; the worker drops the
   * retained planes. Exiting never touches an existing calibration profile.
   */
  async exit(): Promise<void> {
    this.open = false;
    this.#detachFrameChannel();
    this.previewLeft = null;
    this.previewRight = null;
    this.#pendingValidationMeta = null;
    this.#pendingValidation = null;
    this.#pendingReference = null;
    this.solving = false;
    this.stale = false;
    this.degenerate = false;
    this.error = null;
    this.audioConfidence = null;
    this.syncOffset = 0;
    this.syncMethod = "none";
    this.syncConfirmed = false;
    this.offsetSemantics = "";
    this.pins = [];
    this.seeded = false;
    this.solveResult = null;
    this.params = null;
    this.baselineParams = null;
    this.layout = null;
    this.layoutDelta = null;
    this.validationFrames = {};
    this.savedPath = null;
    this.#undoStack = [];
    this.undoDepth = 0;
    try {
      await invoke("manual_exit");
    } catch {
      // Exiting is best-effort; a closed worker is not an error the user sees.
    }
  }

  /** Navigate to a step (free back-navigation; any step is reachable). */
  goToStep(step: ManualStep): void {
    this.step = step;
  }

  /** Move to the next step, bounded at the last. */
  next(): void {
    const idx = MANUAL_STEPS.indexOf(this.step);
    if (idx < MANUAL_STEPS.length - 1) this.step = MANUAL_STEPS[idx + 1];
  }

  /** Move to the previous step, bounded at the first. */
  back(): void {
    const idx = MANUAL_STEPS.indexOf(this.step);
    if (idx > 0) this.step = MANUAL_STEPS[idx - 1];
  }

  /** The 0-based index of the active step. */
  get stepIndex(): number {
    const idx = MANUAL_STEPS.indexOf(this.step);
    return idx < 0 ? 0 : idx;
  }

  /**
   * Whether the offset must be explicitly confirmed before proceeding.
   *
   * True when the audio estimate is absent OR low — both render the same
   * "set the offset manually" gate (MANU-02), with the numeric confidence
   * still shown when present.
   */
  get syncGateRequired(): boolean {
    return (
      this.audioConfidence === null ||
      this.audioConfidence < AUDIO_CONFIDENCE_FLOOR
    );
  }
}

/** The shared manual calibration store instance. */
export const manual = new ManualStore();
