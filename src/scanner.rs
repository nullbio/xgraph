//! Ignore-aware scanning and manifest reconciliation.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::hash::{ContentHash, HashError, hash_file};
use crate::ignore::Matcher;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetectedLanguage {
    Php,
    Blade,
    JavaScript,
    TypeScript,
    Tsx,
    Python,
}

#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub language: Option<DetectedLanguage>,
    pub content_hash: ContentHash,
    pub mtime: SystemTime,
    pub size: u64,
}

#[derive(Debug)]
pub enum ScanError {
    Walk {
        path: PathBuf,
        source: walkdir::Error,
    },
    Metadata {
        path: PathBuf,
        source: io::Error,
    },
    Hash {
        path: PathBuf,
        source: HashError,
    },
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Walk { path, source } => {
                write!(f, "failed to walk {}: {source}", path.display())
            }
            Self::Metadata { path, source } => {
                write!(
                    f,
                    "failed to read metadata for {}: {source}",
                    path.display()
                )
            }
            Self::Hash { path, source } => {
                write!(f, "failed to hash {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Walk { source, .. } => Some(source),
            Self::Metadata { source, .. } => Some(source),
            Self::Hash { source, .. } => Some(source),
        }
    }
}

pub fn detect_language(path: &Path) -> Option<DetectedLanguage> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();

    if lower.ends_with(".blade.php") {
        return Some(DetectedLanguage::Blade);
    }

    let ext = Path::new(&lower).extension()?.to_str()?;
    match ext {
        "php" => Some(DetectedLanguage::Php),
        "js" | "cjs" | "mjs" => Some(DetectedLanguage::JavaScript),
        "ts" => Some(DetectedLanguage::TypeScript),
        "tsx" => Some(DetectedLanguage::Tsx),
        "py" => Some(DetectedLanguage::Python),
        _ => None,
    }
}

