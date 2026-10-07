//! The one-click, redacted, local-only diagnostics bundle (DIAG-03).
//!
//! # What the bundle is
//!
//! [`write_bundle`] writes a single local zip containing the engine's
//! structured logs, the probed system information, and the calibration
//! artifacts (the active profile and the debug inspector payload). It is
//! assembled entirely in-process and written to a user-chosen path; there is no
//! network client anywhere in `reco-app` (DIAG-03 prohibition), so the bundle
//! can only ever land on the operator's disk.
//!
//! # Redaction
//!
//! Every text entry passes through [`redact`]: the home directory becomes
//! `<home>` and the account name (the home's last path component) becomes
//! `<user>`. Media bytes are never included — the debug inspector's raw RGBA
//! thumbnails are dropped by [`redacted_debug_json`] (only their dimensions and
//! the normalized match rows survive) — and no signing secret is ever gathered
//! (T-05-17).
//!
//! # Atomicity
//!
//! The zip is written to a sibling temp path and renamed over the target only
//! on success, so a failure (an unwritable path, a mid-write error) leaves no
//! partial bundle behind (T-05-19).

use std::collections::VecDeque;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use crate::events::DebugReport;
use crate::system::{LogRecord, SystemInfoView};

/// Maximum number of structured records retained for the bundle.
///
/// The same 2000-line bound the LogViewer carries (T-05-12): a long session
/// cannot grow the buffer without limit, and the bundle is a bounded snapshot.
pub const MAX_LOG_RECORDS: usize = 2000;

/// A bounded, shared buffer of structured log records (DIAG-03).
///
/// The tracing layer ([`crate::system::UiLogLayer`]) pushes every record it
/// forwards into the same stream the webview renders; the diagnostics bundle
/// reads a [`LogBuffer::snapshot`] at export time. The buffer is shared (an
/// `Arc<Mutex<..>>`) because the layer lives on the tracing thread and the
/// worker reads it on the engine thread.
#[derive(Clone, Default)]
pub struct LogBuffer {
    inner: std::sync::Arc<std::sync::Mutex<VecDeque<LogRecord>>>,
}

impl LogBuffer {
    /// Build an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `record`, evicting the oldest when [`MAX_LOG_RECORDS`] is reached.
    ///
    /// A poisoned lock is a silent drop: a failed record must never abort the
    /// emitting code path.
    pub fn push(&self, record: LogRecord) {
        if let Ok(mut buf) = self.inner.lock() {
            if buf.len() >= MAX_LOG_RECORDS {
                buf.pop_front();
            }
            buf.push_back(record);
        }
    }

    /// A snapshot of the retained records, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<LogRecord> {
        self.inner
            .lock()
            .map(|buf| buf.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// The gathered inputs for one diagnostics bundle (DIAG-03).
///
/// `calibration` is the active profile serialized as JSON, or `None` when no
/// calibration result exists (the bundle records `calibration: none` rather than
/// omitting the section). `debug` is the redacted debug inspector payload
/// ([`redacted_debug_json`]), or `None` when no report was retained.
#[derive(Debug, Clone)]
pub struct BundleInputs {
    /// The structured log records, oldest first.
    pub logs: Vec<LogRecord>,
    /// The probed system information (GPU/backend/driver, encoders, devices).
    pub system: SystemInfoView,
    /// The active calibration profile as JSON, or `None`.
    pub calibration: Option<String>,
    /// The redacted debug inspector payload as JSON, or `None`.
    pub debug: Option<String>,
    /// The operator's home directory, used by [`redact`].
    pub home: PathBuf,
}

/// The result of a successful [`write_bundle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleSummary {
    /// The path the bundle was written to.
    pub path: PathBuf,
    /// How many entries the zip contains.
    pub files: usize,
}

/// A typed diagnostics-bundle failure (DIAG-03).
///
/// The inner string is a plain-language cause; the caller maps it to a typed
/// [`WorkerError`](crate::events::WorkerError). No partial bundle is left at
/// the target path on any variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiagnosticsError {
    /// The bundle could not be written to disk.
    #[error("cannot write the diagnostics bundle: {0}")]
    Io(String),
    /// The zip archive could not be encoded.
    #[error("cannot encode the diagnostics bundle: {0}")]
    Zip(String),
}

