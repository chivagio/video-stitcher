// Regression test for the lens-profile picker's async catalog handling.
//
// Bug: after the worker emitted `lens_candidates`, the menu stayed on
// "Searching profiles…" — the candidate state was stored nested under
// `inputs[role]` and its async update did not reach the picker's rendering.
// The state now lives in the top-level `lensCatalog` record (reassigned on each
// update), and this test drives the exact transition and asserts the DOM.
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, tick } from "svelte";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => undefined) }));

import LensProfilePicker from "./LensProfilePicker.svelte";
import { importStore } from "../lib/import.svelte";
import type { LensCandidate } from "../lib/types";

const GOPRO: LensCandidate = {
  camera: "GoPro",
  lens: "HERO10 Wide",
  width: 1920,
  height: 1080,
};

async function flush(): Promise<void> {
  await Promise.resolve();
  await tick();
  await Promise.resolve();
  await tick();
}

describe("LensProfilePicker async catalog", () => {
  beforeEach(() => {
    importStore.inputs.left.status = "ready";
    importStore.inputs.left.path = "left.mp4";
  });

  it("renders the candidate list when the catalog resolves after the menu opens", async () => {
    importStore.lensCatalog = {
      ...importStore.lensCatalog,
      left: { status: "idle", candidates: [], error: null },
    };

    const target = document.createElement("div");
    document.body.appendChild(target);
    mount(LensProfilePicker, { target, props: { slot: importStore.inputs.left } });
    await flush();

    const trigger = target.querySelector("button.lens-trigger");
    expect(trigger).not.toBeNull();
    (trigger as HTMLButtonElement).click();
    await flush();

    // The menu is open and awaiting the async response.
    expect(target.textContent).toContain("Searching profiles");

    // Simulate the worker's `lens_candidates` response landing.
    importStore.lensCatalog = {
      ...importStore.lensCatalog,
      left: { status: "ready", candidates: [GOPRO], error: null },
    };
    await flush();

    const list = target.querySelector("ul.candidate-list") as HTMLUListElement;
    expect(list).not.toBeNull();
    expect(list.hidden).toBe(false);
    expect(target.textContent).toContain("GoPro HERO10 Wide");

    const loadingNote = [...target.querySelectorAll("p.menu-note")].find((p) =>
      p.textContent?.includes("Searching profiles"),
    ) as HTMLParagraphElement;
    expect(loadingNote.hidden).toBe(true);
  });

  it("shows the empty-state note when the catalog resolves with no candidates", async () => {
    importStore.lensCatalog = {
      ...importStore.lensCatalog,
      left: { status: "loading", candidates: [], error: null },
    };

    const target = document.createElement("div");
    document.body.appendChild(target);
    mount(LensProfilePicker, { target, props: { slot: importStore.inputs.left } });
    await flush();

    (target.querySelector("button.lens-trigger") as HTMLButtonElement).click();
    await flush();

    importStore.lensCatalog = {
      ...importStore.lensCatalog,
      left: { status: "ready", candidates: [], error: null },
    };
    await flush();

    const emptyNote = [...target.querySelectorAll("p.menu-note")].find((p) =>
      p.textContent?.includes("No matching profiles found"),
    ) as HTMLParagraphElement;
    expect(emptyNote.hidden).toBe(false);
    const list = target.querySelector("ul.candidate-list") as HTMLUListElement;
    expect(list.hidden).toBe(true);
  });
});
