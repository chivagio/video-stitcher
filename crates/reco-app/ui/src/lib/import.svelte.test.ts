// Regression test: a completed manual calibration is installed in the live
// session.
//
// Bug: `GpuEngineBackend::manual_save` already adopts the result and records
// `calibration_path` worker-side (MANU-07), but the worker reports it as
// `WorkerEvent::ManualSaved`, which no store handled. `importStore.profilePath`
// — the only thing `hasResult` keys off — therefore stayed null, `previewEnabled`
// stayed false in App.svelte, and the Preview rail step stayed disabled with a
// reason: the operator had to save the profile to a file and restart the app to
// reach a session the worker already had. This drives the typed event through
// the store's own listener and asserts the acknowledged state.
import { describe, it, expect, vi, beforeEach } from "vitest";

// The typed-event handler the store registers is captured here so the test can
// deliver a worker payload exactly as the Tauri bridge does. Hoisted, because
// the mock factory runs while the import graph is still evaluating and a plain
// module-level `let` would still be in its TDZ.
const listeners = vi.hoisted(() => ({
  typed: null as null | ((event: { payload: unknown }) => void),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => undefined) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(
    async (_event: string, handler: (event: { payload: unknown }) => void) => {
      listeners.typed = handler;
      return () => {};
    },
  ),
}));

import { importStore } from "./import.svelte";

describe("import store manual_saved", () => {
  beforeEach(async () => {
    importStore.destroy();
    listeners.typed = null;
    importStore.profilePath = null;
    importStore.profileStatus = "idle";
    importStore.profileError = null;
    importStore.resultInvalidated = false;
    await importStore.init();
  });

  it("installs the manual result so Preview is enabled without a restart", () => {
    // No result yet: the Preview rail step is disabled.
    expect(importStore.hasResult).toBe(false);

    listeners.typed!({
      payload: { kind: "manual_saved", data: { path: "/tmp/manual.json" } },
    });

    // The manual result is now exactly as loadable as a loaded profile.
    expect(importStore.profilePath).toBe("/tmp/manual.json");
    expect(importStore.hasResult).toBe(true);
  });
});
