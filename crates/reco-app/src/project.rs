//! The `.reco` project manifest (PROJ-01).
//!
//! A `.reco` file is a **versioned, non-destructive JSON manifest**. It
//! *references* the two input paths, the per-input lens overrides, the
//! calibration, the pose, and the export settings. Media is never copied into
//! the project — the manifest holds paths only (prohibition).
//!
//! # Calibration: path vs. inline
//!
//! A calibration is referenced two ways:
//!
//! * [`RecoProject::calibration_path`] — the `.json` a profile was loaded from or
//!   saved to, when the operator has one. This is the human-readable reference.
//! * [`RecoProject::calibration`] — an **inline snapshot** of the current
//!   `MatchCalibration`. A fresh calibration result that was never written to a
//!   `.json` has no path, so without the snapshot reopening the project could not
//!   restore it. The snapshot is small (a few hundred bytes) and is not media, so
//!   inlining it is non-destructive. On open the snapshot wins when present; the
//!   path is the fallback.
//!
//! # Versioning
//!
//! [`PROJECT_VERSION`] gates forward-compatibility. Opening a manifest whose
//! `version` exceeds the supported one returns a typed
//! [`ProjectError::UnsupportedVersion`] rather than a partial restore; a manifest
//! that is not valid JSON returns [`ProjectError::Parse`].

use std::path::{Path, PathBuf};

use crate::events::{ExportSettings, InputRole, LensCandidate};

/// The `.reco` manifest schema version (PROJ-01).
///
/// Opening rejects a manifest whose `version` is **greater** than this with a
/// typed [`ProjectError::UnsupportedVersion`] — never a partial restore.
pub const PROJECT_VERSION: u32 = 1;

/// Upper bound on a `.reco` file we will read (2 MiB), so a pathological file
/// cannot drive an unbounded parse (T-05-16).
const MAX_PROJECT_FILE_SIZE: u64 = 2 * 1024 * 1024;

/// One referenced camera input (PROJ-01).
///
/// The `path` is a local file the manifest points at — the media itself is never
/// copied into the project.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProjectInput {
    /// The local file path the project references (never copied).
    pub path: String,
    /// The per-input lens override, or `None` for auto-detect.
    pub lens_override: Option<LensCandidate>,
}

/// The operator pose restored from a project (PROJ-01).
///
/// Mirrors the settable fields of the `Pose` event. `fov_max` is a property of
/// the clip's stitchable coverage, not operator state, so it is not stored.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PoseView {
    /// Yaw in radians.
    pub yaw: f32,
    /// Pitch in radians.
    pub pitch: f32,
    /// Vertical FOV in degrees.
    pub fov_degrees: f32,
}

impl Default for PoseView {
    /// The engine's rest pose (`PoseControl::with_defaults`).
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            fov_degrees: 75.0,
        }
    }
}

/// One referenced input that could not be found when opening (PROJ-01).
///
/// Carried by [`crate::events::WorkerEvent::ProjectMissingInputs`] so the UI can
/// offer a per-input relocate picker instead of failing the open.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MissingInput {
    /// Which camera the missing path belongs to.
    pub role: InputRole,
    /// The missing path (shown verbatim in the relocate dialog).
    pub path: String,
}

/// The `.reco` manifest (PROJ-01).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecoProject {
    /// The manifest schema version ([`PROJECT_VERSION`] when written here).
    pub version: u32,
    /// The left camera input.
    pub left: ProjectInput,
    /// The right camera input.
    pub right: ProjectInput,
    /// The `.json` the calibration was loaded from / saved to, when known.
    pub calibration_path: Option<String>,
    /// An inline calibration snapshot (see the module docs).
    pub calibration: Option<reco_core::calibration::MatchCalibration>,
    /// The operator pose.
    pub pose: PoseView,
    /// The export settings (preset/codec/quality/bitrate/resolution/trim/variant).
    pub export: ExportSettings,
}

/// A typed `.reco` failure (PROJ-01).
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// The manifest is not valid JSON, or a required field is missing/mistyped.
    #[error("project is not valid JSON: {0}")]
    Parse(String),
    /// The manifest declares a schema version this build does not support.
    #[error("unsupported project version {found} (this build supports up to {supported})")]
    UnsupportedVersion {
        /// The version found in the manifest.
        found: u32,
        /// The highest version this build supports.
        supported: u32,
    },
    /// The manifest parsed but failed semantic validation.
    #[error("invalid project: {0}")]
    Invalid(String),
    /// Reading or writing the project file failed.
    #[error("cannot read or write the project file: {0}")]
    Io(String),
}

