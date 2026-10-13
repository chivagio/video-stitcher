// Regression test: the debounced worker path preview must fire when a setting
// actually changes and stay quiet otherwise (EXPT-06).
//
// Bug: `App.svelte` seeds the export store from the left input's probed
// metadata with a `$effect`. `setInputMetadata` re-read `this.settings` (to
// preserve the operator's overrides) while writing a freshly derived
// `this.settings`, so the effect both read and wrote the same signal:
//
//   * Svelte re-ran the effect in a tight loop until its depth guard tripped
//     (`effect_update_depth_exceeded`), and
//   * every one of those runs called `schedulePathPreview()`, which clears and
//     re-arms the 200 ms debounce timer — so the worker was asked to resolve the
//     same output path again and again, one `export output path: …` INFO log
//     line per re-arm.
//
// The debounce and the worker's INFO mirror are intentional; the defect was
// that the effect re-armed them unconditionally on every reactive pass (any
// unrelated store write re-triggered the whole episode). These tests pin the
// fixed contract:
//
//   1. a metadata record with the same probed values (a new object from every
//      worker probe) must not re-derive settings, and must not re-arm the
//      preview — the comparison is semantic, never object identity;
//   2. a genuinely changed setting must resolve the path exactly once, worker
//      authoritative, through the same debounce.
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

const previewCalls = vi.hoisted(() => ({ count: 0, settings: [] as Array<Record<string, unknown>> }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args: { settings?: Record<string, unknown> }) => {
    if (cmd === "preview_export_path") {
      previewCalls.count++;
      previewCalls.settings.push(args.settings ?? {});
    }
    return undefined;
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));

import { exportStore, deriveSettings, sameInputMetadata, sameSettings } from "./export.svelte";
import type { InputMetadata } from "./types";

/** A probed metadata record shaped exactly like the worker's typed event. */
function meta(resolution: string): InputMetadata {
  const field = (value: string) => ({ value, provenance: "probed" as const });
  return {
    resolution: field(resolution),
    fps: field("60"),
    duration: field("16.7s"),
    codec: field("h264"),
  };
}

/** Longer than the store's 200 ms debounce window. */
const DEBOUNCE_MS = 200;
const AFTER_DEBOUNCE_MS = 260;

describe("export store path preview (EXPT-06)", () => {
  beforeEach(async () => {
    previewCalls.count = 0;
    previewCalls.settings.length = 0;
    // Reset the shared store, then drain whatever debounce the reset armed so
    // each test starts from a quiet state.
    exportStore.settings = deriveSettings("source_match", null);
    exportStore.preset = "source_match";
    exportStore.setInputMetadata(null);
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));
    previewCalls.count = 0;
    previewCalls.settings.length = 0;
  });

  afterEach(() => {
    exportStore.settings = deriveSettings("source_match", null);
    exportStore.preset = "source_match";
    exportStore.setInputMetadata(null);
  });

  it("re-seeding the same probed values does not re-arm the preview", async () => {
    exportStore.setInputMetadata(meta("3840×2160"));
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));
    expect(previewCalls.count).toBe(1);

    // The worker's next probe is a new object with identical values — this is
    // what the seeding effect sees on every reactive pass.
    exportStore.setInputMetadata(meta("3840×2160"));
    exportStore.setInputMetadata(meta("3840×2160"));
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));

    expect(previewCalls.count).toBe(1);
  });

  it("a changed resolution re-derives settings and resolves once", async () => {
    exportStore.setInputMetadata(meta("3840×2160"));
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));
    const before = previewCalls.count;
    expect(before).toBe(1);
    // "Source match" follows the probed resolution.
    expect(exportStore.settings.width).toBe(3840);
    expect(exportStore.settings.height).toBe(2160);

    exportStore.setInputMetadata(meta("1920×1080"));
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));

    expect(previewCalls.count).toBe(before + 1);
    expect(exportStore.settings.width).toBe(1920);
    expect(exportStore.settings.height).toBe(1080);
    // The worker stays authoritative: the settings sent are the store's own.
    expect(previewCalls.settings.at(-1)).toMatchObject({ width: 1920, height: 1080 });
  });

  it("a no-op preset/variant/output-dir/trim change does not re-arm", async () => {
    exportStore.setInputMetadata(meta("3840×2160"));
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));
    const before = previewCalls.count;

    exportStore.setPreset("source_match"); // already active
    exportStore.setVariant("panorama"); // already active
    exportStore.setOutputDir(null); // already default
    exportStore.setTrim(null, null); // already full clip
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));

    expect(previewCalls.count).toBe(before);
  });

  it("a real variant/output-dir change still resolves exactly once", async () => {
    exportStore.setInputMetadata(meta("3840×2160"));
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));
    const before = previewCalls.count;

    exportStore.setVariant("stacked");
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));
    expect(previewCalls.count).toBe(before + 1);

    exportStore.setOutputDir("D:/exports");
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));
    expect(previewCalls.count).toBe(before + 2);
    expect(previewCalls.settings.at(-1)).toMatchObject({ output_dir: "D:/exports" });
  });

  it("compares metadata by value, not identity", () => {
    expect(sameInputMetadata(null, null)).toBe(true);
    expect(sameInputMetadata(null, meta("1920×1080"))).toBe(false);
    expect(sameInputMetadata(meta("1920×1080"), null)).toBe(false);
    expect(sameInputMetadata(meta("1920×1080"), meta("1920×1080"))).toBe(true);
    // Same value, different provenance, is a real change.
    const other = meta("1920×1080");
    other.fps = { value: "60", provenance: "estimated" };
    expect(sameInputMetadata(meta("1920×1080"), other)).toBe(false);
    expect(sameInputMetadata(meta("1920×1080"), meta("3840×2160"))).toBe(false);
  });

  it("finds a rebuilt settings object semantically identical", () => {
    const a = deriveSettings("source_match", meta("3840×2160"));
    // What `deriveSettings` produces for the same inputs: a new object.
    const b = deriveSettings("source_match", meta("3840×2160"));
    expect(a).not.toBe(b);
    expect(sameSettings(a, b)).toBe(true);

    const changed = { ...a, width: 1280 };
    expect(sameSettings(a, changed)).toBe(false);
  });

  it("keeps the debounce window intact", async () => {
    // A burst of changes inside the window resolves once, not once per change.
    exportStore.setVariant("stacked");
    exportStore.setOutputDir("D:/exports");
    await new Promise((r) => setTimeout(r, DEBOUNCE_MS / 2));
    exportStore.setOutputDir("D:/other");
    await new Promise((r) => setTimeout(r, AFTER_DEBOUNCE_MS));

    expect(previewCalls.count).toBe(1);
    expect(previewCalls.settings.at(-1)).toMatchObject({ output_dir: "D:/other" });
  });
});