/// Walk the worktree and hash every non-ignored file.
///
/// The walk is single-threaded (cheap relative to hashing) but each file's
/// metadata read + BLAKE3 hash runs on a rayon worker so a cold scan over a
/// large project saturates available cores. Output is sorted by path so
/// downstream cross-file resolution is deterministic.
pub fn scan(root: &Path, matcher: &dyn Matcher) -> Result<Vec<ScannedFile>, ScanError> {
    // Walk first; cheap relative to the per-file hash.
    let mut candidates: Vec<PathBuf> = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            !matcher.matched(entry.path())
        });
    for entry in walker {
        let entry = entry.map_err(|source| {
            let path = source.path().map(Path::to_path_buf).unwrap_or_default();
            ScanError::Walk { path, source }
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        candidates.push(entry.into_path());
    }

    // Hash + stat in parallel.
    let mut files: Vec<ScannedFile> = candidates
        .into_par_iter()
        .map(hash_candidate)
        .collect::<Result<Vec<_>, _>>()?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn hash_candidate(path: PathBuf) -> Result<ScannedFile, ScanError> {
    let metadata = fs::metadata(&path).map_err(|source| ScanError::Metadata {
        path: path.clone(),
        source,
    })?;
    let mtime = metadata.modified().map_err(|source| ScanError::Metadata {
        path: path.clone(),
        source,
    })?;
    let size = metadata.len();
    let content_hash = hash_file(&path).map_err(|source| ScanError::Hash {
        path: path.clone(),
        source,
    })?;
    let language = detect_language(&path);
    Ok(ScannedFile {
        path,
        language,
        content_hash,
        mtime,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;
    use std::collections::HashSet;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    use tempfile::TempDir;

    struct PrefixMatcher {
        ignored: Vec<PathBuf>,
    }

    impl Matcher for PrefixMatcher {
        fn matched(&self, path: &Path) -> bool {
            self.ignored.iter().any(|p| path.starts_with(p))
        }
    }

    struct AllowAll;
    impl Matcher for AllowAll {
        fn matched(&self, _path: &Path) -> bool {
            false
        }
    }

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        let mut file = fs::File::create(path).expect("create file");
        file.write_all(contents).expect("write file");
        file.flush().expect("flush file");
    }

    #[test]
    fn detect_language_handles_all_known_extensions() {
        assert_eq!(
            detect_language(Path::new("/x/main.php")),
            Some(DetectedLanguage::Php)
        );
        assert_eq!(
            detect_language(Path::new("/x/home.blade.php")),
            Some(DetectedLanguage::Blade)
        );
        assert_eq!(
            detect_language(Path::new("/x/index.js")),
            Some(DetectedLanguage::JavaScript)
        );
        assert_eq!(
            detect_language(Path::new("/x/legacy.cjs")),
            Some(DetectedLanguage::JavaScript)
        );
        assert_eq!(
            detect_language(Path::new("/x/module.mjs")),
            Some(DetectedLanguage::JavaScript)
        );
        assert_eq!(
            detect_language(Path::new("/x/lib.ts")),
            Some(DetectedLanguage::TypeScript)
        );
        assert_eq!(
            detect_language(Path::new("/x/App.tsx")),
            Some(DetectedLanguage::Tsx)
        );
        assert_eq!(
            detect_language(Path::new("/x/script.py")),
            Some(DetectedLanguage::Python)
        );
        assert_eq!(detect_language(Path::new("/x/README.md")), None);
        assert_eq!(detect_language(Path::new("/x/no_extension")), None);
    }

    #[test]
    fn detect_language_is_case_insensitive() {
        assert_eq!(
            detect_language(Path::new("/x/App.TSX")),
            Some(DetectedLanguage::Tsx)
        );
        assert_eq!(
            detect_language(Path::new("/x/Home.Blade.PHP")),
            Some(DetectedLanguage::Blade)
        );
    }

    #[test]
    fn detect_language_prefers_blade_over_php() {
        assert_eq!(
            detect_language(Path::new("/x/page.blade.php")),
            Some(DetectedLanguage::Blade)
        );
        assert_ne!(
            detect_language(Path::new("/x/page.blade.php")),
            Some(DetectedLanguage::Php)
        );
    }

    #[test]
    fn scan_collects_all_files_with_allow_all_matcher() {
        let dir = TempDir::new().expect("temp dir");
        write_file(&dir.path().join("a.php"), b"<?php echo 1;");
        write_file(&dir.path().join("b.js"), b"console.log(1);");
        write_file(&dir.path().join("nested/c.py"), b"print(1)");
        write_file(&dir.path().join("nested/d.txt"), b"plain text");

        let scanned = scan(dir.path(), &AllowAll).expect("scan");
        let names: HashSet<_> = scanned
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains("a.php"));
        assert!(names.contains("b.js"));
        assert!(names.contains("c.py"));
        assert!(names.contains("d.txt"));
        assert_eq!(scanned.len(), 4);
    }

    #[test]
    fn scan_prunes_ignored_directories() {
        let dir = TempDir::new().expect("temp dir");
        write_file(&dir.path().join("keep.php"), b"<?php");
        write_file(&dir.path().join("vendor/skip.php"), b"<?php skip");
        write_file(&dir.path().join("vendor/deep/also_skip.php"), b"<?php skip");
        write_file(&dir.path().join("src/inner.ts"), b"export {};");

        let matcher = PrefixMatcher {
            ignored: vec![dir.path().join("vendor")],
        };

        let scanned = scan(dir.path(), &matcher).expect("scan");
        let names: HashSet<_> = scanned
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains("keep.php"));
        assert!(names.contains("inner.ts"));
        assert!(!names.contains("skip.php"));
        assert!(!names.contains("also_skip.php"));
        assert_eq!(scanned.len(), 2);
    }

    #[test]
    fn scan_skips_individually_matched_files() {
        let dir = TempDir::new().expect("temp dir");
        write_file(&dir.path().join("keep.ts"), b"export const x = 1;");
        write_file(&dir.path().join("skip.ts"), b"export const y = 2;");

        let matcher = PrefixMatcher {
            ignored: vec![dir.path().join("skip.ts")],
        };

        let scanned = scan(dir.path(), &matcher).expect("scan");
        assert_eq!(scanned.len(), 1);
        assert_eq!(
            scanned[0].path.file_name().unwrap().to_string_lossy(),
            "keep.ts"
        );
    }

    #[test]
    fn scan_returns_correct_hash_language_mtime_and_size() {
        let dir = TempDir::new().expect("temp dir");
        let php_path = dir.path().join("app.php");
        let blade_path = dir.path().join("views/home.blade.php");
        let unknown_path = dir.path().join("notes.md");

        let php_bytes = b"<?php echo 'hello';";
        let blade_bytes = b"<div>{{ $user }}</div>";
        let unknown_bytes = b"# Notes";

        write_file(&php_path, php_bytes);
        write_file(&blade_path, blade_bytes);
        write_file(&unknown_path, unknown_bytes);

        let scanned = scan(dir.path(), &AllowAll).expect("scan");
        assert_eq!(scanned.len(), 3);

        for file in &scanned {
            let canonical_name = file
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            match canonical_name.as_str() {
                "app.php" => {
                    assert_eq!(file.language, Some(DetectedLanguage::Php));
                    assert_eq!(file.content_hash, hash_bytes(php_bytes));
                    assert_eq!(file.size, php_bytes.len() as u64);
                    let metadata = fs::metadata(&file.path).expect("metadata");
                    assert_eq!(file.mtime, metadata.modified().expect("mtime"));
                }
                "home.blade.php" => {
                    assert_eq!(file.language, Some(DetectedLanguage::Blade));
                    assert_eq!(file.content_hash, hash_bytes(blade_bytes));
                    assert_eq!(file.size, blade_bytes.len() as u64);
                }
                "notes.md" => {
                    assert_eq!(file.language, None);
                    assert_eq!(file.content_hash, hash_bytes(unknown_bytes));
                    assert_eq!(file.size, unknown_bytes.len() as u64);
                }
                other => panic!("unexpected scanned file: {other}"),
            }
        }
    }

    #[test]
    fn scan_does_not_follow_symlinks() {
        let dir = TempDir::new().expect("temp dir");
        let target = TempDir::new().expect("symlink target dir");
        write_file(&target.path().join("outside.php"), b"<?php outside");
        write_file(&dir.path().join("inside.php"), b"<?php inside");

        std::os::unix::fs::symlink(target.path(), dir.path().join("linked"))
            .expect("create symlink");

        let scanned = scan(dir.path(), &AllowAll).expect("scan");
        let names: HashSet<_> = scanned
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains("inside.php"));
        assert!(!names.contains("outside.php"));
    }

    #[test]
    fn scan_hashes_files_with_unknown_language() {
        let dir = TempDir::new().expect("temp dir");
        let bin_path = dir.path().join("data.bin");
        write_file(&bin_path, &[0u8, 1, 2, 3, 4]);

        let scanned = scan(dir.path(), &AllowAll).expect("scan");
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].language, None);
        assert_eq!(scanned[0].content_hash, hash_bytes(&[0u8, 1, 2, 3, 4]));
    }
}
