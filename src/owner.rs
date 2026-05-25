//! Per-worktree owner that bundles the daemon's long-lived dependencies.
//!
//! Owns the Cozo store, writer queue, ignore matcher, language registry, and
//! the parser-version constant used in content fact rows. Provides the
//! daemon-side primitives for initial indexing and incremental updates.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cozo::{
    ContentHash as CozoContentHash, CozoStore, FileUpdate, FileUpdateMetadata, WriterError,
    WriterHandle, WriterQueue,
};
use crate::ignore::IgnoreMatcher;
use crate::language::LanguageRegistry;
use crate::scanner::{DetectedLanguage, ScanError, scan};

pub const PARSER_VERSION: u32 = 1;

pub struct WorktreeOwner {
    worktree_root: PathBuf,
    matcher: IgnoreMatcher,
    registry: LanguageRegistry,
    writer: WriterHandle,
    generation: u64,
}

impl WorktreeOwner {
    /// Build an owner against a freshly-opened Cozo store. Caller supplies
    /// the store; the owner takes responsibility for the writer queue.
    pub fn new(
        worktree_root: PathBuf,
        matcher: IgnoreMatcher,
        registry: LanguageRegistry,
        store: CozoStore,
    ) -> Result<Self, crate::cozo::CozoError> {
        let writer = WriterQueue::start(store)?;
        Ok(Self {
            worktree_root,
            matcher,
            registry,
            writer,
            generation: 1,
        })
    }

    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    /// Walk the worktree and submit one `FileUpdate` per supported file.
    /// Returns the number of files submitted to the writer queue.
    pub fn index_all(&mut self) -> Result<usize, OwnerError> {
        let scanned = scan(&self.worktree_root, &self.matcher)?;
        let mut count = 0usize;
        for file in scanned {
            if file.language.is_none() {
                continue;
            }
            self.submit_file(file.path, file.mtime, file.size, file.language)?;
            count += 1;
        }
        Ok(count)
    }

    /// Re-extract a single path and submit the update.
    pub fn process_change(&mut self, path: PathBuf) -> Result<bool, OwnerError> {
        if !path.exists() {
            return Ok(false);
        }
        let metadata = fs::metadata(&path).map_err(|source| OwnerError::Io {
            path: path.clone(),
            source,
        })?;
        let mtime = metadata.modified().unwrap_or_else(|_| SystemTime::now());
        let size = metadata.len();
        let detected = crate::scanner::detect_language(&path);
        if detected.is_none() {
            return Ok(false);
        }
        self.submit_file(path, mtime, size, detected)?;
        Ok(true)
    }

    fn submit_file(
        &mut self,
        path: PathBuf,
        mtime: SystemTime,
        size: u64,
        language: Option<DetectedLanguage>,
    ) -> Result<(), OwnerError> {
        let Some(lang) = language else {
            return Ok(());
        };
        let bytes = fs::read(&path).map_err(|source| OwnerError::Io {
            path: path.clone(),
            source,
        })?;
        let content_hash = crate::hash::hash_bytes(&bytes);
        let relative = path
            .strip_prefix(&self.worktree_root)
            .unwrap_or(&path)
            .to_path_buf();
        let Some(extracted) = self.registry.extract_file(&relative, &bytes) else {
            return Ok(());
        };
        let metadata = FileUpdateMetadata {
            content_hash: CozoContentHash::from_bytes(*content_hash.as_bytes()),
            language: language_label(lang).to_owned(),
            parser_version: PARSER_VERSION,
            mtime: mtime_seconds(mtime),
            size,
            generation: self.generation,
        };
        self.generation += 1;
        let mut update = FileUpdate::from_extracted(&extracted, metadata);
        update.path = relative.to_string_lossy().into_owned();
        self.writer.submit(update)?;
        Ok(())
    }

    /// Drain pending submissions and return any errors the writer thread recorded.
    pub fn shutdown(mut self) -> Vec<WriterError> {
        self.writer.shutdown();
        self.writer.take_errors()
    }
}

#[derive(Debug)]
pub enum OwnerError {
    Scan(ScanError),
    Cozo(crate::cozo::CozoError),
    Writer(WriterError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for OwnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OwnerError::Scan(err) => write!(f, "{err}"),
            OwnerError::Cozo(err) => write!(f, "{err}"),
            OwnerError::Writer(err) => write!(f, "{err}"),
            OwnerError::Io { path, source } => {
                write!(f, "io error on {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for OwnerError {}

impl From<ScanError> for OwnerError {
    fn from(err: ScanError) -> Self {
        OwnerError::Scan(err)
    }
}

impl From<crate::cozo::CozoError> for OwnerError {
    fn from(err: crate::cozo::CozoError) -> Self {
        OwnerError::Cozo(err)
    }
}

impl From<WriterError> for OwnerError {
    fn from(err: WriterError) -> Self {
        OwnerError::Writer(err)
    }
}

fn mtime_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_secs() as i64,
        Err(err) => -(err.duration().as_secs() as i64),
    }
}

fn language_label(id: DetectedLanguage) -> &'static str {
    match id {
        DetectedLanguage::Php => "php",
        DetectedLanguage::Blade => "blade",
        DetectedLanguage::JavaScript => "javascript",
        DetectedLanguage::TypeScript => "typescript",
        DetectedLanguage::Tsx => "tsx",
        DetectedLanguage::Python => "python",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn index_all_submits_one_update_per_supported_file() {
        let tmp = TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("a.py"), "def f():\n    return 1\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "no language").unwrap();

        let matcher = IgnoreMatcher::new(tmp.path()).expect("matcher");
        let registry = LanguageRegistry::with_all();
        let store_dir = tmp.path().join(".xgraph-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let store = CozoStore::open(&store_dir).expect("cozo");

        let mut owner =
            WorktreeOwner::new(tmp.path().to_path_buf(), matcher, registry, store).expect("owner");
        let n = owner.index_all().expect("index_all");
        assert_eq!(n, 1, "only the .py file should produce an update");
        let errs = owner.shutdown();
        assert!(errs.is_empty(), "writer errors: {errs:?}");
    }

    #[test]
    fn process_change_skips_paths_without_language() {
        let tmp = TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("README.md"), "# hi\n").unwrap();

        let matcher = IgnoreMatcher::new(tmp.path()).expect("matcher");
        let registry = LanguageRegistry::with_all();
        let store_dir = tmp.path().join(".xgraph-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let store = CozoStore::open(&store_dir).expect("cozo");

        let mut owner =
            WorktreeOwner::new(tmp.path().to_path_buf(), matcher, registry, store).expect("owner");
        let changed = owner
            .process_change(tmp.path().join("README.md"))
            .expect("process_change");
        assert!(!changed);
        let errs = owner.shutdown();
        assert!(errs.is_empty());
    }
}
