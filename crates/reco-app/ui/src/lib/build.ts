/**
 * The build id baked into the bundle at Vite build time (see `define` in
 * `vite.config.ts`): short commit hash plus `-dirty` when the building tree
 * had uncommitted changes. Shown on the System screen so a field report can
 * always name the exact source it runs — no more "which binary ran"
 * ambiguity. Falls back to `"unknown (dev)"` outside a Vite build (vitest
 * without the define, dev server edge cases).
 */
declare const __RECO_BUILD_ID__: string | undefined;

export const BUILD_ID: string =
  typeof __RECO_BUILD_ID__ !== "undefined" && __RECO_BUILD_ID__ !== ""
    ? __RECO_BUILD_ID__
    : "unknown (dev)";
