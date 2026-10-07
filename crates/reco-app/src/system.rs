//! System information probe (DIAG-01) and the structured-log tracing layer
//! (DIAG-02).
//!
//! # DIAG-01 — System Info
//!
//! [`probe_system`] projects the engine's authoritative system state into a
//! transport-agnostic [`SystemInfoView`]: the GPU name / backend / driver from
//! [`reco_core::gpu::GpuContext`], the available encoders (with the hardware /
//! software flag), and a best-effort camera-device list. Every value that is
//! genuinely unknown stays `None` (or empty with an honest `devices_note`); the
//! webview renders `Not reported`, never a fabricated `0` (UI-SPEC Real-values
//! rule, DIAG-01 prohibition).
//!
//! # DIAG-02 — Structured logs
//!
//! [`UiLogLayer`] is a [`tracing_subscriber::Layer`] that captures each record's
//! level, target, message, and timestamp and forwards it as a typed
//! [`WorkerEvent::LogRecord`]. The webview renders the typed record and never
//! regex-parses log text (DIAG-02 prohibition). The layer is composed into the
//! single subscriber `main.rs` installs; the volume is bounded by the
//! `EnvFilter` (DEBUG below the configured filter never reaches the layer) and
//! by the LogViewer's carried 2000-line DOM cap (T-05-12).
//!
//! # DIAG-03 — bundle log retention
//!
//! The same layer also pushes each forwarded record into the shared
//! [`LogBuffer`](crate::diagnostics::LogBuffer) the diagnostics bundle reads, so
//! the one-click bundle carries the structured records without a second channel.

use std::sync::mpsc::Sender;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::events::{EncoderView, WorkerEvent};

/// The in-app System Info panel's field set (DIAG-01).
///
/// Mirrors into `ui/src/lib/types.ts`. Every GPU field is `Option`: a failed
/// [`reco_core::gpu::GpuContext`] probe leaves them `None` and the panel still
/// renders the encoders and devices (DIAG-01 edge probe "no GPU").
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SystemInfoView {
    /// The selected GPU adapter name, or `None` when no context could be made.
    pub gpu_name: Option<String>,
    /// The GPU backend (e.g. `"Vulkan"`, `"Metal"`), or `None`.
    pub backend: Option<String>,
    /// The GPU driver version string, or `None`.
    pub driver: Option<String>,
    /// The available H.264 encoders, hardware first (EXPT-02 order).
    pub encoders: Vec<EncoderView>,
    /// The camera device paths this platform exposes, best-effort.
    ///
    /// Empty on a platform that does not enumerate them; `devices_note` then
    /// carries the honest reason. Empty on Linux with `devices_note: None` means
    /// "no camera devices were found", never a silent success.
    pub devices: Vec<String>,
    /// An honest reason the device list is not enumerable on this platform, or
    /// `None` when the platform does enumerate (Linux).
    pub devices_note: Option<String>,
}

/// One structured log record captured from the tracing subscriber (DIAG-02).
///
/// `level` is the lowercase tracing level word (`"info"` / `"warn"` /
/// `"error"` / `"debug"` / `"trace"`); `target` is the emitting module path;
/// `timestamp_ms` is the Unix epoch time in milliseconds. Mirrors into
/// `ui/src/lib/types.ts`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LogRecord {
    /// The tracing level, lowercase.
    pub level: String,
    /// The emitting module path (`tracing`'s `target`).
    pub target: String,
    /// The formatted message (fields appended when the record carries them).
    pub message: String,
    /// Unix epoch time of the record, in milliseconds.
    pub timestamp_ms: u64,
}

/// Probe the system for the System Info panel (DIAG-01).
///
/// `gpu` is the app's live GPU context when the caller owns one (the real
/// worker backend passes `Some(&self.gpu)` so the panel reports the adapter the
/// app is actually using — never a second, possibly different, adapter). It is
/// `None` for the GPU-free path and tests, which renders the GPU fields
/// `Not reported` (the DIAG-01 "no GPU" edge probe).
#[must_use]
pub fn probe_system(gpu: Option<&reco_core::gpu::GpuContext>) -> SystemInfoView {
    let (gpu_name, backend, driver) = match gpu {
        Some(ctx) => (
            Some(ctx.gpu_name().to_string()),
            Some(ctx.backend_name().to_string()),
            Some(ctx.driver_info().to_string()),
        ),
        None => (None, None, None),
    };

    let (devices, devices_note) = probe_devices();

    SystemInfoView {
        gpu_name,
        backend,
        driver,
        encoders: probe_encoders(),
        devices,
        devices_note,
    }
}

/// Enumerate the H.264 encoders FFmpeg reports, in preference order (EXPT-02).
///
/// Shared with `preflight` so the FFmpeg prerequisite is probed through the
/// same surface the System Info panel renders.
pub(crate) fn probe_encoders() -> Vec<EncoderView> {
    reco_io::ffmpeg::encoder::available_encoders(reco_io::ffmpeg::encoder::VideoCodec::H264)
        .into_iter()
        .map(|e| EncoderView {
            name: e.name,
            description: e.description,
            is_hardware: e.is_hardware,
        })
        .collect()
}

/// Best-effort camera-device enumeration (DIAG-01).
///
/// Linux exposes capture devices as `/dev/videoN`; the list is sorted for a
/// stable readout. Every other platform reports the honest reason rather than
/// an empty success (DIAG-01 edge probe "no devices").
#[cfg(target_os = "linux")]
fn probe_devices() -> (Vec<String>, Option<String>) {
    let mut devices: Vec<String> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("video") && name[5..].chars().all(|c| c.is_ascii_digit()) {
                Some(format!("/dev/{name}"))
            } else {
                None
            }
        })
        .collect();
    devices.sort();
    (devices, None)
}

