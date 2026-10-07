<!--
  Phase 2 production preview shell (UI-SPEC Surface Layout Contract).

  The window is one full-window transparent webview composited ABOVE a
  Rust-owned native wgpu child view (the stitched panorama). The webview
  paints only the opaque chrome panels:

    * bottom transport strip (72px, full width) — opaque
    * right controls rail (40px collapsed / 280px expanded) — opaque
    * event-log drawer (240px expanded / 0 collapsed) — opaque

  The remaining TOP-LEFT region is intentionally TRANSPARENT and
  UNPAINTED — the native panorama renders there. Never paint a
  background, border, or shadow over it (Phase 2 constraint 1).

  The webview never sizes, moves, or creates the native view, and no raw
  window handle crosses this boundary (CONTEXT D-02).
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import type { Channel as ChannelType } from "@tauri-apps/api/core";
  import { transport } from "./lib/transport.svelte";
  import { pose } from "./lib/pose.svelte";
  import { presenter } from "./lib/presenter.svelte";
  import { log } from "./lib/log.svelte";
  import { importStore } from "./lib/import.svelte";
  import { calibration } from "./lib/calibration.svelte";
  import { fieldRoi } from "./lib/roi.svelte";
  import { exportStore } from "./lib/export.svelte";
  import PreviewSurface from "./components/PreviewSurface.svelte";
  import Timeline from "./components/Timeline.svelte";
  import TimecodeReadout from "./components/TimecodeReadout.svelte";
  import TransportButton from "./components/TransportButton.svelte";
  import ViewToggle from "./components/ViewToggle.svelte";
  import PresenterBadge from "./components/PresenterBadge.svelte";
  import ControlsPanel from "./components/ControlsPanel.svelte";
  import LogDrawer from "./components/LogDrawer.svelte";
  import WorkflowRail from "./components/WorkflowRail.svelte";
  import ImportScreen from "./components/ImportScreen.svelte";
  import CalibrateScreen from "./components/CalibrateScreen.svelte";
  import ExportScreen from "./components/ExportScreen.svelte";
  import FieldRoiEditor from "./components/FieldRoiEditor.svelte";
  import ConfirmDialog from "./components/ConfirmDialog.svelte";
  import type { PresenterKind, Screen, ViewMode } from "./lib/types";

  // Chrome state (reported to the worker via set_chrome).
  let panelExpanded = $state(false);
  let drawerExpanded = $state(false);

  // View mode (mirrors the worker's ViewMode).
  let viewMode = $state<ViewMode>("panorama");

  // Active screen (D3-01). Import is the landing screen when no result exists.
  let screen = $state<Screen>("import");

  // Cancel-calibration confirmation (CALB-02). Owned here so the dialog overlays
  // the whole app; the Calibrate screen requests it via `onRequestCancel`.
  let cancelDialogOpen = $state(false);

  // Field ROI editor panel on Preview (CALB-09). It renders its own
  // webview-owned canvas in the controls region (never over the Rust-owned
  // native child view), so opening it expands the controls panel and the worker
  // shrinks the native viewport to match.
  let fieldRoiOpen = $state(false);

  // Initialize stores on mount, then reconcile with the worker.
  onMount(() => {
    void (async () => {
      await transport.init();
      await pose.init();
      await presenter.init();
      await log.init();
      // Subscribe the import store to the typed bridge before the reconcile, so
      // its listener is registered when `republish_projection` fires (A10
      // ordering).
      await importStore.init();
      await calibration.init();
      await fieldRoi.init();
      // Subscribe the export store to the typed bridge before the reconcile too
      // (A10 ordering), so its EncoderList/ExportProgress listeners are live.
      await exportStore.init();
      void exportStore.probeEncoders();
      // Subscribe first, *then* ask the worker to re-assert its state. The
      // worker boots and imports before the webview has loaded, and the event
      // bridge only reaches listeners registered at emit time — so its opening
      // position/transport/pose/view were dropped and the UI sat empty
      // (transport disabled, clip length unknown). Ordering matters: the
      // reconcile must land after the listeners, or it is dropped too.
      try {
        await invoke("republish_projection");
      } catch (e) {
        console.error("republish_projection failed", e);
      }
    })();
    return () => {
      transport.destroy();
      pose.destroy();
      presenter.destroy();
      log.destroy();
      importStore.destroy();
      calibration.destroy();
      fieldRoi.destroy();
      exportStore.destroy();
    };
  });

  // Report chrome state changes to the worker. The active screen is part of
  // chrome state so Rust (the geometry/visibility authority) can suspend the
  // native child view on Import/Calibrate and show it on Preview (D3-01/E6).
  $effect(() => {
    void invoke("set_chrome", {
      panel_expanded: panelExpanded,
      drawer_expanded: drawerExpanded,
      active_screen: screen,
    });
  });

  // Seed the export store's "Source match" derivation from the left input's
  // probed metadata. The worker is authoritative for the resolution; the store
  // re-derives the preset parameters when it lands.
  $effect(() => {
    exportStore.setInputMetadata(importStore.inputs.left.metadata);
  });

  // Transport controls.
  function handlePlayPause(): void {
    if (transport.status === "playing") {
      void transport.pause();
    } else {
      void transport.play();
    }
  }

  function handleStepBack(): void {
    void transport.step(-1);
  }

  function handleStepForward(): void {
    void transport.step(1);
  }

  function handleLoopToggle(): void {
    void transport.setLoop(!transport.loop);
  }

  function handleSeek(frame: number): void {
    void transport.seek(frame);
  }

  // View toggle.
  function handleViewToggle(): void {
    const next = viewMode === "source" ? "panorama" : "source";
    viewMode = next;
    void invoke("set_view", { mode: next });
  }

  // Panel toggle.
  function handlePanelToggle(): void {
    panelExpanded = !panelExpanded;
  }

  // Field ROI editor toggle (CALB-09). Opening the editor expands the controls
  // panel so the native viewport (which excludes the panel region) never
  // overlaps the webview-owned editor canvas.
  function handleFieldRoiToggle(): void {
    fieldRoiOpen = !fieldRoiOpen;
    if (fieldRoiOpen) panelExpanded = true;
  }

  // Drawer toggle.
  function handleDrawerToggle(): void {
    drawerExpanded = !drawerExpanded;
  }

  // Workflow-rail navigation (D3-01). The rail is the single router; the
  // operator can revisit any enabled step at any time.
  function handleNavigate(next: Screen): void {
    screen = next;
  }

  // Calibrate CTA on the Import screen (plan 03-05 routing).
  function handleCalibrate(): void {
    screen = "calibrate";
  }

  // Cancel-calibration confirmation (CALB-02). The Calibrate screen requests
  // the dialog; confirming sets the worker's shared cancel flag.
  function handleRequestCancel(): void {
    cancelDialogOpen = true;
  }

  function handleConfirmCancel(): void {
    cancelDialogOpen = false;
    void calibration.cancel();
  }

  function handleKeepRunning(): void {
    cancelDialogOpen = false;
  }

  // Pose controls.
  function handleFovChange(value: number): void {
    void pose.setFov(value);
  }

  function handleResetView(): void {
    void pose.reset();
  }

  function handlePresenterChange(value: PresenterKind | "auto"): void {
    if (value !== "auto") {
      void presenter.setPresenter(value);
    }
  }

  // Readback attach.
  function handleAttachReadback(channel: ChannelType<ArrayBuffer>): void {
    void presenter.attachReadback(channel);
  }

  // Show preview window.
  function handleShowPreviewWindow(): void {
    void presenter.showPreviewWindow();
  }

  // Keyboard routing (focus-scoped).
  function handleGlobalKeyDown(e: KeyboardEvent): void {
    // Never hijack a text field: the lens search and the advanced number
    // inputs need Space, `l`, `v`, etc. to type, not to drive playback (WR-04).
    const target = e.target as HTMLElement | null;
    if (
      target &&
      (target.isContentEditable ||
        /^(input|textarea|select)$/i.test(target.tagName))
    ) {
      return;
    }
    if (e.key === " ") {
      e.preventDefault();
      handlePlayPause();
    } else if (e.key === ",") {
      e.preventDefault();
      handleStepBack();
    } else if (e.key === ".") {
      e.preventDefault();
      handleStepForward();
    } else if (e.key === "l" || e.key === "L") {
      e.preventDefault();
      handleLoopToggle();
    } else if (e.key === "v" || e.key === "V") {
      e.preventDefault();
      handleViewToggle();
    } else if (e.shiftKey && e.key === "ArrowLeft") {
      // CONTEXT D-03 "arrow-key nudge". Shift is required because bare
      // Left/Right belong to the timeline (frame stepping) whenever it has
      // focus, and a bare arrow reaching here too would seek and pan at once.
      e.preventDefault();
      // Signs mirror the CLI's arrow mapping (crates/reco-cli/src/preview.rs),
      // which is the authority: +yaw looks LEFT, so Left takes the positive
      // delta. Verified against view_matrix, not assumed from the doc comments.
      void pose.nudgeYawStep(1);
    } else if (e.shiftKey && e.key === "ArrowRight") {
      e.preventDefault();
      void pose.nudgeYawStep(-1);
    } else if (e.shiftKey && e.key === "ArrowUp") {
      e.preventDefault();
      void pose.nudgePitchStep(1);
    } else if (e.shiftKey && e.key === "ArrowDown") {
      e.preventDefault();
      void pose.nudgePitchStep(-1);
    }
  }

  // Derived state.
  const isPlaying = $derived(transport.status === "playing");
  const controlsEnabled = $derived(transport.controlsEnabled);
  const isWarning = $derived(presenter.kind !== "native");

  // Workflow-rail enablement (UI-SPEC Interaction & State Contract).
  const bothReady = $derived(
    importStore.inputs.left.status === "ready" &&
      importStore.inputs.right.status === "ready",
  );
  // Calibrate needs both clips valid, or a loaded profile (which populates a
  // result the operator can preview without re-running).
  const calibrateEnabled = $derived(bothReady || importStore.profilePath !== null);
  // Preview needs a valid result — a loaded profile or a fresh calibration run.
  const previewEnabled = $derived(
    importStore.hasResult || calibration.result !== null,
  );
  // Export needs the same valid calibration result (EXPT-01 edge probe: the
  // Export step is disabled with a reason until a result exists).
  const exportEnabled = $derived(previewEnabled);
