//! Runtime prerequisite preflight (DIAG-05).
//!
//! [`run_preflight`] verifies the shipped runtime prerequisites — the FFmpeg
//! shared libraries, ONNX Runtime (when detection is built), and the webview
//! runtime — and reports each as a typed [`PreflightItem`] with an actionable
//! remediation string. The System Info panel renders the report; a failing item
//! always carries a remediation (DIAG-05 prohibition: never a bare failure).
//!
//! The FFmpeg and ONNX Runtime items are probed **directly** (encoder
//! enumeration, ORT provider registration), never hardcoded to pass (T-05-13).
//! The webview item is established by the fact the app is already running inside
//! that webview — a direct observation of the running process, not a probe
//! (IN-03) — and its `detail` says so. The documented prerequisite matrix lives
//! in `RUNTIME-PREREQUISITES.md`, which every remediation points at.

use crate::events::EncoderView;

/// Where a remediation sends the operator for the full install instructions.
pub const PREREQUISITES_DOC: &str = "See RUNTIME-PREREQUISITES.md for install instructions.";

/// One checked runtime prerequisite (DIAG-05).
///
/// `ok` is the pass/fail verdict; `detail` is the observed evidence; and
/// `remediation` is the actionable next step, always non-empty when `ok` is
/// `false` (DIAG-05 prohibition). Mirrors into `ui/src/lib/types.ts`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreflightItem {
    /// Stable identifier (`"ffmpeg"` / `"onnxruntime"` / `"webview"`).
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Whether the prerequisite is satisfied.
    pub ok: bool,
    /// The observed evidence (or the failure reason).
    pub detail: String,
    /// The actionable next step; non-empty whenever `ok` is `false`.
    pub remediation: String,
}

/// The full preflight verdict (DIAG-05).
///
/// `all_ok` is the conjunction of the items; the panel states it plainly
/// (UI-SPEC Copywriting Contract: "Runtime prerequisites OK" / "Runtime
/// prerequisites incomplete").
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreflightReport {
    /// The per-prerequisite rows, in a stable order.
    pub items: Vec<PreflightItem>,
    /// Whether every prerequisite passed.
    pub all_ok: bool,
}

/// Run the runtime preflight (DIAG-05).
#[must_use]
pub fn run_preflight() -> PreflightReport {
    let items = vec![
        ffmpeg_item(&crate::system::probe_encoders()),
        onnxruntime_item(probe_ort()),
        webview_item(),
    ];
    let all_ok = items.iter().all(|item| item.ok);
    PreflightReport { items, all_ok }
}

/// The FFmpeg prerequisite: the shared libraries must be loadable and their
/// H.264 encoders enumerable.
fn ffmpeg_item(encoders: &[EncoderView]) -> PreflightItem {
    let ok = !encoders.is_empty();
    let detail = if ok {
        format!(
            "{} H.264 encoder(s) available (auto: {})",
            encoders.len(),
            encoders.first().map(|e| e.name.as_str()).unwrap_or("none")
        )
    } else {
        "FFmpeg shared libraries could not enumerate any H.264 encoder".to_string()
    };
    PreflightItem {
        id: "ffmpeg".to_string(),
        label: "FFmpeg".to_string(),
        ok,
        detail,
        remediation: if ok {
            String::new()
        } else {
            format!(
                "Install the FFmpeg shared libraries (e.g. `sudo apt install ffmpeg` on \
                 Debian/Ubuntu). {PREREQUISITES_DOC}"
            )
        },
    }
}

/// The ONNX Runtime prerequisite state, probed directly (T-05-13).
enum OrtProbe {
    /// Detection is not compiled into this build — the item is not required.
    ///
    /// Constructed only when `feature = "ort"` is off; the `ort` build keeps
    /// the variant for the (always-compiled) match arm, hence the scoped allow.
    #[cfg_attr(feature = "ort", allow(dead_code))]
    NotBuilt,
    /// Detection is built and ONNX Runtime initialised; the best provider.
    Available { best: String },
    /// Detection is built but ONNX Runtime could not initialise.
    Unavailable { reason: String },
}

