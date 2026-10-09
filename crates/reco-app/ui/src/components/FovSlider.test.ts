// Regression test: the FOV slider's advertised range must be a range the engine
// accepts (FRICTION A12 — "a bound the engine cannot reach is indistinguishable
// from a broken control").
//
// The floor used to be a hardcoded 40 while `PoseControl::fov_min_degrees` is
// 20: the control advertised a minimum the engine never enforced, the mirror
// image of the earlier ceiling lie (40..150 advertised while
// `clamp_via_coverage` pinned the pose to 50.87°). The floor is now the engine
// minimum and the ceiling stays the worker-reported, coverage-limited maximum,
// with a guard so a pathological coverage ceiling cannot produce an empty range.
import { describe, it, expect, vi } from "vitest";
import { mount, tick } from "svelte";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => undefined) }));

import FovSlider from "./FovSlider.svelte";

async function mountSlider(
  props: { value: number; max: number; onChange: (value: number) => void },
): Promise<HTMLInputElement> {
  const target = document.createElement("div");
  document.body.appendChild(target);
  mount(FovSlider, { target, props });
  await tick();
  return target.querySelector("input.fov-input") as HTMLInputElement;
}

describe("FovSlider bounds", () => {
  it("spans the engine's 20° floor to the worker-reported coverage ceiling", async () => {
    const input = await mountSlider({ value: 75, max: 150, onChange: () => {} });
    // The operator-requested 20-150, while the worker reports the configured
    // maximum.
    expect(input.min).toBe("20");
    expect(input.max).toBe("150");
  });

  it("keeps the range non-empty for a pathological coverage ceiling", async () => {
    const input = await mountSlider({ value: 20, max: 0, onChange: () => {} });
    // Everything below the engine minimum is unreachable anyway, so the floor
    // doubles as the empty-range guard.
    expect(input.min).toBe("20");
    expect(input.max).toBe("20");
  });

  it("clamps an above-ceiling value before it reaches the engine", async () => {
    const onChange = vi.fn();
    const input = await mountSlider({ value: 40, max: 50, onChange });

    input.value = "150";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    await tick();

    expect(onChange).toHaveBeenCalledWith(50);
  });
});
