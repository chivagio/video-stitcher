import { defineConfig } from "vitest/config";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Minimal config for the component regression tests: the Svelte plugin compiles
// the .svelte components, jsdom supplies the DOM. Kept separate from the
// production `vite.config.ts` (whose root is `ui/` and which also runs Tailwind).
export default defineConfig({
  plugins: [svelte()],
  resolve: {
    // Use the browser condition so `mount`/DOM APIs resolve (not the SSR build).
    conditions: ["browser"],
  },
  test: {
    environment: "jsdom",
    include: ["ui/src/**/*.test.ts"],
  },
});
