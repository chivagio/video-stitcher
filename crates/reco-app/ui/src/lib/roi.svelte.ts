/**
 * Field ROI rune store (CALB-09).
 *
 * Holds the per-camera field ROI polygon (normalized `[0,1]`) for the active
 * camera, mirroring the engine's `FieldRoi { left, right }` shape. Edits are
 * local until `apply()` posts a typed `set_field_roi` command; the worker writes
 * the polygon onto the current calibration and confirms with a typed
 * `field_roi_applied` / `field_roi_cleared` event, which this store mirrors so it
 * never drifts from what the worker persisted.
 *
 * The worker is authoritative for the stored polygon: a rejected command leaves
 * the local polygon untouched and surfaces the typed error (UI-SPEC Field ROI
 * Editor Contract, error state).
 *
 * Pure UI (D-06): this store only sends worker commands over Tauri IPC and
 * renders worker events. It never touches engine types or window lifecycle.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { FieldRoi, InputRole, WorkerEventTyped } from "./types";
import { WORKER_EVENT_TYPED } from "./types";
import { formatWorkerError } from "./errors";

/** A vertex in normalized `[0,1]` source-frame coordinates. */
export type Vertex = [number, number];

/** Clamp a normalized coordinate to `[0,1]`; a non-finite value becomes `0`. */
export function clamp01(v: number): number {
  if (!Number.isFinite(v)) return 0;
  return Math.min(1, Math.max(0, v));
}

/** Whether a vertex list is a polygon (at least three vertices). */
export function isPolygon(verts: readonly Vertex[]): boolean {
  return verts.length >= 3;
}

/**
 * The field ROI rune store (class with `$state` fields, shared across
 * components).
 */
class FieldRoiStore {
  /** The left camera's polygon vertices, normalized `[0,1]`. */
  left = $state<Vertex[]>([]);
  /** The right camera's polygon vertices, normalized `[0,1]`. */
  right = $state<Vertex[]>([]);
  /** Which camera's polygon the editor is currently editing. */
  activeCamera = $state<InputRole>("left");
  /** The last typed rejection, or null. */
  error = $state<string | null>(null);
  /** Whether the last apply was confirmed by the worker. */
  saved = $state(false);

  /** The polygon as loaded / last confirmed by the worker, for Reset. */
  #baseline: FieldRoi = { left: [], right: [] };
  /** Unlisten function for the typed worker-event listener. */
  #unlisten: (() => void) | null = null;
  /** Whether an `apply()` is awaiting the worker's confirmation. */
  #pendingApply = false;

  /** Start listening for typed worker events. */
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
  }

  /** Apply a typed worker event to the store state (worker-authoritative). */
  #onEvent(event: WorkerEventTyped): void {
    switch (event.kind) {
      case "field_roi_applied": {
        this.#adopt(event.data.field_roi);
        this.#confirm();
        break;
      }
      case "field_roi_cleared": {
        this.#adopt({ left: [], right: [] });
        this.#confirm();
        break;
      }
      case "failed": {
        // A field-ROI rejection arrives as the generic typed `Failed` event
        // (the worker flattens command failures onto it). Only claim it while an
        // apply is in flight, so an unrelated failure cannot clobber the editor.
        if (this.#pendingApply) {
          this.#pendingApply = false;
          this.error = formatWorkerError(event.data);
        }
        break;
      }
      default:
        break;
    }
  }

  /** Adopt a polygon as both the current value and the Reset baseline. */
  #adopt(roi: FieldRoi): void {
    this.left = roi.left.map(([x, y]) => [x, y] as Vertex);
    this.right = roi.right.map(([x, y]) => [x, y] as Vertex);
    this.#baseline = { left: [...this.left], right: [...this.right] };
  }

  /** Mark a pending apply confirmed. */
  #confirm(): void {
    if (this.#pendingApply) {
      this.#pendingApply = false;
      this.saved = true;
      this.error = null;
    }
  }

  /** The active camera's polygon. */
  get active(): Vertex[] {
    return this.activeCamera === "left" ? this.left : this.right;
  }

  /** Replace the active camera's polygon. */
  #setActive(verts: Vertex[]): void {
    if (this.activeCamera === "left") this.left = verts;
    else this.right = verts;
  }

  /** Mark the polygon edited since the last save/load. */
  #edited(): void {
    this.error = null;
    this.saved = false;
  }

  /** Switch which camera's polygon is being edited. */
  setCamera(role: InputRole): void {
    this.activeCamera = role;
    this.error = null;
    this.saved = false;
  }

  /**
   * Insert a vertex into the active polygon.
   *
   * With no `index` (or an out-of-range one) the vertex is appended; otherwise
   * it is inserted before `index` (used by edge-click to splice onto the edge).
   */
  addVertex(x: number, y: number, index?: number): void {
    const verts = [...this.active];
    const v: Vertex = [clamp01(x), clamp01(y)];
    if (index === undefined || index >= verts.length) verts.push(v);
    else verts.splice(index, 0, v);
    this.#setActive(verts);
    this.#edited();
  }

  /** Remove one vertex from the active polygon. */
  removeVertex(index: number): void {
    const verts = [...this.active];
    if (index < 0 || index >= verts.length) return;
    verts.splice(index, 1);
    this.#setActive(verts);
    this.#edited();
  }

  /** Move one vertex of the active polygon to a normalized position. */
  moveVertex(index: number, x: number, y: number): void {
    const verts = [...this.active];
    if (index < 0 || index >= verts.length) return;
    verts[index] = [clamp01(x), clamp01(y)];
    this.#setActive(verts);
    this.#edited();
  }

  /** Translate the whole active polygon by a normalized delta. */
  movePolygon(dx: number, dy: number): void {
    const verts = this.active.map(
      ([x, y]) => [clamp01(x + dx), clamp01(y + dy)] as Vertex,
    );
    this.#setActive(verts);
    this.#edited();
  }

  /** Restore the polygon to its loaded / last-confirmed value. */
  reset(): void {
    this.#adopt(this.#baseline);
    this.error = null;
    this.saved = false;
  }

  /** Clear the active camera's polygon (treated as "no filter" by the engine). */
  clear(): void {
    this.#setActive([]);
    this.#edited();
  }

  /**
   * Restore the active polygon to a snapshot.
   *
   * Used to cancel an in-progress drag (Escape); it does not touch the Reset
   * baseline.
   */
  restore(verts: Vertex[]): void {
    this.#setActive(verts.map(([x, y]) => [x, y] as Vertex));
    this.#edited();
  }

  /**
   * Post the current polygon pair to the worker.
   *
   * The invoke resolves when the command is accepted; the worker's typed
   * `field_roi_applied` / `field_roi_cleared` (or `failed`) confirms it. A
   * boundary rejection (out-of-range/oversized) rejects the invoke directly.
   */
  async apply(): Promise<void> {
    this.#pendingApply = true;
    this.error = null;
    this.saved = false;
    try {
      await invoke("set_field_roi", { left: this.left, right: this.right });
    } catch (e) {
      this.#pendingApply = false;
      this.error = formatWorkerError(e);
    }
  }
}

/** The shared field ROI store instance. */
export const fieldRoi = new FieldRoiStore();
