import { defineConfig } from "vite";
import { execSync } from "node:child_process";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";

// The build id baked into the bundle (shown on the System screen): short
// commit hash plus a `-dirty` suffix when the working tree has uncommitted
// changes, so a Windows report can always name the exact source it runs.
// Falls back to "unknown" when git is unavailable (never fails the build).
function buildId(): string {
  try {
    const hash = execSync("git rev-parse --short HEAD", {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
    let dirty = "";
    try {
      const status = execSync("git status --porcelain", {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "ignore"],
      }).trim();
      if (status.length > 0) dirty = "-dirty";
    } catch {
      // Status unreadable: report the hash without a dirt flag.
    }
    return `${hash}${dirty}`;
  } catch {
    return "unknown";
  }
}

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
  define: {
    __RECO_BUILD_ID__: JSON.stringify(buildId()),
  },
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
