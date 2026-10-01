import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";

// Phase 2 production preview shell build (UI-SPEC).
//
// Vite's `root` is `ui/` (where index.html, src/, and app.css live) and
// output goes to `ui/dist`, matching `tauri.conf.json`'s
// `build.frontendDist: "ui/dist"`. Only Vite-emitted files live in the
// output; the TypeScript/Svelte sources are build-time only.
//
// `npm run build` runs `svelte-check --tsconfig ./tsconfig.json && vite
// build`, so this config is never reached with a type error.
//
// The Svelte plugin compiles .svelte components; the Tailwind CSS v4 plugin
// processes the `@import "tailwindcss"` in app.css. No component library —
// hand-rolled Svelte 5 components styled with Tailwind utility classes.
export default defineConfig({
  root: "ui",
  base: "./",
  plugins: [svelte(), tailwindcss()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // The shell loads local assets in the webview (WebviewUrl::App); never a
    // remote URL. No source maps ship in the production bundle.
    sourcemap: false,
  },
  server: {
    strictPort: true,
  },
});
