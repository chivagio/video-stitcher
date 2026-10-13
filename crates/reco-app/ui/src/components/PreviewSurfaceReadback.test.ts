// Regression test: the readback canvas must bind its backing store to each
// frame's own geometry, never to a fixed size.
//
// Bug (UAT test 7): swapping the presenter to Readback left the canvas empty.
// `paintFrame` sized `ImageData` from the hard-coded canvas attributes
// (1240x728, the collapsed-panel rect) while the frame the worker actually
// produced was 1000x728 — the controls panel MUST be expanded to reach the
// Presenter select, so every real reproduction ran at the expanded rect.
// `new ImageData(new Uint8ClampedArray(buffer), 1240, 728)` then threw
// `IndexSizeError` (it requires exactly 4*width*height bytes) inside the Tauri
// Channel `onmessage` handler; the throw was uncaught, no `putImageData` ran,
// and nothing was ever painted.
//
// The frame now carries `[width: u32 LE][height: u32 LE][RGBA]`
// (`READBACK_HEADER_LEN = 8`, mirrored by
// `crates/reco-app/src/presenter/readback.rs`), so these tests pin the
// frontend half of that contract: the canvas backing store follows the header,
// and a header/payload mismatch fails closed instead of throwing.
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { mount, tick, unmount } from "svelte";

// --- Tauri Channel mock -----------------------------------------------------
//
// The component builds `new Channel<ArrayBuffer>()` inside the `$effect` keyed
// on `isReadback` and hands it to `onAttachReadback`. There is no worker in a
// unit test, so capture the instances and drive `onmessage` by hand.
const { channelInstances } = vi.hoisted(() => ({
  channelInstances: [] as Array<{
    onmessage: ((buffer: ArrayBuffer) => void) | null;
  }>,
}));

vi.mock("@tauri-apps/api/core", () => {
  // A real class, not `vi.fn()`: `new Channel<ArrayBuffer>()` in the component
  // needs a constructible value.
  class MockChannel {
    onmessage: ((buffer: ArrayBuffer) => void) | null = null;

    constructor() {
      channelInstances.push(this);
    }
  }
  return {
    invoke: vi.fn(async () => undefined),
    Channel: MockChannel,
  };
});

import PreviewSurface from "./PreviewSurface.svelte";

/** Must equal `presenter::readback::READBACK_HEADER_LEN` on the Rust side. */
const READBACK_HEADER_LEN = 8;

/**
 * Minimal stand-in for the DOM `ImageData`, which jsdom does not implement.
 *
 * It enforces the browser contract that caused the original bug: the source
 * must be exactly `4 * width * height` bytes, otherwise the constructor throws
 * (a `SizeError`/`IndexSizeError` in the real engine). Keeping the throw here
 * means a regression that sizes the canvas wrong fails loudly instead of
 * silently painting a short buffer.
 */
class RecordingImageData {
  readonly width: number;
  readonly height: number;
  readonly data: Uint8ClampedArray;

  constructor(data: Uint8ClampedArray, width: number, height: number) {
    if (data.length !== width * height * 4) {
      throw new Error(
        `IndexSizeError: image data has ${data.length} bytes, ` +
          `4*${width}*${height} needs ${width * height * 4}`,
      );
    }
    this.width = width;
    this.height = height;
    this.data = data;
  }
}

function installImageData(): void {
  // Only shim when absent: a jsdom build that grows real canvas support should
  // be tested against the real implementation.
  if (typeof globalThis.ImageData !== "undefined") return;
  Object.defineProperty(globalThis, "ImageData", {
    value: RecordingImageData,
    writable: true,
    configurable: true,
  });
}

/**
 * jsdom has no 2D context (`getContext("2d")` returns null), so supply a
 * spyable one for the paint step.
 */
function installCanvasContext(): { putImageData: ReturnType<typeof vi.fn> } {
  const putImageData = vi.fn();
  const ctx = { putImageData } as unknown as CanvasRenderingContext2D;
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(ctx);
  return { putImageData };
}

async function mountReadback(): Promise<{
  canvas: HTMLCanvasElement;
  cleanup: () => Promise<void>;
}> {
  const target = document.createElement("div");
  document.body.appendChild(target);
  const component = mount(PreviewSurface, {
    target,
    props: {
      presenterKind: "readback",
      viewMode: "panorama",
      onAttachReadback: () => {},
      onShowPreviewWindow: () => {},
    },
  });
  await tick();
  const canvas = target.querySelector(
    "canvas.readback-canvas",
  ) as HTMLCanvasElement;
  if (canvas === null) {
    throw new Error("the readback arm rendered no canvas");
  }
  return {
    canvas,
    cleanup: async () => {
      await unmount(component);
      target.remove();
    },
  };
}

