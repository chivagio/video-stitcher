//! The shared untrusted-path policy for FFmpeg-bound paths (T-03-01).
//!
//! Any path the app hands to FFmpeg must be a **local file**, never a URL or an
//! FFmpeg protocol (`http://`, `https://`, `concat:`, `pipe:`, `data:`). This is
//! the explicit T-03-01 boundary: rejecting the protocol prefixes here stops a
//! crafted string from making FFmpeg issue an outbound open or read an arbitrary
//! protocol source.
//!
//! The policy is applied in two places:
//!
//! * the Tauri command boundary (`crate::commands::validate_profile_path`), for
//!   operator-supplied paths; and
//! * [`crate::project::RecoProject::validate`], for every path a `.reco` manifest
//!   supplies — inputs, the calibration path, and the export output directory.
//!
//! It lives in one module so the two call sites cannot drift: a `.reco` file is
//! untrusted input (T-05-14), and projects are the kind of artifact users share,
//! so the manifest paths must not bypass the guard the command surface enforces
//! (CR-01).

/// The FFmpeg protocol prefixes that name a non-local source. Compared
/// case-insensitively against the trimmed path.
const FORBIDDEN_PREFIXES: &[&str] = &["http://", "https://", "concat:", "pipe:", "data:"];

/// Why a path is rejected by the local-file policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathViolation {
    /// The path is empty or only whitespace.
    Empty,
    /// The path names an FFmpeg protocol / URL rather than a local file.
    NonLocal,
}

impl PathViolation {
    /// The operator-facing reason, shared by both error surfaces so their text
    /// cannot drift.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            PathViolation::Empty => "path must not be empty",
            PathViolation::NonLocal => "path must be a local file, not a URL or ffmpeg protocol",
        }
    }
}

/// Classify `path` against the local-file policy (T-03-01).
///
/// Returns `None` when `path` is an acceptable local path, or the reason it is
/// rejected. Pure, so it is unit-testable without a Tauri `State`.
#[must_use]
pub fn classify(path: &str) -> Option<PathViolation> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Some(PathViolation::Empty);
    }
    let lower = trimmed.to_ascii_lowercase();
    if FORBIDDEN_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return Some(PathViolation::NonLocal);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_paths_are_accepted() {
        assert_eq!(classify("/home/op/clips/left.mp4"), None);
        assert_eq!(classify("C:\\media\\left.mp4"), None);
        assert_eq!(classify("  ./relative/left.mp4  "), None);
    }

    #[test]
    fn empty_and_protocol_paths_are_rejected() {
        assert_eq!(classify(""), Some(PathViolation::Empty));
        assert_eq!(classify("   "), Some(PathViolation::Empty));
        assert_eq!(
            classify("http://attacker/left.mp4"),
            Some(PathViolation::NonLocal)
        );
        assert_eq!(
            classify("HTTPS://attacker/left.mp4"),
            Some(PathViolation::NonLocal)
        );
        assert_eq!(classify("concat:a|b"), Some(PathViolation::NonLocal));
        assert_eq!(classify("pipe:0"), Some(PathViolation::NonLocal));
        assert_eq!(classify("data:abc"), Some(PathViolation::NonLocal));
    }
}
