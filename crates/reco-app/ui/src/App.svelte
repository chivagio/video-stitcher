<!--
  Phase 2 production preview shell (UI-SPEC Surface Layout Contract).

  Minimal placeholder for Task 2 build verification. Task 3 replaces this
  with the full component tree: transport bar, controls panel, log drawer,
  preview surface, and state overlays.
-->
<main class="contents">
  <div id="preview-region" class="preview-region" aria-label="Panorama"></div>
</main>

<style>
  /* The preview region must stay fully transparent — any background/border/
   * shadow here would hide the native wgpu panorama (Phase 2 constraint 1). */
  .preview-region {
    position: fixed;
    left: 0;
    top: 0;
    width: 100%;
    height: 100%;
    background: transparent;
    border: 0;
    box-shadow: none;
  }
</style>