/** Hand a framed buffer to the channel the component attached. */
function deliver(buffer: ArrayBuffer): void {
  const channel = channelInstances.at(-1);
  if (channel === undefined || channel.onmessage === null) {
    throw new Error("PreviewSurface attached no readable readback channel");
  }
  channel.onmessage(buffer);
}

/**
 * Build a readback frame. `payloadBytes` defaults to the exact length the
 * header claims; pass a different value to forge a mismatched frame.
 */
function framed(
  width: number,
  height: number,
  payloadBytes?: number,
): ArrayBuffer {
  const buffer = new ArrayBuffer(
    READBACK_HEADER_LEN + (payloadBytes ?? width * height * 4),
  );
  const view = new DataView(buffer);
  view.setUint32(0, width, true);
  view.setUint32(4, height, true);
  return buffer;
}

describe("PreviewSurface readback dimension contract", () => {
  beforeEach(() => {
    channelInstances.length = 0;
    installImageData();
  });

  afterEach(() => {
    vi.restoreAllMocks();
    channelInstances.length = 0;
  });

  it("binds the canvas backing store to the frame's own width and height", async () => {
    const { putImageData } = installCanvasContext();
    const { canvas, cleanup } = await mountReadback();

    // The UAT reproduction: the controls panel is expanded (to reach the
    // Presenter select), so the frame is 1000x728. jsdom's default backing
    // store is 300x150, which a hard-coded 1240x728 used to overwrite.
    deliver(framed(1000, 728));
    await tick();

    expect(canvas.width).toBe(1000);
    expect(canvas.height).toBe(728);
    expect(putImageData).toHaveBeenCalledTimes(1);

    const painted = putImageData.mock.calls[0]?.[0];
    expect(painted?.width).toBe(1000);
    expect(painted?.height).toBe(728);
    // The pixels handed to the 2D context are the frame payload, exactly.
    expect(painted?.data.length).toBe(1000 * 728 * 4);

    await cleanup();
  });

  it("fails closed, without throwing, when the payload contradicts the header", async () => {
    const { putImageData } = installCanvasContext();
    const { canvas, cleanup } = await mountReadback();

    // The mismatch that started the bug: header says 1000x728, payload is
    // 1240x728 bytes. The real ImageData throws here; the guard must swallow
    // the frame instead of killing the uncaught Channel handler.
    expect(() => deliver(framed(1000, 728, 1240 * 728 * 4))).not.toThrow();
    await tick();

    expect(putImageData).not.toHaveBeenCalled();
    // Nothing was painted, and the backing store was left alone.
    expect(canvas.width).toBe(300);
    expect(canvas.height).toBe(150);

    await cleanup();
  });

  it("re-binds the backing store when a later frame has different geometry", async () => {
    const { putImageData } = installCanvasContext();
    const { canvas, cleanup } = await mountReadback();

    deliver(framed(320, 200));
    await tick();
    // The panel collapsing (or a window resize) changes the worker's viewport
    // and therefore the next frame's geometry; the canvas must follow it.
    deliver(framed(640, 400));
    await tick();

    expect(canvas.width).toBe(640);
    expect(canvas.height).toBe(400);
    expect(putImageData).toHaveBeenCalledTimes(2);

    await cleanup();
  });

  it("ignores frames too short to carry a header", async () => {
    const { putImageData } = installCanvasContext();
    const { cleanup } = await mountReadback();

    expect(() => deliver(new ArrayBuffer(4))).not.toThrow();
    await tick();

    expect(putImageData).not.toHaveBeenCalled();

    await cleanup();
  });

  it("ignores a header claiming zero dimensions", async () => {
    const { putImageData } = installCanvasContext();
    const { cleanup } = await mountReadback();

    // An all-zero header decodes to 0x0: reject rather than resize the canvas
    // to nothing (or divide by zero into a zero-length allocation).
    expect(() => deliver(framed(0, 0))).not.toThrow();
    await tick();

    expect(putImageData).not.toHaveBeenCalled();

    await cleanup();
  });
});
