//! Typed events emitted by the engine worker back to the UI (D-06).
//!
//! # The message-passing contract
//!
//! The engine worker owns every engine object for the app's lifetime
//! (FOUND-03). It never hands an engine value to the UI thread and the UI
//! never holds an engine lock across a tick. Instead the worker reports what
//! happened as a stream of [`WorkerEvent`] values over an
//! [`std::sync::mpsc`](std::sync::mpsc) channel; the Tauri bridge (Plan 04)
//! drains that channel on an async task and forwards each event to the webview
//! via `app.emit`.
//!
//! ```text
//!   engine worker thread                          UI thread / webview
//!   ─────────────────────                         ───────────────────
//!   engine call ──▶ WorkerEvent ──▶ Sender ──▶ mpsc ──▶ Tauri bridge ──▶ emit
//! ```
//!
//! # Shape
//!
//! [`WorkerEvent`] mirrors the transport-agnostic serde vocabulary of
//! `reco_control::ControlIntent` (`crates/reco-control/src/lib.rs:58-81`):
//! internally tagged (`kind` / `data`), snake-case, and `#[non_exhaustive]` so
//! new event categories can be added without breaking every consumer's match
//! arm (UI-SPEC Event Log Contract).
//!
//! Failures cross the channel as a typed [`WorkerError`] — never a `String`.
//! The UI renders `err.to_string()` as the human-readable message, but the
//! value stays typed so a later consumer can branch on it
//! (AGENTS.md / CONVENTIONS.md:113).

/// Severity level for an event-log line (UI-SPEC Event Log Contract).
///
/// The webview maps these one-to-one onto the three log colours: `Info` →
/// `#c8c8c8`, `Warn` → `#e0a02e` (degradation, e.g. the Wayland presenter
/// fallback or a soft-encoder fallback), `Error` → `#e5484d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Level {
    /// Normal event line: command accepted, work started or finished.
    Info,
    /// Non-fatal degradation: a fallback path was taken.
    Warn,
    /// A command was rejected or the engine failed.
    Error,
}

/// An event emitted by the engine worker and rendered in the webview log pane.
///
/// `Clone + Send + 'static` so it can cross the worker→UI channel; the
/// compile-time assertion at the bottom of this file enforces that bound.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkerEvent {
    /// A human-readable line for the event log at the given [`Level`].
    ///
    /// Used for the ordinary progress narrative ("import started", "preview
    /// running") the UI-SPEC Event Log Contract specifies.
    Log {
        /// Severity of the line (drives the log colour).
        level: Level,
        /// Message text, already user-facing.
        message: String,
    },

    /// A command was rejected or the engine failed.
    ///
    /// Carries the typed [`WorkerError`] rather than a string; the webview
    /// renders `error.to_string()`, preserving the typed value for any other
    /// consumer.
    Failed(WorkerError),
}

/// A typed worker failure that crosses the command/event channel.
///
/// `Clone + Send + Sync` (enforced by the assertion below) because it is moved
/// through [`std::sync::mpsc`](std::sync::mpsc) and may be read by more than
/// one consumer. It never carries a raw OS or GPU handle — only owned data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub enum WorkerError {
    /// A command required an imported session but none exists yet.
    #[error("no clips are imported yet — press Import first")]
    NotImported,

    /// The command is not implemented in this phase.
    ///
    /// Plan 05 implements `Shutdown`-adjacent device-loss simulation; until
    /// then the worker rejects it with this variant.
    #[error("operation not supported yet: {operation}")]
    Unsupported {
        /// Which operation was rejected.
        operation: String,
    },

    /// The worker was already shutting down and refuses new work.
    #[error("the engine worker is shutting down")]
    ShuttingDown,

    /// The worker thread's command channel is closed (the worker has exited).
    #[error("the engine worker is no longer running")]
    ChannelClosed,

    /// The worker thread did not stop within the shutdown timeout.
    #[error("the engine worker did not stop within {timeout_ms} ms")]
    ShutdownTimeout {
        /// The timeout that elapsed, in milliseconds.
        timeout_ms: u64,
    },

    /// An engine operation failed; the inner message is the rendered cause.
    ///
    /// Engine errors are flattened to a string **at this boundary** because
    /// several of them (e.g. `wgpu` request errors) are not `Clone`. The typed
    /// outer variant is what crosses the channel (CONVENTIONS.md:135).
    #[error("{0}")]
    Engine(String),
}

// Compile-time bound check: both halves of the protocol are `Clone + Send +
// 'static` so a worker thread can move them through an mpsc channel (the same
// idiom as `crates/reco-control/src/lib.rs:211-218`). The `WorkerCommand` half
// is declared in `commands.rs`; it is asserted here alongside `WorkerEvent` so
// the whole protocol's thread-safety is checked in one place.
const _: fn() = || {
    fn assert_clone_send<T: Clone + Send + 'static>() {}
    assert_clone_send::<WorkerEvent>();
    assert_clone_send::<WorkerError>();
    assert_clone_send::<crate::commands::WorkerCommand>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_event_roundtrips_through_serde() {
        let event = WorkerEvent::Log {
            level: Level::Info,
            message: "import started".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"kind\":\"log\""), "unexpected json: {json}");
        assert!(
            json.contains("\"level\":\"info\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn warn_level_serializes_snake_case() {
        let event = WorkerEvent::Log {
            level: Level::Warn,
            message: "software encoder fallback".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"level\":\"warn\""),
            "unexpected json: {json}"
        );
    }

    #[test]
    fn failed_event_roundtrips_typed_error() {
        let event = WorkerEvent::Failed(WorkerError::NotImported);
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"failed\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn failed_event_carries_a_typed_error_not_a_string() {
        // The UI renders `to_string()`, but the value stays typed so a
        // consumer can branch on the variant.
        let err = WorkerError::Unsupported {
            operation: "simulate_device_loss".to_string(),
        };
        assert!(err.to_string().contains("simulate_device_loss"));
        let event = WorkerEvent::Failed(err.clone());
        match event {
            WorkerEvent::Failed(error) => {
                assert_eq!(
                    error,
                    WorkerError::Unsupported {
                        operation: "simulate_device_loss".to_string()
                    }
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