/// The operator's home directory (the redaction anchor).
///
/// Reads `HOME` (Unix) then `USERPROFILE` (Windows). An empty path means the
/// environment did not expose one; [`redact`] then leaves text untouched rather
/// than guessing.
#[must_use]
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Redact a text entry for the bundle (T-05-17).
///
/// Replaces the home directory with `<home>` and the account name (the home's
/// last path component) with `<user>`. Everything else is left untouched.
///
/// Both anchors are matched in a single left-to-right pass over the **original**
/// text, so the inserted `<home>` placeholder can never be rewritten — even when
/// the home directory's basename is literally `home` (IN-04). The account name
/// is only replaced at a token boundary, so a short name (`dev`, `test`) does
/// not mangle unrelated text such as `device`.
#[must_use]
pub fn redact(text: &str, home: &Path) -> String {
    let home_str = home.to_string_lossy();
    if home_str.is_empty() {
        return text.to_string();
    }
    let user = home
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty());

    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with(home_str.as_ref()) {
            out.push_str("<home>");
            i += home_str.len();
            continue;
        }
        if let Some(user) = user
            && text[i..].starts_with(user)
            && token_boundary(text, i, user.len())
        {
            out.push_str("<user>");
            i += user.len();
            continue;
        }
        let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Whether the `[start, start + len)` span is a standalone token: not preceded
/// or followed by an identifier character, so a short account name does not
/// rewrite unrelated text (e.g. `dev` inside `device`) (IN-04).
fn token_boundary(text: &str, start: usize, len: usize) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let before = text[..start].chars().next_back();
    let after = text[start + len..].chars().next();
    !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
}

