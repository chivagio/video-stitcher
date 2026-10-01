// Phase 2 production preview shell entry (UI-SPEC Interaction + Event Log).
//
// Svelte 5 mount entry: renders App.svelte into #app. The full rune stores
// and component tree are built in Task 3; this minimal shell lets the
// Task 2 build verify pass.
//
// Pure UI (CONTEXT D-02/D-06/D-09): this module only sends worker commands
// over Tauri IPC and renders worker events. It never touches window
// lifecycle, raw handles, or any engine type.

import { mount } from "svelte";
import App from "./App.svelte";

const app = mount(App, {
  target: document.getElementById("app")!,
});

export default app;
