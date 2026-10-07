<!--
  System Info screen (DIAG-01 / DIAG-02 / DIAG-05): GPU/backend/driver, the
  available encoders (HW/SW), the camera-device list, the runtime preflight
  status, and the structured LogViewer.

  The worker is authoritative: every value is the typed `SystemInfo` /
  `Preflight` / `LogRecord` the worker reported, rendered verbatim. A value that
  is genuinely unknown renders `Not reported` — never a fabricated `0` (UI-SPEC
  Real-values rule). Preflight pass/fail is word + colour, never colour alone,
  and a failing prerequisite always carries its remediation (DIAG-05
  prohibition).
-->
<script lang="ts">
  import { systemStore } from "../lib/system.svelte";
  import ActionButton from "./ActionButton.svelte";
  import Icon from "./Icon.svelte";
  import InlineNotice from "./InlineNotice.svelte";
  import LogViewer from "./LogViewer.svelte";

  const info = $derived(systemStore.info);
  const preflight = $derived(systemStore.preflight);

  /** Render an optional technical string, or the honest unknown marker. */
  function orNotReported(value: string | null): string {
    return value && value.length > 0 ? value : "Not reported";
  }
</script>

<div class="system-screen">
  <div class="system-column">
    <header class="screen-header">
      <div class="header-row">
        <div>
          <h2 class="screen-title">System</h2>
          <p class="screen-subtitle">
            GPU, encoders, camera devices, runtime prerequisites, and structured
            engine logs.
          </p>
        </div>
        <ActionButton
          variant="secondary"
          disabled={systemStore.loading}
          onClick={() => void systemStore.refresh()}
        >
          <Icon name="refresh" />
          {systemStore.loading ? "Reading…" : "Refresh"}
        </ActionButton>
      </div>
    </header>

    {#if systemStore.error !== null}
      <InlineNotice
        level="error"
        message={`Couldn't read system info: ${systemStore.error}`}
      />
    {/if}

    <section class="panel" aria-labelledby="sys-gpu-heading">
      <h3 id="sys-gpu-heading" class="panel-title">Graphics</h3>
      {#if info === null}
        <p class="reading">Reading…</p>
      {:else}
        <dl class="sys-dl">
          <div class="row">
            <dt class="label">GPU</dt>
            <dd class="value mono">{orNotReported(info.gpu_name)}</dd>
          </div>
          <div class="row">
            <dt class="label">Backend</dt>
            <dd class="value mono">{orNotReported(info.backend)}</dd>
          </div>
          <div class="row">
            <dt class="label">Driver</dt>
            <dd class="value mono">{orNotReported(info.driver)}</dd>
          </div>
        </dl>
      {/if}
    </section>

    <section class="panel" aria-labelledby="sys-encoders-heading">
      <h3 id="sys-encoders-heading" class="panel-title">Encoders</h3>
      {#if info === null}
        <p class="reading">Reading…</p>
      {:else if info.encoders.length === 0}
        <p class="empty">No encoders were reported.</p>
      {:else}
        <table class="encoder-table">
          <thead>
            <tr>
              <th scope="col">Encoder</th>
              <th scope="col">Type</th>
              <th scope="col">Description</th>
            </tr>
          </thead>
          <tbody>
            {#each info.encoders as encoder (encoder.name)}
              <tr>
                <td class="mono">{encoder.name}</td>
                <td>
                  <span class="tag {encoder.is_hardware ? 'hw' : 'sw'}">
                    {encoder.is_hardware ? "Hardware" : "Software"}
                  </span>
                </td>
                <td class="description">{encoder.description}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>

    <section class="panel" aria-labelledby="sys-devices-heading">
      <h3 id="sys-devices-heading" class="panel-title">Camera devices</h3>
      {#if info === null}
        <p class="reading">Reading…</p>
      {:else if info.devices_note !== null}
        <p class="empty">{info.devices_note}</p>
      {:else if info.devices.length === 0}
        <p class="empty">No camera devices found.</p>
      {:else}
        <ul class="device-list">
          {#each info.devices as device (device)}
            <li class="mono">{device}</li>
          {/each}
        </ul>
      {/if}
    </section>

    <section class="panel" aria-labelledby="sys-preflight-heading">
      <h3 id="sys-preflight-heading" class="panel-title">
        Runtime prerequisites
      </h3>
      {#if preflight === null}
        <p class="reading">Reading…</p>
      {:else}
        <p
          class="preflight-summary {preflight.all_ok ? 'ok' : 'warn'}"
          role="status"
        >
          {preflight.all_ok
            ? "Runtime prerequisites OK"
            : "Runtime prerequisites incomplete"}
        </p>
        <ul class="preflight-list">
          {#each preflight.items as item (item.id)}
            <li class="preflight-item">
              <span class="verdict {item.ok ? 'pass' : 'fail'}">
                {item.ok ? "Pass" : "Fail"}
              </span>
              <span class="preflight-label">{item.label}</span>
              <span class="preflight-detail">{item.detail}</span>
              {#if !item.ok}
                <span class="preflight-remediation">{item.remediation}</span>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <section class="panel" aria-labelledby="sys-logs-heading">
      <h3 id="sys-logs-heading" class="panel-title">Structured logs</h3>
      <LogViewer />
    </section>
  </div>
</div>

<style>
  .system-screen {
    position: fixed;
    top: var(--workflow-rail-height);
    left: 0;
    right: 0;
    bottom: 0;
    /* Opaque dominant: the native preview surface must not show through
       (UI-SPEC Screen Router). Rust suspends the native view on this screen. */
    background: var(--color-dominant);
    overflow-y: auto;
  }

  .system-column {
    max-width: 960px;
    margin: 0 auto;
    padding: var(--space-lg);
    display: flex;
    flex-direction: column;
    gap: var(--space-lg);
  }

  .header-row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-md);
  }

  .screen-title {
    margin: 0;
    font-size: var(--text-display);
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .screen-subtitle {
    margin: var(--space-xs) 0 0;
    color: var(--color-log-info);
    font-size: var(--text-body);
  }

  .panel {
    padding: var(--space-lg);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
  }

  .panel-title {
    margin: 0 0 var(--space-sm);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .sys-dl {
    margin: 0;
  }

  .row {
    display: grid;
    grid-template-columns: minmax(120px, 200px) 1fr;
    gap: var(--space-md);
    align-items: baseline;
    min-height: 28px;
    padding: var(--space-xs) 0;
  }

  .label {
    color: var(--color-log-info);
    font-weight: var(--weight-semibold);
  }

  .value {
    margin: 0;
    color: var(--color-body-text);
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .mono {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }

  .reading,
  .empty {
    margin: 0;
    color: var(--color-log-info);
    font-size: var(--text-body);
  }

  .encoder-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--text-body);
  }

  .encoder-table th,
  .encoder-table td {
    text-align: left;
    padding: var(--space-xs) var(--space-sm);
    border-bottom: 1px solid var(--color-dominant);
    vertical-align: top;
  }

  .encoder-table th {
    color: var(--color-log-info);
    font-weight: var(--weight-semibold);
  }

  .encoder-table td {
    color: var(--color-body-text);
  }

  .description {
    overflow-wrap: anywhere;
  }

  .tag {
    display: inline-block;
    padding: 1px var(--space-xs);
    border-radius: var(--space-xs);
    font-size: 12px;
    font-weight: var(--weight-semibold);
  }

  .tag.hw {
    color: var(--color-success);
    border: 1px solid var(--color-success);
  }

  .tag.sw {
    color: var(--color-log-info);
    border: 1px solid var(--color-log-info);
  }

  .device-list {
    margin: 0;
    padding-left: var(--space-md);
    color: var(--color-body-text);
  }

  .preflight-summary {
    margin: 0 0 var(--space-sm);
    font-weight: var(--weight-semibold);
  }

  .preflight-summary.ok {
    color: var(--color-success);
  }

  .preflight-summary.warn {
    color: var(--color-log-warn);
  }

  .preflight-list {
    margin: 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }

  .preflight-item {
    display: grid;
    grid-template-columns: auto minmax(120px, 160px) 1fr;
    gap: var(--space-sm);
    align-items: baseline;
  }

  .verdict {
    font-weight: var(--weight-semibold);
  }

  .verdict.pass {
    color: var(--color-success);
  }

  .verdict.fail {
    color: var(--color-log-warn);
  }

  .preflight-label {
    color: var(--color-body-text);
    font-weight: var(--weight-semibold);
  }

  .preflight-detail {
    color: var(--color-log-info);
    overflow-wrap: anywhere;
  }

  .preflight-remediation {
    grid-column: 3;
    color: var(--color-log-warn);
    overflow-wrap: anywhere;
  }
</style>