</script>

<svelte:window onkeydown={handleGlobalKeyDown} />

<div class="app-shell">
  <!-- Workflow rail (D3-01): persistent router on every screen. -->
  <WorkflowRail
    active={screen}
    onNavigate={handleNavigate}
    logExpanded={drawerExpanded}
    onToggleLog={handleDrawerToggle}
    {calibrateEnabled}
    {previewEnabled}
    {exportEnabled}
  />

  {#if screen === "import"}
    <!-- Import screen: opaque dominant, covers the native preview region. -->
    <ImportScreen onCalibrate={handleCalibrate} />
  {:else if screen === "calibrate"}
    <!-- Calibrate wizard (CALB-01/02/03). -->
    <CalibrateScreen
      onRequestCancel={handleRequestCancel}
      onBackToImport={() => (screen = "import")}
    />
  {:else if screen === "export"}
    <!-- Modal Export screen (EXPT-01/02/04). The native view is suspended by
         Rust on this screen; the webview owns the whole surface. -->
    <ExportScreen />
  {:else}
    <!-- Preview surface (transparent in native mode) -->
    <PreviewSurface
      presenterKind={presenter.kind}
      {viewMode}
      onAttachReadback={handleAttachReadback}
      onShowPreviewWindow={handleShowPreviewWindow}
    />

    <!-- Transport bar (bottom) -->
    <div class="transport-bar" role="toolbar" aria-label="Transport and engine controls">
      <div class="transport-row">
        <!-- Timeline row -->
        <Timeline
          frame={transport.frame}
          total={transport.total}
          disabled={!controlsEnabled}
          onSeek={handleSeek}
        />
        <TimecodeReadout
          current={transport.currentTimecode}
          duration={transport.durationTimecode}
        />
      </div>

      <div class="transport-row">
        <!-- Control row -->
        <TransportButton
          icon="step-back"
          label="Step back"
          disabled={!controlsEnabled}
          onClick={handleStepBack}
        />
        <TransportButton
          icon={isPlaying ? "pause" : "play"}
          label={isPlaying ? "Pause" : "Play"}
          disabled={!controlsEnabled}
          active={isPlaying}
          onClick={handlePlayPause}
        />
        <TransportButton
          icon="step-forward"
          label="Step forward"
          disabled={!controlsEnabled}
          onClick={handleStepForward}
        />
        <TransportButton
          icon="loop"
          label="Loop"
          disabled={!controlsEnabled}
          active={transport.loop}
          onClick={handleLoopToggle}
        />
        <ViewToggle
          mode={viewMode}
          disabled={!controlsEnabled}
          onToggle={handleViewToggle}
        />
        <TransportButton
          icon="polygon"
          label="Field ROI"
          active={fieldRoiOpen}
          onClick={handleFieldRoiToggle}
        />
        <TransportButton
          icon="log"
          label="Log"
          active={drawerExpanded}
          onClick={handleDrawerToggle}
        />
        <PresenterBadge
          label={presenter.badgeLabel}
          isWarning={isWarning}
        />
      </div>
    </div>

    <!-- Controls panel (right edge) -->
    <ControlsPanel
      expanded={panelExpanded}
      onToggle={handlePanelToggle}
      onFovChange={handleFovChange}
      onResetView={handleResetView}
      onPresenterChange={handlePresenterChange}
    />

    <!-- Field ROI editor (CALB-09). Webview-owned, docked in the controls
         region (the native viewport excludes it), so it never hit-tests or
         paints over the Rust-owned native child view. -->
    {#if fieldRoiOpen}
      <div class="field-roi-panel" role="region" aria-label="Field ROI editor">
        <div class="field-roi-panel-header">
          <span class="field-roi-panel-title">Field ROI</span>
          <button
            type="button"
            class="field-roi-close"
            aria-label="Close field ROI editor"
            onclick={() => (fieldRoiOpen = false)}
          >
            Close
          </button>
        </div>
        <FieldRoiEditor report={calibration.debug} />
      </div>
    {/if}
  {/if}

  <!-- Event-log drawer (global; toggled from the workflow rail). -->
  <LogDrawer expanded={drawerExpanded} onToggle={handleDrawerToggle} />

  <!-- Cancel-calibration confirmation (CALB-02). -->
  <ConfirmDialog
    open={cancelDialogOpen}
    heading="Cancel calibration?"
    body="The calibration will stop and the partial result will be discarded. You can start again at any time."
    confirmLabel="Cancel calibration"
    cancelLabel="Keep running"
    destructive={true}
    onConfirm={handleConfirmCancel}
    onCancel={handleKeepRunning}
  />
</div>

<style>
  .app-shell {
    position: fixed;
    inset: 0;
    overflow: hidden;
  }

  .transport-bar {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 0;
    height: var(--transport-bar-height);
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: var(--space-xs);
    padding: 0 var(--space-md);
    background: var(--color-secondary);
    z-index: 10;
  }

  .transport-row {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
  }

  .field-roi-panel {
    position: fixed;
    right: 0;
    top: var(--workflow-rail-height);
    bottom: var(--transport-bar-height);
    width: var(--controls-panel-width);
    padding: var(--space-md);
    background: var(--color-secondary);
    border-left: 1px solid var(--color-dominant);
    overflow-y: auto;
    z-index: 12;
  }

  .field-roi-panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-sm);
    margin-bottom: var(--space-sm);
  }

  .field-roi-panel-title {
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .field-roi-close {
    min-height: 32px;
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
    background: var(--color-dominant);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    cursor: pointer;
  }

  .field-roi-close:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }
</style>
