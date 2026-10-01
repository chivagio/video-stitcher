// Phase 2 production preview shell entry (UI-SPEC Interaction + Event Log).
//
// Svelte 5 mount entry: renders App.svelte into #app. The rune stores
// (transport, pose, presenter, log) are initialized in App.svelte's onMount.
//
// Pure UI (CONTEXT D-02/D-06/D-09): this module only sends worker commands
// over Tauri IPC and renders worker events. It never touches window
// lifecycle, raw handles, or any engine type — all engine interplay goes
// through the worker's command channel, and events arrive as typed
// `WorkerEvent`s already projected by Rust into `{ level, message }`.

import { mount } from "svelte";
import App from "./App.svelte";

const app = mount(App, {
  target: document.getElementById("app")!,
});

export default app;
