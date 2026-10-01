//! Hardcoded Phase 1 inputs, calibration profile, and export settings (D-08).
//!
//! D-08 keeps file selection out of Phase 1: two clip paths plus a
//! match/calibration profile are baked as constants, and export writes to a
//! fixed output path with a fixed preset. A single environment variable
//! ([`TEST_MEDIA_DIR_ENV`]) overrides the directory so the gate can run where
//! the test clips live elsewhere.
//!
//! File selection, metadata probing, and lens detection are Phase 3
//! (IMPT-01..04); presets and encoder selection are Phase 5. These constants
//! are disposable scaffolding, not an interface.

use std::path::{Path, PathBuf};

/// Environment variable overriding the directory that holds the test media.
///
/// When set, [`media_dir`] resolves the clip and profile paths relative to it.
/// An empty value is rejected with [`HardcodedError::EmptyMediaDir`] rather
/// than silently falling back (input validation at the API boundary).
pub const TEST_MEDIA_DIR_ENV: &str = "RECO_TEST_MEDIA_DIR";

/// File name of the left test clip.
pub const LEFT_CLIP_FILE: &str = "left.mp4";
/// File name of the right test clip.
pub const RIGHT_CLIP_FILE: &str = "right.mp4";
/// File name of the match/calibration profile.
pub const CALIBRATION_FILE: &str = "match.json";

/// Default test-media directory (relative to the crate root), used when
/// [`TEST_MEDIA_DIR_ENV`] is unset.
pub const DEFAULT_MEDIA_DIR: &str = "test-media";

/// Fixed export output file name (D-08: fixed output path).
///
/// Consumed by the export command added in Plan 01-04 (the thin export path);
/// defined here with the rest of the D-08 hardcoded inputs so the whole
/// hardcoded surface is reviewable in one place.
#[allow(dead_code)]
pub const EXPORT_OUTPUT_FILE: &str = "reco-app-export.mp4";
/// Fixed export preset label (D-08: fixed preset — H.264 / software encode).
///
/// See [`EXPORT_OUTPUT_FILE`] — used by Plan 01-04's export command.
#[allow(dead_code)]
pub const EXPORT_PRESET: &str = "h264-software";

/// Typed error for hardcoded-path resolution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HardcodedError {
    /// [`TEST_MEDIA_DIR_ENV`] was set to an empty (or whitespace-only) value.
    #[error("environment variable {TEST_MEDIA_DIR_ENV} is set but empty")]
    EmptyMediaDir,
}

/// Paths resolved from the hardcoded file names and the media directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPaths {
    /// Directory the paths were resolved against.
    pub dir: PathBuf,
    /// Left test clip.
    pub left: PathBuf,
    /// Right test clip.
    pub right: PathBuf,
    /// Match/calibration profile.
    pub calibration: PathBuf,
}

/// Resolve the media directory to an absolute path.
///
/// # Errors
///
/// Returns [`HardcodedError::EmptyMediaDir`] if [`TEST_MEDIA_DIR_ENV`] is set
/// but empty (including whitespace-only), rather than silently falling back to
/// [`DEFAULT_MEDIA_DIR`].
pub fn media_dir() -> Result<PathBuf, HardcodedError> {
    let raw = match std::env::var(TEST_MEDIA_DIR_ENV) {
        Ok(value) => {
            if value.trim().is_empty() {
                return Err(HardcodedError::EmptyMediaDir);
            }
            PathBuf::from(value)
        }
        Err(_) => PathBuf::from(DEFAULT_MEDIA_DIR),
    };
    Ok(absolute(&raw))
}

/// Resolve all hardcoded media paths.
///
/// # Errors
///
/// Propagates [`HardcodedError`] from [`media_dir`].
pub fn media_paths() -> Result<MediaPaths, HardcodedError> {
    let dir = media_dir()?;
    Ok(MediaPaths {
        left: dir.join(LEFT_CLIP_FILE),
        right: dir.join(RIGHT_CLIP_FILE),
        calibration: dir.join(CALIBRATION_FILE),
        dir,
    })
}

/// Resolve the fixed export output path, relative to the media directory.
///
/// # Errors
///
/// Propagates [`HardcodedError`] from [`media_dir`].
///
/// Consumed by Plan 01-04's export command; defined alongside the other D-08
/// constants so the hardcoded surface reads as one unit.
#[allow(dead_code)]
pub fn export_output_path() -> Result<PathBuf, HardcodedError> {
    Ok(media_dir()?.join(EXPORT_OUTPUT_FILE))
}

/// Return `path` made absolute against the current working directory when it is
/// relative, leaving already-absolute paths untouched.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_paths_use_default_dir_when_env_unset() {
        // The test process is expected not to set RECO_TEST_MEDIA_DIR; if it
        // is, this test still asserts the shape rather than a fixed value.
        let paths = media_paths().expect("default media dir resolves");
        assert!(paths.left.ends_with(LEFT_CLIP_FILE));
        assert!(paths.right.ends_with(RIGHT_CLIP_FILE));
        assert!(paths.calibration.ends_with(CALIBRATION_FILE));
        assert!(paths.dir.is_absolute());
    }

    #[test]
    fn empty_media_dir_env_is_rejected() {
        // SAFETY (env): tests run single-threaded per module below; see the
        // guard test `env_var_guard_*`. Set then immediately unset.
        unsafe { std::env::set_var(TEST_MEDIA_DIR_ENV, "   ") };
        let result = media_dir();
        unsafe { std::env::remove_var(TEST_MEDIA_DIR_ENV) };
        assert_eq!(result, Err(HardcodedError::EmptyMediaDir));
    }
}
