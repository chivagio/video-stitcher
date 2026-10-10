// The build id baked into the bundle must always render something honest:
// the short commit hash from the building tree, or the explicit dev fallback
// outside a Vite build. A blank or crashing import would defeat the System
// screen's "which binary ran" purpose.
import { describe, it, expect } from "vitest";

import { BUILD_ID } from "./build";

describe("build id", () => {
  it("is a non-empty string", () => {
    expect(typeof BUILD_ID).toBe("string");
    expect(BUILD_ID.length).toBeGreaterThan(0);
  });
});
