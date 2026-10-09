// Regression test: the completed scorecard's Preview handoff.
//
// The result screen used to leave the operator with no next step — the store
// set `status = "done"` and nothing on the screen routed forward. The handoff
// is an explicit, non-forcing action (Save profile… and Re-run calibration stay
// available), and this pins that the action reaches the caller's handler rather
// than being a dead button: exactly the class of defect where a green
// `svelte-check` and a green build still shipped an unreachable control.
import { describe, it, expect, vi } from "vitest";
import { mount, tick } from "svelte";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => undefined) }));

import Scorecard from "./Scorecard.svelte";
import type { Scorecard as ScorecardView } from "../lib/types";

const FIXTURE: ScorecardView = {
  confidence: 0.91,
  confidence_band: "high",
  residual_error: 0.42,
  total_matches: 1280,
  per_frame_matches: 213.3,
  frames_used: 6,
  lens_profile: null,
  k1: -0.1234,
  layout_warning: null,
  sync: {
    method: "audio",
    confidence: 0.9,
    offset_frames: 2,
    provenance: { ran: "audio", is_manual: false },
    offset_semantics: "The right clip starts 2 frames after the left.",
  },
};

describe("Scorecard Preview handoff", () => {
  it("invokes the caller's handler when Go to Preview is clicked", async () => {
    const onGoToPreview = vi.fn();
    const target = document.createElement("div");
    document.body.appendChild(target);
    mount(Scorecard, {
      target,
      props: {
        scorecard: FIXTURE,
        onRerun: () => {},
        onGoToPreview,
      },
    });
    await tick();

    const handoff = [...target.querySelectorAll("button")].find((b) =>
      b.textContent?.includes("Go to Preview"),
    );
    expect(handoff).not.toBeUndefined();
    (handoff as HTMLButtonElement).click();

    expect(onGoToPreview).toHaveBeenCalledTimes(1);
  });
});
