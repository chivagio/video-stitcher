import { defineConfig } from "vite";

// Phase 1 walking-skeleton frontend build.
//
// Vite's `root` is `ui/` (where index.html, main.ts, and style.css live) and
// output goes to `ui/dist`, matching `tauri.conf.json`'s
// `build.frontendDist: "ui/dist"`. Only Vite-emitted files live in the output;
// the TypeScript sources are build-time only.
//
// `npm run build` runs `tsc --noEmit && vite build`, so this config is never
// reached with a type error.
//
// No framework plugin: the UI-SPEC locks framework-free vanilla DOM. Phase 2/3
// may adopt one without unwinding a pre-committed component model.
export default defineConfig({
  root: "ui",
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // The skeleton loads local assets in the webview (WebviewUrl::App); never a
    // remote URL. No source maps ship in the production bundle.
    sourcemap: false,
  },
  server: {
    strictPort: true,
  },
});
