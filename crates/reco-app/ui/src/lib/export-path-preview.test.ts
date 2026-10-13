// Regression test: the App.svelte metadata-seeding effect must not re-arm the
// debounced path preview on every reactive pass.
//
// Bug (see `export.svelte.test.ts` for the full contract): the seeding
// `$effect` called `setInputMetadata`, which read `this.settings` while writing
// a freshly derived one. The effect therefore read and wrote the same signal,
// so Svelte re-ran it in a tight loop until its depth guard tripped
// (`effect_update_depth_exceeded`), and each of those runs cleared and re-armed
// the 200 ms debounce — the worker was asked to resolve the same output path
// forever, one `export output path: …` INFO line per re-arm.
//
// Before the fix this test measured ~1001 effect runs per pass (Svelte's
// `flush_count > 1000` guard) and an unbounded number of preview requests; the
// assertions below are written so that a regression is unmistakable.
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, unmount, tick } from "svelte";

const state = vi.hoisted(() => ({ previews: 0, effectRuns: 0 }));
const listeners = vi.hoisted(() => ({ typed: [] as ((e: { payload: unknown }) => void)[] }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string) => {
    if (cmd === "preview_export_path") state.previews++;
    return undefined;
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_event: string, handler: (e: { payload: unknown }) => void) => {
    listeners.typed.push(handler);
    return () => {};
  }),
}));

import { exportStore } from "./export.svelte";
import { importStore } from "./import.svelte";
import ExportSeedHarness from "./ExportSeedHarness.svelte";
import type { InputMetadata } from "./types";

/** Deliver a typed worker event exactly as the Tauri bridge does. */
function emit(payload: unknown): void {
  for (const handler of listeners.typed) handler({ payload });
}

function meta(resolution: string): InputMetadata {
  const field = (value: string) => ({ value, provenance: "probed" as const });
  return {
    resolution: field(resolution),
    fps: field("60"),
    duration: field("16.7s"),
    codec: field("h264"),
  };
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const DEBOUNCE_SETTLE_MS = 260;

describe("App metadata-seeding effect (EXPT-06)", () => {
  beforeEach(async () => {
    state.previews = 0;
    state.effectRuns = 0;
    listeners.typed.length = 0;
    importStore.destroy();
    await importStore.init();

    exportStore.settings = {
      preset: "source_match",
      width: 1920,
      height: 1080,
      codec: "h264",
      quality: "high",
      bitrate_kbps: null,
      encoder_name: null,
      start_frame: null,
      end_frame: null,
      variant: "panorama",
      output_dir: null,
    };
    exportStore.preset = "source_match";
    exportStore.setInputMetadata(null);
    await sleep(DEBOUNCE_SETTLE_MS);
    state.previews = 0;
    state.effectRuns = 0;
  });

  it("resolves once per real change and never re-arms on its own", async () => {
    const target = document.createElement("div");
    document.body.appendChild(target);
    const component = mount(ExportSeedHarness, {
      target,
      props: { onEffectRun: () => state.effectRuns++ },
    });
    await tick();

    // The probed metadata lands: one effect pass, one resolution request.
    emit({ kind: "import_metadata", data: { role: "left", metadata: meta("3840×2160") } });
    await tick();
    await sleep(DEBOUNCE_SETTLE_MS);

    expect(state.previews).toBe(1);
    // One initial pass plus the one real metadata change. Before the fix this
    // was ~1001 per pass (Svelte's depth guard) and kept climbing.
    expect(state.effectRuns).toBeLessThanOrEqual(2);

    // Reactive noise that must NOT re-arm the preview: an unrelated store
    // write, then the worker re-probing the identical values (a new object).
    importStore.checksUnavailable = true;
    await tick();
    emit({
      kind: "import_metadata",
      data: { role: "left", metadata: meta("3840×2160") },
    });
    await tick();
    await sleep(DEBOUNCE_SETTLE_MS);

    expect(state.previews).toBe(1);
    expect(state.effectRuns).toBeLessThanOrEqual(3);

    // A real change still resolves, once.
    emit({ kind: "import_metadata", data: { role: "left", metadata: meta("1920×1080") } });
    await tick();
    await sleep(DEBOUNCE_SETTLE_MS);

    expect(state.previews).toBe(2);
    expect(exportStore.settings.width).toBe(1920);

    unmount(component as never);
    importStore.destroy();
  }, 15000);
});
