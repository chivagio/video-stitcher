<!--
  Test harness: the App.svelte metadata-seeding effect (DO NOT use in the app).

  `App.svelte` seeds the export store's "Source match" derivation with:

      $effect(() => {
        exportStore.setInputMetadata(importStore.inputs.left.metadata);
      });

  A regression test cannot mount the whole app (Tauri bridges, native child view,
  every screen), and runes may only run inside `.svelte` files, so this harness
  reproduces that wiring — the same store pair, the same effect body — and counts
  how many times it runs. It exists solely for
  `export-path-preview.test.ts`; the app itself imports `App.svelte`.
-->
<script lang="ts">
  import { exportStore } from "./export.svelte";
  import { importStore } from "./import.svelte";

  let { onEffectRun }: { onEffectRun: () => void } = $props();

  $effect(() => {
    onEffectRun();
    exportStore.setInputMetadata(importStore.inputs.left.metadata);
  });
</script>