/// Non-Linux platforms do not enumerate camera devices here; report why.
#[cfg(not(target_os = "linux"))]
fn probe_devices() -> (Vec<String>, Option<String>) {
    (
        Vec::new(),
        Some("camera device enumeration is not available on this platform".to_string()),
    )
}

/// The tracing layer that forwards engine records to the UI as typed events
/// (DIAG-02) and retains them for the diagnostics bundle (DIAG-03).
///
/// One instance is composed into the single subscriber `main.rs` installs. It
/// sends a [`WorkerEvent::LogRecord`] for every record that survives the
/// configured `EnvFilter` and pushes the same record into the shared
/// [`LogBuffer`](crate::diagnostics::LogBuffer) the diagnostics bundle reads; a
/// closed channel is a silent drop (the UI is gone).
pub struct UiLogLayer {
    tx: Sender<WorkerEvent>,
    buffer: crate::diagnostics::LogBuffer,
}

impl UiLogLayer {
    /// Build the layer over the worker's event sender and the shared log buffer.
    #[must_use]
    pub fn new(tx: Sender<WorkerEvent>, buffer: crate::diagnostics::LogBuffer) -> Self {
        Self { tx, buffer }
    }
}

impl<S> Layer<S> for UiLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = RecordVisitor::default();
        event.record(&mut visitor);
        let record = LogRecord {
            level: metadata.level().as_str().to_lowercase(),
            target: metadata.target().to_string(),
            message: visitor.finish(),
            timestamp_ms: now_ms(),
        };
        // DIAG-03: retain the record for the diagnostics bundle. The clone is
        // cheap relative to the send and keeps the event and the buffer in sync.
        self.buffer.push(record.clone());
        // A closed channel means the app is shutting down; dropping the record
        // must never abort the emitting code path.
        let _ = self.tx.send(WorkerEvent::LogRecord { record });
    }
}

/// Current Unix epoch time in milliseconds (0 if the clock is before the epoch).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Extracts a tracing event's `message` field plus any other fields.
#[derive(Default)]
struct RecordVisitor {
    message: Option<String>,
    fields: Vec<String>,
}

impl RecordVisitor {
    /// The final message: the `message` field, with any other fields appended.
    fn finish(self) -> String {
        let mut out = self.message.unwrap_or_default();
        if !self.fields.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&self.fields.join(" "));
        }
        out
    }

    /// Record one field: the `message` field becomes the message body, any
    /// other field is appended as `name=value`.
    fn push(&mut self, field: &Field, rendered: String) {
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.fields.push(format!("{}={rendered}", field.name()));
        }
    }
}

impl Visit for RecordVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `tracing` renders `message` through a `fmt::Arguments` Debug impl,
        // which prints the formatted message without surrounding quotes.
        self.push(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_system_without_a_gpu_reports_none_not_fabricated_values() {
        // DIAG-01 edge probe "no GPU": the panel still renders, and every GPU
        // field is an honest `None` (the UI shows `Not reported`).
        let view = probe_system(None);
        assert_eq!(view.gpu_name, None);
        assert_eq!(view.backend, None);
        assert_eq!(view.driver, None);
    }

    #[test]
    fn system_info_view_roundtrips_and_keeps_unknowns_null() {
        let view = SystemInfoView {
            gpu_name: None,
            backend: None,
            driver: None,
            encoders: vec![EncoderView {
                name: "libx264".to_string(),
                description: "libx264 H.264".to_string(),
                is_hardware: false,
            }],
            devices: Vec::new(),
            devices_note: Some("not available on this platform".to_string()),
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(
            json.contains("\"gpu_name\":null"),
            "an unknown GPU name must serialize as null: {json}"
        );
        assert!(
            json.contains("\"devices_note\":\"not available on this platform\""),
            "the honest device reason must ride the payload: {json}"
        );
        let back: SystemInfoView = serde_json::from_str(&json).unwrap();
        assert_eq!(view, back);
    }

    #[test]
    fn log_record_roundtrips_through_serde() {
        let record = LogRecord {
            level: "warn".to_string(),
            target: "reco_core::gpu".to_string(),
            message: "adapter fell back to software".to_string(),
            timestamp_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.contains("\"level\":\"warn\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"target\":\"reco_core::gpu\""),
            "unexpected json: {json}"
        );
        let back: LogRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn ui_log_layer_forwards_records_as_typed_events() {
        use tracing_subscriber::prelude::*;

        let (tx, rx) = std::sync::mpsc::channel();
        let buffer = crate::diagnostics::LogBuffer::new();
        let subscriber = tracing_subscriber::registry().with(UiLogLayer::new(tx, buffer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "reco_test_target", "a structured record");
        });

        let event = rx.try_recv().expect("the layer must forward a record");
        match event {
            WorkerEvent::LogRecord { record } => {
                assert_eq!(record.level, "warn");
                assert_eq!(record.target, "reco_test_target");
                assert!(
                    record.message.contains("a structured record"),
                    "unexpected message: {}",
                    record.message
                );
            }
            other => panic!("expected LogRecord, got {other:?}"),
        }

        // DIAG-03: the layer also retains the record for the diagnostics bundle.
        let retained = buffer.snapshot();
        assert_eq!(retained.len(), 1, "the layer must retain the record");
        assert_eq!(retained[0].target, "reco_test_target");
    }
}
