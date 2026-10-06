/**
 * Worker-error formatting shared by the import and calibration rune stores.
 *
 * Worker errors cross IPC as the externally-tagged `WorkerError` enum, e.g.
 * `{ Engine: "…" }` or `{ InvalidInput: { field, reason } }`. This unwraps the
 * typed inner text so the UI renders the engine/worker message, never a bare
 * code or `[object Object]` (CONVENTIONS.md:113).
 *
 * It lives in its own module (not `import.svelte.ts`) so the calibration store
 * can use it without importing the import store — keeping the two store modules
 * acyclic.
 */

/** Render a typed worker error (or a rejected command) as display text. */
export function formatWorkerError(error: unknown): string {
  if (error == null) return "unknown error";
  if (typeof error === "string") return error;
  if (typeof error === "object") {
    const obj = error as Record<string, unknown>;
    const keys = Object.keys(obj);
    if (keys.length === 1) {
      const payload = obj[keys[0]];
      if (typeof payload === "string") return payload;
      if (payload && typeof payload === "object") {
        const p = payload as Record<string, unknown>;
        if (typeof p.field === "string" && typeof p.reason === "string") {
          return `invalid ${p.field}: ${p.reason}`;
        }
        return JSON.stringify(payload);
      }
    }
    if (typeof obj.message === "string") return obj.message;
    return JSON.stringify(error);
  }
  return String(error);
}