impl RecoProject {
    /// Serialize to pretty-printed, deterministic JSON.
    ///
    /// `serde` emits struct fields in declaration order, so two projects with
    /// equal state produce byte-identical JSON — the round-trip test relies on
    /// this.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("RecoProject is always serializable")
    }

    /// Parse and validate a manifest (PROJ-01).
    ///
    /// # Errors
    ///
    /// * [`ProjectError::Parse`] when the text is not a well-formed manifest.
    /// * [`ProjectError::UnsupportedVersion`] when `version > PROJECT_VERSION`.
    /// * [`ProjectError::Invalid`] when a required field is semantically invalid.
    pub fn from_json(json: &str) -> Result<Self, ProjectError> {
        let project: Self =
            serde_json::from_str(json).map_err(|e| ProjectError::Parse(e.to_string()))?;
        if project.version > PROJECT_VERSION {
            return Err(ProjectError::UnsupportedVersion {
                found: project.version,
                supported: PROJECT_VERSION,
            });
        }
        project.validate()?;
        Ok(project)
    }

    /// Read and parse a `.reco` file, bounded by [`MAX_PROJECT_FILE_SIZE`].
    ///
    /// # Errors
    ///
    /// [`ProjectError::Io`] when the file cannot be read or is too large, plus
    /// the [`Self::from_json`] errors.
    pub fn read_from_file(path: &Path) -> Result<Self, ProjectError> {
        use std::io::Read;

        let file = std::fs::File::open(path).map_err(|e| ProjectError::Io(e.to_string()))?;
        let mut json = String::new();
        file.take(MAX_PROJECT_FILE_SIZE + 1)
            .read_to_string(&mut json)
            .map_err(|e| ProjectError::Io(e.to_string()))?;
        if json.len() as u64 > MAX_PROJECT_FILE_SIZE {
            return Err(ProjectError::Io(format!(
                "project file exceeds the {MAX_PROJECT_FILE_SIZE}-byte limit"
            )));
        }
        Self::from_json(&json)
    }

    /// Write the manifest atomically.
    ///
    /// Writes to a sibling `<name>.tmp` and renames it over the target, so a
    /// failure mid-write never leaves a partial `.reco` at the target path
    /// (PROJ-01 edge probe "unwritable path": a typed error, no partial file).
    ///
    /// # Errors
    ///
    /// [`ProjectError::Io`] when the write or rename fails.
    pub fn write_to_file(&self, path: &Path) -> Result<(), ProjectError> {
        let json = self.to_json();
        let tmp = tmp_path(path);
        std::fs::write(&tmp, json.as_bytes()).map_err(|e| ProjectError::Io(e.to_string()))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            // Never leave the temp file behind on a failed rename.
            let _ = std::fs::remove_file(&tmp);
            return Err(ProjectError::Io(e.to_string()));
        }
        Ok(())
    }

    /// Semantic validation (PROJ-01).
    ///
    /// Checks the version is positive, both input paths are non-empty, and the
    /// export settings are structurally usable (non-zero resolution, non-empty
    /// codec/quality). An unknown codec/quality *name* is **not** a failure: the
    /// worker parses it with a documented default-on-unknown (T-05-01).
    ///
    /// # Errors
    ///
    /// [`ProjectError::Invalid`] describing the first invalid field.
    pub fn validate(&self) -> Result<(), ProjectError> {
        if self.version == 0 {
            return Err(ProjectError::Invalid(
                "version must be at least 1".to_string(),
            ));
        }
        // Every path the manifest supplies is untrusted input (T-05-14) and must
        // satisfy the same local-file policy the command boundary applies, or a
        // crafted manifest could smuggle an FFmpeg protocol/URL past the guard
        // into `probe_video`, `MatchCalibration::from_file`, and the export
        // output directory (CR-01 / T-03-01).
        ensure_local_path(&self.left.path, "left input path")?;
        ensure_local_path(&self.right.path, "right input path")?;
        if let Some(calibration_path) = &self.calibration_path {
            ensure_local_path(calibration_path, "calibration path")?;
        }
        if let Some(output_dir) = &self.export.output_dir {
            ensure_local_path(output_dir, "export output directory")?;
        }
        validate_export(&self.export)?;
        Ok(())
    }

    /// The referenced inputs that `exists` reports as missing (PROJ-01).
    ///
    /// Pure and unit-testable: the caller supplies the existence probe (the real
    /// backend probes with FFmpeg; tests pass a predicate), so the missing-input
    /// decision needs no worker or GPU.
    #[must_use]
    pub fn missing_inputs(&self, exists: impl Fn(&str) -> bool) -> Vec<MissingInput> {
        let mut missing = Vec::new();
        for (role, input) in [
            (InputRole::Left, &self.left),
            (InputRole::Right, &self.right),
        ] {
            if !exists(&input.path) {
                missing.push(MissingInput {
                    role,
                    path: input.path.clone(),
                });
            }
        }
        missing
    }

    /// Replace one input's referenced path (the relocate flow, PROJ-01).
    pub fn set_input_path(&mut self, role: InputRole, path: String) {
        match role {
            InputRole::Left => self.left.path = path,
            InputRole::Right => self.right.path = path,
        }
    }
}

