// Regression test: pointer capture for drag-to-pan must not swallow clicks on
// interactive elements inside the preview surface.
//
// Bug: `handlePointerDown` called `setPointerCapture` on the surface div for
// EVERY left-button press in the region (separate-window/readback + panorama).
// Chromium retargets all subsequent pointer events — including `click` — to
// the capture element, so the "Show preview window" button pressed visibly yet
// never fired: no invoke, no worker line, no error, no placeholder line, on
// every log surface. That was the inert Windows Preview screen on a confirmed
// fresh build (946f2cdc): IPC, the command channel, and the worker were all
// proven healthy by the continuous `preview_export_path` round-trips, yet the
// worker's `show preview window requested` discriminator never appeared.
//
// The press must only start a pan (and capture) when it begins on the
// non-interactive surface itself; a press on a button must pass through.
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, tick, unmount } from "svelte";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async () => undefined),
  Channel: vi.fn(),
}));

import PreviewSurface from "./PreviewSurface.svelte";

function ensurePointerCaptureMock(): void {
  // happy-dom may not implement pointer capture: provide a spyable stub.
  const proto = HTMLElement.prototype as HTMLElement & {
    setPointerCapture?: (pointerId: number) => void;
    releasePointerCapture?: (pointerId: number) => void;
    hasPointerCapture?: (pointerId: number) => boolean;
  };
  if (typeof proto.setPointerCapture !== "function") {
    proto.setPointerCapture = () => {};
  }
  if (typeof proto.releasePointerCapture !== "function") {
    proto.releasePointerCapture = () => {};
  }
  if (typeof proto.hasPointerCapture !== "function") {
    proto.hasPointerCapture = () => false;
  }
}

async function mountSeparateWindow(onShowPreviewWindow: () => void): Promise<{
  target: HTMLDivElement;
  cleanup: () => Promise<void>;
}> {
  ensurePointerCaptureMock();
  const target = document.createElement("div");
  document.body.appendChild(target);
  const component = mount(PreviewSurface, {
    target,
    props: {
      presenterKind: "separate_window",
      viewMode: "panorama",
      onAttachReadback: () => {},
      onShowPreviewWindow,
    },
  });
  await tick();
  return {
    target,
    cleanup: async () => {
      await unmount(component);
      target.remove();
    },
  };
}

function press(el: Element): void {
  el.dispatchEvent(
    new PointerEvent("pointerdown", { bubbles: true, button: 0 }),
  );
}

describe("PreviewSurface pointer capture", () => {
  beforeEach(() => {
    ensurePointerCaptureMock();
    vi.restoreAllMocks();
    vi.spyOn(HTMLElement.prototype, "setPointerCapture").mockImplementation(
      () => {},
    );
    vi.spyOn(HTMLElement.prototype, "releasePointerCapture").mockImplementation(
      () => {},
    );
    vi.spyOn(HTMLElement.prototype, "hasPointerCapture").mockReturnValue(false);
  });

  it("does not capture the pointer when the press starts on the preview button", async () => {
    const onShowPreviewWindow = vi.fn();
    const { target, cleanup } = await mountSeparateWindow(onShowPreviewWindow);

    const button = target.querySelector(
      ".placeholder-btn",
    ) as HTMLButtonElement;
    press(button);
    await tick();

    // No capture: the press's click still targets the button, so the action
    // fires instead of dying silently inside the pan gesture.
    expect(HTMLElement.prototype.setPointerCapture).not.toHaveBeenCalled();

    // The click itself reaches the wired handler.
    button.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await tick();
    expect(onShowPreviewWindow).toHaveBeenCalledTimes(1);

    await cleanup();
  });

  it("still captures the pointer when the press starts on the surface itself", async () => {
    const { target, cleanup } = await mountSeparateWindow(() => {});

    const surface = target.querySelector(".preview-surface") as HTMLDivElement;
    press(surface);
    await tick();

    // Drag-to-pan keeps working where there is no interactive element.
    expect(HTMLElement.prototype.setPointerCapture).toHaveBeenCalledTimes(1);

    await cleanup();
  });
});
