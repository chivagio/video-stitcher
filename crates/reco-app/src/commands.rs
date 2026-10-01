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

/// Begin/continue playing the preview session (PREV-02).
///
/// Thin, same contract as [`import`]: post `Play` and return. The worker owns
/// the paced session loop; the webview only sees the `Transport`/`Position`
/// events it emits.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn play(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Play)
}

/// Pause the preview session at the current position (PREV-02).
///
/// Thin, same contract as [`play`].
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn pause(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Pause)
}

/// Coalesced-seek to an absolute frame index (PREV-02).
///
/// Thin, same contract as [`play`]. The frame is typed `u64` (no strings) and
/// clamped inside the worker against the loaded source (T-02-04).
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn seek(state: tauri::State<'_, WorkerHandle>, frame: u64) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Seek { frame })
}

/// Validate a `StepFrame` direction at the command boundary.
///
/// Only `-1` and `+1` are valid; anything else is rejected before it can reach
/// the decoder (T-02-04). Extracted as a pure function so the boundary check is
/// unit-testable without a Tauri `State`.
fn validate_step_direction(direction: i32) -> Result<(), WorkerError> {
    if direction == 1 || direction == -1 {
        Ok(())
    } else {
        Err(WorkerError::Unsupported {
            operation: format!("step_frame direction {direction} (expected -1 or +1)"),
        })
    }
}

/// Step exactly `direction` frames (expected `-1` or `+1`; PREV-02).
///
/// Validates `direction` at the command boundary — any value other than ±1 is
/// rejected with [`WorkerError::Unsupported`] rather than reaching the decoder
/// (T-02-04) — then posts a `StepFrame`. Thin: names no engine type.
///
/// # Errors
///
/// Returns [`WorkerError::Unsupported`] for a direction other than ±1, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn step_frame(
    state: tauri::State<'_, WorkerHandle>,
    direction: i32,
) -> Result<(), WorkerError> {
    validate_step_direction(direction)?;
    state.send(WorkerCommand::StepFrame { direction })
}

/// Enable/disable full-clip looping (PREV-02).
///
/// Thin, same contract as [`play`].
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn set_loop(
    state: tauri::State<'_, WorkerHandle>,
    enabled: bool,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::SetLoop(enabled))
}

/// Report the webview chrome's collapsible state (UI-SPEC Geometry authority).
///
/// Thin: posts `SetChrome`; the worker recomputes the native viewport. Names no
/// engine type.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn set_chrome(
    state: tauri::State<'_, WorkerHandle>,
    panel_expanded: bool,
    drawer_expanded: bool,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::SetChrome {
        panel_expanded,
        drawer_expanded,
    })
}

/// A command from the UI to the engine worker.
///
/// `Clone + Send + 'static` — the compile-time assertion in `events.rs`
/// enforces it, because a command is moved through the worker channel.
// The Tauri command handlers construct these variants; the worker loop consumes
// them. A subset is still dead-code-flagged in test builds that do not exercise
// the binary path, so the suppression stays narrowly scoped to this enum.
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

    /// Begin/continue playing the preview session (PREV-02).
    Play,

    /// Pause the preview session at the current position (PREV-02).
    Pause,

    /// Coalesced seek to `frame` (PREV-02). The worker stores this as the single
    /// pending seek and executes at most one `source.seek` per session tick.
    Seek {
        /// Target frame index (0-based). Clamped to the loaded source's total.
        frame: u64,
    },

    /// Step exactly `direction` frames (expected `-1` or `+1`; PREV-02).
    StepFrame {
        /// Frame delta; only `-1` and `+1` are accepted (T-02-04).
        direction: i32,
    },

    /// Enable/disable full-clip looping (PREV-02).
    SetLoop(bool),

    /// Report the webview chrome's collapsible state so the worker recomputes
    /// the native viewport (UI-SPEC Geometry authority).
    SetChrome {
        /// Whether the right controls panel is expanded.
        panel_expanded: bool,
        /// Whether the event-log drawer is expanded.
        drawer_expanded: bool,
    },

    /// Reconfigure the native viewport for a new window size (PREV-01/04).
    ///
    /// Sent by `main.rs` on `WindowEvent::Resized`; carries no engine type and
    /// preserves transport position and pose (Pitfall 8).
    ResizeViewport {
        /// New window width in physical pixels.
        width: u32,
        /// New window height in physical pixels.
        height: u32,
    },

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

    #[test]
    fn new_transport_commands_round_trip_through_the_channel() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle.send(WorkerCommand::Play).unwrap();
        handle.send(WorkerCommand::Pause).unwrap();
        handle.send(WorkerCommand::Seek { frame: 42 }).unwrap();
        handle
            .send(WorkerCommand::StepFrame { direction: 1 })
            .unwrap();
        handle.send(WorkerCommand::SetLoop(true)).unwrap();
        handle
            .send(WorkerCommand::SetChrome {
                panel_expanded: true,
                drawer_expanded: false,
            })
            .unwrap();
        handle
            .send(WorkerCommand::ResizeViewport {
                width: 1280,
                height: 800,
            })
            .unwrap();
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Play);
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Pause);
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Seek { frame: 42 });
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::StepFrame { direction: 1 }
        );
        assert_eq!(rx.recv().unwrap(), WorkerCommand::SetLoop(true));
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::SetChrome {
                panel_expanded: true,
                drawer_expanded: false
            }
        );
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ResizeViewport {
                width: 1280,
                height: 800
            }
        );
    }

    #[test]
    fn step_direction_validation_accepts_only_plus_minus_one() {
        assert!(validate_step_direction(1).is_ok());
        assert!(validate_step_direction(-1).is_ok());
        for bad in [0, 2, -2, 30, i32::MAX, i32::MIN] {
            match validate_step_direction(bad) {
                Err(WorkerError::Unsupported { operation }) => {
                    assert!(operation.contains("expected -1 or +1"), "op: {operation}");
                }
                other => panic!("expected Unsupported for {bad}, got {other:?}"),
            }
        }
    }
}