/// Probe ONNX Runtime's availability (only meaningful when detection is built).
fn probe_ort() -> OrtProbe {
    #[cfg(feature = "ort")]
    {
        // A genuine runtime probe: it registers the compiled-in execution
        // providers on a throwaway session builder, so a build whose ORT dylib
        // is missing reports unavailable rather than a hardcoded pass.
        let result = reco_detect::probe_execution_providers();
        if result.is_available() {
            OrtProbe::Available {
                best: result.best_provider().to_string(),
            }
        } else {
            OrtProbe::Unavailable {
                reason: if result.errors.is_empty() {
                    "ONNX Runtime is not available at runtime".to_string()
                } else {
                    result.errors.join("; ")
                },
            }
        }
    }
    #[cfg(not(feature = "ort"))]
    {
        OrtProbe::NotBuilt
    }
}

/// Map the ONNX Runtime probe into its preflight row.
fn onnxruntime_item(probe: OrtProbe) -> PreflightItem {
    let (ok, detail, remediation) = match probe {
        OrtProbe::NotBuilt => (
            true,
            "not required in this build".to_string(),
            String::new(),
        ),
        OrtProbe::Available { best } => (
            true,
            format!("ONNX Runtime available (best provider: {best})"),
            String::new(),
        ),
        OrtProbe::Unavailable { reason } => (
            false,
            reason,
            format!(
                "Install ONNX Runtime and place its library next to the app binary. \
                 {PREREQUISITES_DOC}"
            ),
        ),
    };
    PreflightItem {
        id: "onnxruntime".to_string(),
        label: "ONNX Runtime".to_string(),
        ok,
        detail,
        remediation,
    }
}

/// The webview-runtime prerequisite (DIAG-05).
///
/// The app is running inside the webview, so its presence is directly observed
/// rather than assumed; the row names the platform backend and points AppImage
/// users at the manual install (`RUNTIME-PREREQUISITES.md`). This is an
/// observation of the running process, not a probed check (IN-03): the `detail`
/// states the app is running inside the named backend.
fn webview_item() -> PreflightItem {
    #[cfg(target_os = "windows")]
    let backend = "WebView2";
    #[cfg(target_os = "macos")]
    let backend = "WKWebView";
    #[cfg(all(unix, not(target_os = "macos")))]
    let backend = "webkit2gtk-4.1";

    PreflightItem {
        id: "webview".to_string(),
        label: "Webview runtime".to_string(),
        ok: true,
        detail: format!("{backend} (the app is running inside it)"),
        remediation: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoder(name: &str, is_hardware: bool) -> EncoderView {
        EncoderView {
            name: name.to_string(),
            description: name.to_string(),
            is_hardware,
        }
    }

    #[test]
    fn ffmpeg_failure_carries_a_non_empty_remediation() {
        // DIAG-05 prohibition: a failing prerequisite never reports a bare
        // failure. No enumerable encoders => a failure with an actionable fix.
        let item = ffmpeg_item(&[]);
        assert!(!item.ok);
        assert!(
            !item.remediation.is_empty(),
            "a failing preflight item must carry a remediation"
        );
        assert!(item.remediation.contains("RUNTIME-PREREQUISITES"));
    }

    #[test]
    fn ffmpeg_success_names_the_auto_encoder() {
        let item = ffmpeg_item(&[encoder("h264_nvenc", true), encoder("libx264", false)]);
        assert!(item.ok);
        assert!(
            item.detail.contains("h264_nvenc"),
            "detail: {}",
            item.detail
        );
    }

    #[test]
    fn onnxruntime_not_built_is_not_a_failure() {
        // DIAG-05 edge probe "detection not built": the ORT item is reported as
        // "not required" rather than a failure.
        let item = onnxruntime_item(OrtProbe::NotBuilt);
        assert!(item.ok);
        assert_eq!(item.detail, "not required in this build");
    }

    #[test]
    fn onnxruntime_unavailable_carries_a_remediation() {
        let item = onnxruntime_item(OrtProbe::Unavailable {
            reason: "library not found".to_string(),
        });
        assert!(!item.ok);
        assert_eq!(item.detail, "library not found");
        assert!(
            !item.remediation.is_empty(),
            "a failing preflight item must carry a remediation"
        );
    }

    #[test]
    fn every_failing_item_carries_a_remediation_and_all_ok_is_the_conjunction() {
        let report = run_preflight();
        assert_eq!(
            report.items.len(),
            3,
            "the three DIAG-05 prerequisites must be reported"
        );
        for item in &report.items {
            if !item.ok {
                assert!(
                    !item.remediation.is_empty(),
                    "item `{}` failed without a remediation",
                    item.id
                );
            }
        }
        assert_eq!(report.all_ok, report.items.iter().all(|i| i.ok));
    }
}
