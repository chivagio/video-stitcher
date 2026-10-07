//! Deterministic export output naming with explicit collision handling
//! (EXPT-06).
//!
//! # Why this is a pure module
//!
//! CONTEXT locks the naming convention: `<stem>_panorama.mp4`,
//! `<stem>_sbs.mp4`, `<stem>_stacked.mp4`, with a bounded `_1` / `_2` suffix
//! when the path is already taken, and the resolved path shown to the operator.
//! The resolution is a pure function of `(dir, stem, variant)` plus the
//! filesystem's current contents, so it is unit-testable without a GPU and
//! without the worker.
//!
//! # Trust boundary (T-05-08 / T-05-09 / T-05-10)
//!
//! The webview never supplies a path: the worker derives the stem from the
//! input file name and joins it under the chosen directory. The stem is
//! sanitized so a crafted input name cannot introduce a path separator or climb
//! out of the directory ([`sanitize_stem`]). The collision loop is bounded
//! ([`MAX_COLLISION_SUFFIX`]) and fails with a typed error rather than spinning
//! ([`ExportNamingError::CollisionExhausted`]). An existing file is never
//! returned, so an export never silently overwrites a user file.

use std::path::{Path, PathBuf};

use crate::events::ExportVariant;

/// The maximum number of collision suffixes tried before giving up.
///
/// `_1` ..= `_1000`; a bounded loop keeps a pathological directory (or a bug)
/// from spinning forever (T-05-09).
pub const MAX_COLLISION_SUFFIX: u32 = 1000;

/// A failure to resolve a collision-free output path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExportNamingError {
    /// Every candidate name up to [`MAX_COLLISION_SUFFIX`] already exists.
    #[error(
        "cannot find a free output name for {stem} in {dir}: {MAX_COLLISION_SUFFIX} candidates \
         already exist"
    )]
    CollisionExhausted {
        /// The sanitized stem the candidates were built from.
        stem: String,
        /// The directory that was searched.
        dir: String,
    },
}

/// The bare, deterministic filename suffix for a variant (`panorama`, `sbs`,
/// `stacked`) — no leading separator.
///
/// Derived from [`ExportVariant::suffix`] (the single source of the convention)
/// so the two can never drift.
#[must_use]
pub fn variant_suffix(variant: ExportVariant) -> &'static str {
    variant.suffix().trim_start_matches('_')
}

/// Sanitize an input file stem for use in an output filename (T-05-08).
///
/// The stem already comes from `Path::file_stem`, so it normally has no
/// separator; this is defense-in-depth against a crafted name (`..`, a
/// separator, or control characters) escaping the chosen directory. An empty
/// result falls back to `"export"` so the output name is never degenerate.
#[must_use]
pub fn sanitize_stem(stem: &str) -> String {
    let cleaned: String = stem
        .chars()
        .map(|c| match c {
            '/' | '\\' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // A name made only of dots (`..`, `.`) would resolve to the directory
    // itself or its parent; treat it as absent.
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "export".to_string()
    } else {
        cleaned
    }
}

/// Resolve a collision-free output path: `<dir>/<stem>_<suffix>.mp4`.
///
/// Returns the base name when it is free, otherwise the first free
/// `<stem>_<suffix>_<n>.mp4` for `n` in `1..=MAX_COLLISION_SUFFIX`. An existing
/// file is never returned (prohibition: never overwrite silently); exhausting
/// the bound is a typed [`ExportNamingError::CollisionExhausted`].
///
/// # Errors
///
/// Returns [`ExportNamingError::CollisionExhausted`] when every candidate up to
/// the cap already exists.
pub fn resolve_output_path(
    dir: &Path,
    stem: &str,
    variant: ExportVariant,
) -> Result<PathBuf, ExportNamingError> {
    let stem = sanitize_stem(stem);
    let suffix = variant_suffix(variant);
    let base = dir.join(format!("{stem}_{suffix}.mp4"));
    if !base.exists() {
        return Ok(base);
    }
    for n in 1..=MAX_COLLISION_SUFFIX {
        let candidate = dir.join(format!("{stem}_{suffix}_{n}.mp4"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(ExportNamingError::CollisionExhausted {
        stem,
        dir: dir.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique, empty scratch directory under the system temp dir.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "reco_export_naming_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn variant_suffix_is_the_bare_locked_suffix() {
        assert_eq!(variant_suffix(ExportVariant::Panorama), "panorama");
        assert_eq!(variant_suffix(ExportVariant::SideBySide), "sbs");
        assert_eq!(variant_suffix(ExportVariant::Stacked), "stacked");
    }

    #[test]
    fn base_name_has_the_locked_shape() {
        let dir = scratch_dir("base");
        let path = resolve_output_path(&dir, "clip", ExportVariant::Panorama).unwrap();
        assert_eq!(path, dir.join("clip_panorama.mp4"));
        assert!(!path.exists(), "the resolver must not create the file");
    }

    #[test]
    fn pre_existing_output_yields_a_collision_suffix() {
        let dir = scratch_dir("collision");
        std::fs::write(dir.join("clip_sbs.mp4"), b"taken").unwrap();
        let path = resolve_output_path(&dir, "clip", ExportVariant::SideBySide).unwrap();
        assert_eq!(path, dir.join("clip_sbs_1.mp4"));
    }

    #[test]
    fn a_taken_suffix_advances_to_the_next_free_number() {
        let dir = scratch_dir("advance");
        std::fs::write(dir.join("clip_stacked.mp4"), b"taken").unwrap();
        std::fs::write(dir.join("clip_stacked_1.mp4"), b"taken").unwrap();
        let path = resolve_output_path(&dir, "clip", ExportVariant::Stacked).unwrap();
        assert_eq!(path, dir.join("clip_stacked_2.mp4"));
    }

    #[test]
    fn sanitize_stem_blocks_separators_and_dot_climbs() {
        assert_eq!(sanitize_stem("a/b"), "a_b");
        assert_eq!(sanitize_stem(".."), "export");
        assert_eq!(sanitize_stem(""), "export");
        assert_eq!(sanitize_stem("clip"), "clip");
    }

    #[test]
    fn exhausted_collisions_fail_with_a_typed_error() {
        let dir = scratch_dir("exhausted");
        std::fs::write(dir.join("clip_sbs.mp4"), b"taken").unwrap();
        for n in 1..=MAX_COLLISION_SUFFIX {
            std::fs::write(dir.join(format!("clip_sbs_{n}.mp4")), b"taken").unwrap();
        }
        let err = resolve_output_path(&dir, "clip", ExportVariant::SideBySide).unwrap_err();
        assert!(matches!(err, ExportNamingError::CollisionExhausted { .. }));
    }
}
