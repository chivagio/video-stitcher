//! Worker command vocabulary and the UI-side [`WorkerHandle`] (D-06).
//!
//! # The message-passing contract
//!
//! Every UI→engine interaction crosses an [`std::sync::mpsc`](std::sync::mpsc)
//! channel as a typed [`WorkerCommand`]. The `#[tauri::command]` handlers (Plan
//! 04) are **thin**: they validate their arguments, build a [`WorkerCommand`],
//! and call [`WorkerHandle::send`]. They never call an engine entry point or
//! any other engine API directly — that is the whole point of D-06 and of
//! FOUND-03's "all UI↔engine communication is message passing".
//!
//! ```text
//!   #[tauri::command] import/preview/export        engine worker thread
//!   ───────────────────────────────────────        ────────────────────
//!   build WorkerCommand ──▶ WorkerHandle::send ──▶ mpsc ──▶ worker_loop
//! ```
//!
//! This module deliberately imports **no engine types** (no `reco-io` /
//! `reco-core` engine surfaces) beyond the transport-agnostic
//! [`reco_control::ControlIntent`] input vocabulary: a command handler that
//! cannot name an engine type cannot accidentally call one.
//!
//! # Why `std::sync::mpsc`
//!
//! Phase 1's command volume is three buttons. The worker loop needs only
//! "drain everything pending, then block when idle" — `try_recv` + `recv`
//! cover that. `crossbeam-channel`'s `select!`, bounded back-pressure, and
//! `recv_timeout` ergonomics buy nothing at this volume and would add a
//! dependency (RESEARCH Assumption A6). If a later phase needs bounded/select
//! semantics the channel type is already behind [`WorkerHandle`], so swapping
//! it touches this module alone.

use std::sync::mpsc::{SendError, Sender};

use crate::events::WorkerError;

/// Load the hardcoded clips and match profile into the engine worker (D-08).
///
/// # The thin-command contract (D-06)
///
/// This handler **only** posts a typed [`WorkerCommand`] to the worker's
/// channel and returns. It performs no engine work, imports no engine type
/// (see the module header), and therefore cannot hold a lock across a render
/// tick or block the webview. On Windows a synchronous command that did work
/// here would deadlock window/webview creation (wry #583 — RESEARCH Pitfall 2);
/// the handler is declared `async` and the posting is a non-blocking
/// `mpsc::send` (the channel is unbounded).
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited; the
/// frontend renders that as an ERROR log line rather than hanging.
#[tauri::command]
pub async fn import(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Import)
}

/// Start the live stitched-frame preview loop (D-07).
///
/// Thin, same contract as [`import`]: post `Preview` and return. The worker
/// owns the decode/render loop; the webview only sees the events it emits.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn preview(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Preview)
}

/// Run the hardcoded file→file export to the fixed output path (D-08).
///
/// Thin, same contract as [`import`]: post `Export` and return. Export runs on
/// the worker thread (the engine's one-shot file→file job), never on the
/// webview/main thread.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn export(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Export)
}

/// A command from the UI to the engine worker.
///
/// `Clone + Send + 'static` — the compile-time assertion in `events.rs`
/// enforces it, because a command is moved through the worker channel.
// The Tauri command handlers that construct these variants land in Plan 04
// (they need the webview), and the worker loop that consumes them lands in
// Task 2 of this plan. A subset of variants has no constructor until then, so
// dead-code analysis flags them even in test builds (the tests exercise only
// Import/Preview/Shutdown). The suppression is narrowly scoped to this enum
// with this justification and disappears once the worker and handlers exist.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WorkerCommand {
    /// Load the hardcoded clips and match profile into the session.
    ///
    /// Paths are resolved inside the worker from `hardcoded.rs` (D-08); the
    /// command deliberately carries no paths so file selection stays out of
    /// the UI for Phase 1.
    Import,

    /// Start the live stitched-frame preview loop (D-07).
    Preview,

    /// Run the hardcoded file→file export to the fixed output path (D-08).
    Export,

    /// Forward a transport-agnostic input intent (pan / zoom / quality) to the
    /// worker's pose state on the same message-passing path as the other
    /// commands.
    ///
    /// Reusing `reco_control::ControlIntent` (PATTERNS.md "Don't Hand-Roll")
    /// means the UI never invents a parallel input vocabulary and there is no
    /// direct pose-control call from a command handler.
    Intent(reco_control::ControlIntent),

    /// Stop the worker loop and return.
    Shutdown,

    /// Simulate a device-loss event (reserved for Plan 05's FOUND-05 work).
    ///
    /// Rejected with [`WorkerError::Unsupported`] in this plan; the variant
    /// exists now so Plan 05 extends the protocol rather than reshaping it.
    SimulateDeviceLoss,
}

/// The UI-side handle to the engine worker's command channel.
///
/// Wraps the sending half of the channel. Only [`WorkerHandle::send`] is
/// exposed — there is no way to reach the worker's engine objects from here,
/// which is what makes the single-owner boundary (FOUND-03) structurally
/// difficult to violate.
// See the `WorkerCommand` note above: the handle's constructor (`EngineWorker`)
// and the Tauri handlers using it land in Task 2 / Plan 04.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct WorkerHandle {
    tx: Sender<WorkerCommand>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WorkerHandle {
    /// Wrap a command sender, e.g. the one retained by [`crate::worker::EngineWorker`].
    pub fn new(tx: Sender<WorkerCommand>) -> Self {
        Self { tx }
    }

    /// Post a command to the worker.
    ///
    /// Blocks only if the channel is bounded (it is not: `std::sync::mpsc` is
    /// unbounded), so in practice this is a non-blocking handoff.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::ChannelClosed`] when the worker thread has
    /// exited and the receiver has been dropped — the UI turns this into an
    /// ERROR log line rather than panicking.
    pub fn send(&self, cmd: WorkerCommand) -> Result<(), WorkerError> {
        self.tx
            .send(cmd)
            .map_err(|SendError(_)| WorkerError::ChannelClosed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn send_delivers_in_arrival_order() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Import);
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Preview);
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Shutdown);
    }

    #[test]
    fn send_after_receiver_drop_reports_channel_closed() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        drop(rx);
        assert_eq!(
            handle.send(WorkerCommand::Import),
            Err(WorkerError::ChannelClosed)
        );
    }

    #[test]
    fn handle_is_clone_and_send() {
        // The Tauri command handlers each hold their own clone of the handle;
        // `State<WorkerHandle>` requires it be `Send + Sync`.
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<WorkerHandle>();
    }
}
