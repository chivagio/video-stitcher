// Regression test: a rejected presenter IPC call is reported, not swallowed.
//
// Bug: `PresenterStore.setPresenter` / `attachReadback` / `showPreviewWindow` were
// `try { await invoke(...) } catch {}` with the comment "The worker will emit a
// Presenter event to correct the state". That premise only holds when the command
// is ACCEPTED. When the invoke itself fails (unknown command, argument
// deserialization, managed-state extraction, a closed channel) the worker never
// sees it and no event is ever emitted, so every such failure looked exactly like
// "the button does nothing" — with no line on either log surface. This is what the
// inert Windows Preview screen reported: "Show preview window does nothing" and
// "switching the presenter to Readback does nothing", with no `failed` line.
//
// The failure must now be recorded on the store AND appended to the event log.
import { describe, it, expect, vi, beforeEach } from "vitest";

// The invoke mock is hoisted so each test can choose its own behaviour per
// command name, exactly as the Tauri bridge dispatches on the command string.
const invokeMock = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));

import { presenter } from "./presenter.svelte";
import { log } from "./log.svelte";

describe("presenter store IPC failure reporting", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    presenter.destroy();
    presenter.lastError = null;
    presenter.pendingAction = null;
    log.entries = [];
  });

  it("reports a rejected show_preview_window instead of swallowing it", async () => {
    // The exact Windows shape: the invoke rejects before the worker ever runs.
    invokeMock.mockRejectedValueOnce(new Error("command show_preview_window not found"));

    await presenter.showPreviewWindow();

    expect(presenter.lastError).toContain("show preview window failed");
    expect(presenter.lastError).toContain("command show_preview_window not found");
    // The event log is the contract surface (UI-SPEC Event Log), so the failure
    // must be readable where the operator already looks.
    const errors = log.entries.filter((e) => e.level === "error");
    expect(errors).toHaveLength(1);
    expect(errors[0].message).toContain("show preview window failed");
  });

  it("reports a rejected set_presenter with the requested kind", async () => {
    // A serde mismatch on `PresenterKind` is the other named candidate for the
    // inert Windows controls: the invoke rejects, nothing else is observable.
    invokeMock.mockRejectedValueOnce(new Error("invalid type: string \"readback\""));

    await presenter.setPresenter("readback");

    expect(presenter.lastError).toContain("presenter override to readback failed");
    expect(presenter.lastError).toContain("invalid type: string \"readback\"");
    expect(log.entries.filter((e) => e.level === "error")).toHaveLength(1);
  });

  it("clears the error once an action is accepted", async () => {
    invokeMock.mockResolvedValueOnce(undefined);

    await presenter.showPreviewWindow();

    expect(presenter.lastError).toBeNull();
    expect(log.entries.filter((e) => e.level === "error")).toHaveLength(0);
  });

  it("marks the action pending while the invoke is in flight", async () => {
    // A hung invoke (never settles) must stay visible: without this the
    // placeholder looks exactly like a button that does nothing.
    let resolveInvoke!: (value: unknown) => void;
    invokeMock.mockImplementationOnce(
      () => new Promise((resolve) => (resolveInvoke = resolve)),
    );

    const flight = presenter.showPreviewWindow();
    expect(presenter.pendingAction).toBe("show preview window");

    resolveInvoke(undefined);
    await flight;
    expect(presenter.pendingAction).toBeNull();
    expect(presenter.lastError).toBeNull();
  });

  it("clears the pending mark when the invoke rejects", async () => {
    invokeMock.mockRejectedValueOnce(new Error("channel closed"));

    await presenter.showPreviewWindow();

    expect(presenter.pendingAction).toBeNull();
    expect(presenter.lastError).toContain("show preview window failed");
  });
});