/// Reject a manifest-supplied path that is not a local file (CR-01).
///
/// Reuses the shared [`crate::path_guard`] policy the command boundary applies,
/// so the two cannot drift. `field` names the offending manifest field so the
/// error is actionable.
fn ensure_local_path(path: &str, field: &str) -> Result<(), ProjectError> {
    match crate::path_guard::classify(path) {
        None => Ok(()),
        Some(violation) => Err(ProjectError::Invalid(format!(
            "{field} {}",
            violation.reason()
        ))),
    }
}

/// Validate the carried export settings (PROJ-01).
fn validate_export(export: &ExportSettings) -> Result<(), ProjectError> {
    if export.width == 0 || export.height == 0 {
        return Err(ProjectError::Invalid(
            "export resolution must be non-zero".to_string(),
        ));
    }
    if export.codec.trim().is_empty() {
        return Err(ProjectError::Invalid(
            "export codec must not be empty".to_string(),
        ));
    }
    if export.quality.trim().is_empty() {
        return Err(ProjectError::Invalid(
            "export quality must not be empty".to_string(),
        ));
    }
    Ok(())
}

/// The sibling temp path used for the atomic write (`<name>.tmp`).
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ExportPreset, ExportVariant};

    /// A fully-populated sample manifest for round-trip tests.
    fn sample() -> RecoProject {
        RecoProject {
            version: PROJECT_VERSION,
            left: ProjectInput {
                path: "/media/left.mp4".to_string(),
                lens_override: Some(LensCandidate {
                    camera: "GoPro HERO10".to_string(),
                    lens: "Wide".to_string(),
                    width: 1920,
                    height: 1080,
                }),
            },
            right: ProjectInput {
                path: "/media/right.mp4".to_string(),
                lens_override: None,
            },
            calibration_path: Some("/media/match.json".to_string()),
            calibration: None,
            pose: PoseView {
                yaw: 0.25,
                pitch: -0.1,
                fov_degrees: 80.0,
            },
            export: ExportSettings {
                preset: ExportPreset::P1080,
                width: 1920,
                height: 1080,
                codec: "h264".to_string(),
                quality: "high".to_string(),
                bitrate_kbps: Some(12_000),
                encoder_name: Some("h264_nvenc".to_string()),
                start_frame: Some(30),
                end_frame: Some(900),
                variant: ExportVariant::SideBySide,
                output_dir: Some("/media/out".to_string()),
            },
        }
    }

    #[test]
    fn project_roundtrips_through_json() {
        let project = sample();
        let json = project.to_json();
        let back = RecoProject::from_json(&json).expect("sample parses");
        // Deterministic serialization: re-serializing the parsed value yields the
        // same bytes, so every known field round-trips exactly.
        assert_eq!(back.to_json(), json);
        assert_eq!(back.version, PROJECT_VERSION);
        assert_eq!(back.left, project.left);
        assert_eq!(back.right, project.right);
        assert_eq!(back.pose, project.pose);
        assert_eq!(back.export, project.export);
        assert_eq!(back.calibration_path, project.calibration_path);
    }

    #[test]
    fn project_roundtrips_an_inline_calibration_snapshot() {
        let mut project = sample();
        project.calibration_path = None;
        project.calibration = Some(sample_calibration());
        let json = project.to_json();
        let back = RecoProject::from_json(&json).expect("inline project parses");
        assert!(back.calibration.is_some());
        assert_eq!(back.to_json(), json);
    }

    #[test]
    fn from_json_rejects_a_newer_version_without_partial_restore() {
        let mut project = sample();
        project.version = PROJECT_VERSION + 1;
        // Serialize the *raw* struct so the newer version survives to `from_json`
        // (the public `to_json` writes the struct as-is; version is data).
        let json = project.to_json();
        match RecoProject::from_json(&json) {
            Err(ProjectError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, PROJECT_VERSION + 1);
                assert_eq!(supported, PROJECT_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn from_json_rejects_malformed_json() {
        assert!(matches!(
            RecoProject::from_json("not json at all"),
            Err(ProjectError::Parse(_))
        ));
    }

    #[test]
    fn validate_rejects_empty_input_paths_and_zero_resolution() {
        let mut project = sample();
        project.left.path = "  ".to_string();
        assert!(matches!(project.validate(), Err(ProjectError::Invalid(_))));

        let mut project = sample();
        project.export.width = 0;
        assert!(matches!(project.validate(), Err(ProjectError::Invalid(_))));
    }

    #[test]
    fn from_json_rejects_non_local_manifest_paths() {
        // CR-01 / T-05-14: a crafted `.reco` must not smuggle an FFmpeg
        // protocol or URL past the command-boundary guard into `probe_video`,
        // `MatchCalibration::from_file`, or the export output directory.
        for malicious in ["http://attacker/left.mp4", "concat:/a|/b", "pipe:0"] {
            let mut project = sample();
            project.left.path = malicious.to_string();
            assert!(
                matches!(
                    RecoProject::from_json(&project.to_json()),
                    Err(ProjectError::Invalid(_))
                ),
                "left input {malicious:?} must be rejected"
            );
        }

        let mut project = sample();
        project.calibration_path = Some("https://attacker/match.json".to_string());
        assert!(matches!(
            RecoProject::from_json(&project.to_json()),
            Err(ProjectError::Invalid(_))
        ));

        let mut project = sample();
        project.export.output_dir = Some("data:application/octet-stream;base64,AAAA".to_string());
        assert!(matches!(
            RecoProject::from_json(&project.to_json()),
            Err(ProjectError::Invalid(_))
        ));
    }

    #[test]
    fn missing_inputs_reports_each_absent_path_by_role() {
        let project = sample();
        let missing = project.missing_inputs(|p| p.ends_with("left.mp4"));
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].role, InputRole::Right);
        assert_eq!(missing[0].path, "/media/right.mp4");

        assert!(project.missing_inputs(|_| true).is_empty());
    }

    #[test]
    fn write_then_read_roundtrips_on_disk() {
        let dir = std::env::temp_dir().join(format!("reco-project-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.reco");
        let project = sample();
        project.write_to_file(&path).expect("write succeeds");
        let back = RecoProject::read_from_file(&path).expect("read succeeds");
        assert_eq!(back.to_json(), project.to_json());
        // No temp file is left behind after a successful write.
        assert!(!tmp_path(&path).exists());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn set_input_path_replaces_the_role() {
        let mut project = sample();
        project.set_input_path(InputRole::Right, "/media/right2.mp4".to_string());
        assert_eq!(project.right.path, "/media/right2.mp4");
        assert_eq!(project.left.path, "/media/left.mp4");
    }

    /// A minimal valid calibration for the inline-snapshot test.
    fn sample_calibration() -> reco_core::calibration::MatchCalibration {
        let cam = |w: u32, h: u32| reco_core::calibration::CameraParams {
            width: w,
            height: h,
            fx: 1000.0,
            fy: 1000.0,
            cx: w as f64 / 2.0,
            cy: h as f64 / 2.0,
            d: [0.0; 4],
        };
        reco_core::calibration::MatchCalibration {
            left: cam(1920, 1080),
            right: cam(1920, 1080),
            layout: reco_core::calibration::PlaneLayout {
                camera_axis_offset: 0.24,
                intersect: 0.55,
                x_ty: 0.0,
                x_rz: 0.0,
                z_rx: 0.0,
                x_rx: 0.0,
                z_rz: 0.0,
            },
            rig_tilt: 0.0,
            rig_roll: 0.0,
            sync_offset: 0,
            field_roi: None,
            lens_correction_amount: 1.0,
            blend_width: 0.05,
        }
    }
}
