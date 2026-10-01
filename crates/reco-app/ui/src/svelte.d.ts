// Svelte 5 component type declarations for svelte-check.
//
// TypeScript cannot resolve `.svelte` imports natively; this declaration
// maps them to the Svelte 5 `Component` type so svelte-check and tsc can
// type-check imports of .svelte files.
declare module "*.svelte" {
  import type { Component } from "svelte";
  const component: Component;
  export default component;
}