/// Serialize the debug inspector payload for the bundle, without media bytes.
///
/// The report's `left_thumb`/`right_thumb` are raw RGBA frames — media bytes the
/// bundle must never carry. They are dropped (their dimensions are kept so the
/// reader knows a frame pair existed); the normalized match points and the
/// per-frame count rows are retained.
#[must_use]
pub fn redacted_debug_json(report: &DebugReport) -> String {
    let verified = &report.verified;
    let rejected = &report.rejected;
    let per_frame = &report.per_frame;
    let value = serde_json::json!({
        "frame_index": report.frame_index,
        "frames_total": report.frames_total,
        "left_width": report.left_width,
        "left_height": report.left_height,
        "right_width": report.right_width,
        "right_height": report.right_height,
        "thumbnails_omitted": true,
        "verified": verified,
        "rejected": rejected,
        "residual_error": report.residual_error,
        "per_frame": per_frame,
        "points_capped": report.points_capped,
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
}

/// Write a redacted, local-only diagnostics bundle to `path` (DIAG-03).
///
/// Entries: `logs.jsonl` (one redacted JSON record per line), `system-info.json`,
/// `calibration.json` (the active profile, or the `calibration: none` marker when
/// absent), and `debug.json` when a debug report was retained. The write is
/// atomic (temp + rename), so a failure leaves no partial bundle (T-05-19).
///
/// # Errors
///
/// [`DiagnosticsError::Io`] when the temp file or the final rename fails;
/// [`DiagnosticsError::Zip`] when the archive cannot be encoded.
pub fn write_bundle(path: &Path, inputs: &BundleInputs) -> Result<BundleSummary, DiagnosticsError> {
    let tmp = temp_sibling(path);
    match write_entries(&tmp, inputs) {
        Ok(files) => match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(BundleSummary {
                path: path.to_path_buf(),
                files,
            }),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(DiagnosticsError::Io(e.to_string()))
            }
        },
        Err(e) => {
            // A failure never leaves the temp archive behind (no partial bundle).
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Write the zip entries into `tmp`, returning the entry count.
fn write_entries(tmp: &Path, inputs: &BundleInputs) -> Result<usize, DiagnosticsError> {
    let file = std::fs::File::create(tmp).map_err(|e| DiagnosticsError::Io(e.to_string()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let home = inputs.home.as_path();
    let mut files = 0usize;

    // logs.jsonl — one JSON record per line, each line redacted.
    let mut logs = String::new();
    for record in &inputs.logs {
        let line =
            serde_json::to_string(record).map_err(|e| DiagnosticsError::Io(e.to_string()))?;
        logs.push_str(&redact(&line, home));
        logs.push('\n');
    }
    add_entry(&mut zip, "logs.jsonl", &logs, options)?;
    files += 1;

    // system-info.json — the probed system view.
    let system = serde_json::to_string_pretty(&inputs.system)
        .map_err(|e| DiagnosticsError::Io(e.to_string()))?;
    add_entry(
        &mut zip,
        "system-info.json",
        &redact(&system, home),
        options,
    )?;
    files += 1;

    // calibration.json — the active profile, or an explicit "none" marker so the
    // section is never silently omitted.
    let calibration = match &inputs.calibration {
        Some(json) => redact(json, home),
        None => "calibration: none\n".to_string(),
    };
    add_entry(&mut zip, "calibration.json", &calibration, options)?;
    files += 1;

    // debug.json — only when a debug inspector payload was retained.
    if let Some(debug) = &inputs.debug {
        add_entry(&mut zip, "debug.json", &redact(debug, home), options)?;
        files += 1;
    }

    zip.finish()
        .map_err(|e| DiagnosticsError::Zip(e.to_string()))?;
    Ok(files)
}

/// Append one text entry to the archive.
fn add_entry<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    name: &str,
    body: &str,
    options: zip::write::SimpleFileOptions,
) -> Result<(), DiagnosticsError> {
    zip.start_file(name, options)
        .map_err(|e| DiagnosticsError::Zip(e.to_string()))?;
    zip.write_all(body.as_bytes())
        .map_err(|e| DiagnosticsError::Io(e.to_string()))?;
    Ok(())
}

/// The sibling temp path a bundle is written to before the atomic rename.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|| std::ffi::OsString::from("diagnostics.zip"));
    name.push(".tmp");
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::events::{DebugPoint, FrameMatchRow};

    /// A unique, created temp directory for one test.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("reco-diag-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_system() -> SystemInfoView {
        SystemInfoView {
            gpu_name: Some("NVIDIA RTX".to_string()),
            backend: Some("Vulkan".to_string()),
            driver: Some("555.42".to_string()),
            encoders: Vec::new(),
            devices: vec!["/dev/video0".to_string()],
            devices_note: None,
        }
    }

    fn sample_debug() -> DebugReport {
        DebugReport {
            frame_index: 3,
            frames_total: 10,
            left_width: 2,
            left_height: 2,
            right_width: 2,
            right_height: 2,
            // Media bytes that must never reach the bundle.
            left_thumb: vec![1, 2, 3, 4, 5, 6, 7, 8],
            right_thumb: vec![9, 10, 11, 12, 13, 14, 15, 16],
            verified: vec![DebugPoint {
                x_nx: 0.25,
                y_nx: 0.5,
                error: 0.1,
            }],
            rejected: Vec::new(),
            residual_error: 0.42,
            per_frame: vec![FrameMatchRow {
                frame: 0,
                keypoints_left: 100,
                keypoints_right: 90,
                post_ratio_test: 40,
                post_spatial_filter: 30,
                post_ransac: 25,
            }],
            points_capped: false,
        }
    }

    #[test]
    fn redact_replaces_home_and_user_name() {
        let home = PathBuf::from("/home/alice");
        let text = "/home/alice/clips/left.mp4 written by alice";
        let redacted = redact(text, &home);
        assert!(
            redacted.contains("<home>/clips/left.mp4"),
            "the home directory must become <home>: {redacted}"
        );
        assert!(
            redacted.contains("<user>"),
            "the account name must become <user>: {redacted}"
        );
        assert!(
            !redacted.contains("alice"),
            "the account name must not survive: {redacted}"
        );
        assert!(
            !redacted.contains("/home/alice"),
            "the home path must not survive: {redacted}"
        );
    }

    #[test]
    fn redact_with_no_home_is_a_noop() {
        let text = "plain text with no paths";
        assert_eq!(redact(text, Path::new("")), text);
    }

    #[test]
    fn redact_does_not_corrupt_the_home_placeholder() {
        // IN-04: with a home basename of `home`, the inserted `<home>` must not
        // be rewritten by the account-name pass.
        let home = PathBuf::from("/home/home");
        assert_eq!(
            redact("/home/home/clips/left.mp4", &home),
            "<home>/clips/left.mp4"
        );
    }

    #[test]
    fn redact_does_not_over_redact_a_short_user_name() {
        // IN-04: `dev` must not rewrite `device`.
        let home = PathBuf::from("/home/dev");
        let redacted = redact("device dev /home/dev/clip.mp4", &home);
        assert!(
            redacted.contains("device"),
            "an unrelated identifier must survive: {redacted}"
        );
        assert!(
            redacted.contains("<user>"),
            "the standalone account name must be redacted: {redacted}"
        );
        assert!(
            redacted.contains("<home>/clip.mp4"),
            "the home path must be redacted: {redacted}"
        );
    }

    #[test]
    fn bundle_roundtrips_with_entries_and_redaction() {
        let dir = temp_dir("roundtrip");
        // The temp dir stands in for the operator's home so redaction is real.
        let home = dir.clone();
        let path = dir.join("reco-diagnostics.zip");
        let inputs = BundleInputs {
            logs: vec![LogRecord {
                level: "info".to_string(),
                target: "reco_app::worker".to_string(),
                message: format!("loaded {}", home.join("clips/left.mp4").display()),
                timestamp_ms: 1_700_000_000_000,
            }],
            system: sample_system(),
            calibration: Some("{\"left\":\"calibrated\"}".to_string()),
            debug: Some(redacted_debug_json(&sample_debug())),
            home: home.clone(),
        };

        let summary = write_bundle(&path, &inputs).expect("the bundle must write");
        assert_eq!(summary.files, 4, "logs + system + calibration + debug");
        assert_eq!(summary.path, path);
        assert!(path.exists(), "the bundle must exist at the target path");
        assert!(
            !temp_sibling(&path).exists(),
            "the temp file must be gone after the rename"
        );

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "calibration.json",
                "debug.json",
                "logs.jsonl",
                "system-info.json"
            ]
        );

        let mut logs = String::new();
        archive
            .by_name("logs.jsonl")
            .unwrap()
            .read_to_string(&mut logs)
            .unwrap();
        assert!(
            logs.contains("<home>/clips/left.mp4"),
            "the log line must be redacted: {logs}"
        );
        let user = home.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !logs.contains(&user),
            "the account name must not survive in the bundle: {logs}"
        );

        let mut system = String::new();
        archive
            .by_name("system-info.json")
            .unwrap()
            .read_to_string(&mut system)
            .unwrap();
        assert!(
            system.contains("NVIDIA RTX"),
            "the system info must ride the bundle: {system}"
        );

        // The debug entry must not carry the thumbnail media bytes.
        let mut debug = String::new();
        archive
            .by_name("debug.json")
            .unwrap()
            .read_to_string(&mut debug)
            .unwrap();
        assert!(
            debug.contains("\"thumbnails_omitted\": true"),
            "the debug payload must record that thumbnails were dropped: {debug}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bundle_without_a_calibration_records_none_rather_than_omitting_it() {
        let dir = temp_dir("nocal");
        let path = dir.join("reco-diagnostics.zip");
        let inputs = BundleInputs {
            logs: Vec::new(),
            system: sample_system(),
            calibration: None,
            debug: None,
            home: dir.clone(),
        };
        let summary = write_bundle(&path, &inputs).expect("the bundle must write");
        assert_eq!(summary.files, 3, "no debug entry without a report");

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut calibration = String::new();
        archive
            .by_name("calibration.json")
            .unwrap()
            .read_to_string(&mut calibration)
            .unwrap();
        assert!(
            calibration.contains("calibration: none"),
            "an absent calibration must be recorded, not omitted: {calibration}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bundle_with_no_logs_still_writes() {
        let dir = temp_dir("nologs");
        let path = dir.join("reco-diagnostics.zip");
        let inputs = BundleInputs {
            logs: Vec::new(),
            system: sample_system(),
            calibration: Some("{}".to_string()),
            debug: None,
            home: dir.clone(),
        };
        let summary = write_bundle(&path, &inputs).expect("an empty log set must still write");
        assert_eq!(summary.files, 3);

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut logs = String::new();
        archive
            .by_name("logs.jsonl")
            .unwrap()
            .read_to_string(&mut logs)
            .unwrap();
        assert!(logs.is_empty(), "an empty log set writes an empty entry");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn redacted_debug_json_drops_thumbnail_bytes() {
        let json = redacted_debug_json(&sample_debug());
        assert!(
            json.contains("\"thumbnails_omitted\": true"),
            "the payload must declare the thumbnails were dropped: {json}"
        );
        assert!(
            json.contains("\"verified\""),
            "the normalized match points are retained: {json}"
        );
        assert!(
            json.contains("\"per_frame\""),
            "the per-frame count rows are retained: {json}"
        );
        // The raw byte vectors are gone; only their dimensions survive.
        assert!(
            !json.contains("\"left_thumb\""),
            "the raw thumbnail bytes must not be serialized: {json}"
        );
    }

    #[test]
    fn log_buffer_caps_and_snapshots() {
        let buffer = LogBuffer::new();
        for i in 0..(MAX_LOG_RECORDS + 5) {
            buffer.push(LogRecord {
                level: "info".to_string(),
                target: "t".to_string(),
                message: format!("record {i}"),
                timestamp_ms: i as u64,
            });
        }
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.len(), MAX_LOG_RECORDS, "the buffer is bounded");
        // The oldest five were evicted, so the snapshot starts at record 5.
        assert_eq!(snapshot.first().unwrap().message, "record 5");
        assert_eq!(
            snapshot.last().unwrap().message,
            format!("record {}", MAX_LOG_RECORDS + 4)
        );
    }

    #[test]
    fn write_bundle_failure_leaves_no_partial_bundle() {
        let dir = temp_dir("unwritable");
        // A target whose parent does not exist cannot be created or renamed.
        let path = dir.join("missing-subdir").join("reco-diagnostics.zip");
        let inputs = BundleInputs {
            logs: Vec::new(),
            system: sample_system(),
            calibration: None,
            debug: None,
            home: dir.clone(),
        };
        let err = write_bundle(&path, &inputs).expect_err("an unwritable path must fail");
        assert!(
            matches!(err, DiagnosticsError::Io(_)),
            "an unwritable path is an Io error: {err:?}"
        );
        assert!(!path.exists(), "no partial bundle may be left behind");
        std::fs::remove_dir_all(&dir).ok();
    }
}
