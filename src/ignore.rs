//! Shared ignore matcher for scanner, watcher, sync, reindex, and startup
//! reconciliation.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use ignore as ignore_crate;
use ignore_crate::gitignore::{Gitignore, GitignoreBuilder};
use ignore_crate::overrides::{Override, OverrideBuilder};
use ignore_crate::{DirEntry, Match, WalkBuilder};

const BUILT_IN_EXCLUSIONS: &[&str] = &[
    ".git",
    ".xgraph",
    "xgraph/",
    "node_modules",
    "vendor",
    "dist",
    "build",
    "target",
    "__pycache__",
    ".pytest_cache",
];

const XGRAPHIGNORE_FILENAME: &str = ".xgraphignore";

#[derive(Debug)]
pub enum IgnoreError {
    Ignore(ignore_crate::Error),
    Io { path: PathBuf, source: io::Error },
}

impl std::fmt::Display for IgnoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IgnoreError::Ignore(err) => write!(f, "ignore error: {err}"),
            IgnoreError::Io { path, source } => {
                write!(f, "io error reading `{}`: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for IgnoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IgnoreError::Ignore(err) => Some(err),
            IgnoreError::Io { source, .. } => Some(source),
        }
    }
}

impl From<ignore_crate::Error> for IgnoreError {
    fn from(err: ignore_crate::Error) -> Self {
        IgnoreError::Ignore(err)
    }
}

pub trait Matcher: Send + Sync {
    fn matched(&self, path: &Path) -> bool;
}

impl Matcher for IgnoreMatcher {
    fn matched(&self, path: &Path) -> bool {
        IgnoreMatcher::matched(self, path)
    }
}

pub struct IgnoreMatcher {
    root: PathBuf,
    built_in_overrides: Override,
    built_in_matcher: Gitignore,
    gitignore_files: Vec<Gitignore>,
    git_exclude: Option<Gitignore>,
    xgraphignore_files: Vec<Gitignore>,
}

impl IgnoreMatcher {
    pub fn new(worktree_root: &Path) -> Result<Self, IgnoreError> {
        let root = worktree_root.to_path_buf();
        let built_in_overrides = build_built_in_overrides(&root)?;
        let built_in_matcher = build_built_in_gitignore(&root)?;
        let mut matcher = Self {
            root,
            built_in_overrides,
            built_in_matcher,
            gitignore_files: Vec::new(),
            git_exclude: None,
            xgraphignore_files: Vec::new(),
        };
        matcher.load_file_matchers()?;
        Ok(matcher)
    }

    pub fn matched(&self, path: &Path) -> bool {
        // Mirror `WalkBuilder`'s top-down traversal: an ancestor directory
        // that resolves to `Ignore` excludes everything inside it. If every
        // ancestor is reachable, the leaf is the deciding component.
        let rel = match path.strip_prefix(&self.root) {
            Ok(rel) => rel,
            Err(_) => return false,
        };
        if rel.as_os_str().is_empty() {
            return false;
        }

        // `Path::ancestors` returns leaf-first; reverse to walk root-first.
        let mut ancestors: Vec<&Path> = rel
            .ancestors()
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        ancestors.reverse();

        let leaf_is_dir = path.is_dir();
        let last_index = ancestors.len() - 1;
        for (idx, sub_rel) in ancestors.into_iter().enumerate() {
            let abs = self.root.join(sub_rel);
            // Intermediate ancestors are always directories. Only the leaf
            // queries the filesystem to know whether it's a file or dir.
            let is_dir = idx != last_index || leaf_is_dir;
            if self.decide(&abs, is_dir).is_ignore() {
                return true;
            }
        }
        false
    }

    fn decide(&self, path: &Path, is_dir: bool) -> Decision {
        // Built-in exclusions are absolute: a positive match here cannot be
        // overridden by any project-controlled ignore file.
        if match_gitignore(&self.built_in_matcher, path, is_dir) == Some(true) {
            return Decision::Ignore;
        }

        // Custom ignore files (`.xgraphignore`) take precedence over
        // `.gitignore`. `.git/info/exclude` ranks below the in-tree
        // `.gitignore` files, matching the `ignore` crate's ordering.
        let mut decision = Decision::None;
        decision = decision.or(self.decide_in_stack(&self.xgraphignore_files, path, is_dir));
        decision = decision.or(self.decide_in_stack(&self.gitignore_files, path, is_dir));
        if let Some(exclude) = self.git_exclude.as_ref() {
            decision = decision.or(decision_from_match(match_gitignore(exclude, path, is_dir)));
        }
        decision
    }

    fn decide_in_stack(&self, stack: &[Gitignore], path: &Path, is_dir: bool) -> Decision {
        // Deeper files override shallower ones; the first decisive match
        // from leaf-to-root wins.
        for gi in stack.iter().rev() {
            let decision = decision_from_match(match_gitignore(gi, path, is_dir));
            if !matches!(decision, Decision::None) {
                return decision;
            }
        }
        Decision::None
    }

    pub fn walk(&self) -> impl Iterator<Item = Result<DirEntry, IgnoreError>> {
        self.walk_builder().build().map(|entry| match entry {
            Ok(entry) => Ok(entry),
            Err(err) => Err(IgnoreError::from(err)),
        })
    }

    pub fn rebuild(&mut self) -> Result<(), IgnoreError> {
        self.built_in_overrides = build_built_in_overrides(&self.root)?;
        self.built_in_matcher = build_built_in_gitignore(&self.root)?;
        self.gitignore_files.clear();
        self.git_exclude = None;
        self.xgraphignore_files.clear();
        self.load_file_matchers()?;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn walk_builder(&self) -> WalkBuilder {
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .standard_filters(false)
            .hidden(false)
            .parents(false)
            .ignore(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(false)
            .require_git(false)
            .add_custom_ignore_filename(XGRAPHIGNORE_FILENAME)
            .overrides(self.built_in_overrides.clone());
        builder
    }

    fn load_file_matchers(&mut self) -> Result<(), IgnoreError> {
        // Walk the tree honoring only the built-in overrides so that we
        // discover every `.gitignore` and `.xgraphignore` in non-excluded
        // directories. Each discovered file becomes its own `Gitignore`
        // matcher rooted at its parent directory.
        let mut discovery = WalkBuilder::new(&self.root);
        discovery
            .standard_filters(false)
            .hidden(false)
            .parents(false)
            .ignore(false)
            .git_ignore(false)
            .git_exclude(false)
            .git_global(false)
            .require_git(false)
            .overrides(self.built_in_overrides.clone());

        for entry in discovery.build() {
            let entry = entry?;
            let file_type = match entry.file_type() {
                Some(ft) => ft,
                None => continue,
            };
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            match file_name {
                ".gitignore" => {
                    let parent = path.parent().unwrap_or(&self.root);
                    self.gitignore_files.push(build_gitignore(parent, path)?);
                }
                XGRAPHIGNORE_FILENAME => {
                    let parent = path.parent().unwrap_or(&self.root);
                    self.xgraphignore_files.push(build_gitignore(parent, path)?);
                }
                _ => {}
            }
        }

        if let Some(common_dir) = resolve_git_common_dir(&self.root)? {
            let exclude_path = common_dir.join("info").join("exclude");
            if exclude_path.is_file() {
                self.git_exclude = Some(build_gitignore(&self.root, &exclude_path)?);
            }
        }

        Ok(())
    }
}

/// Resolves Git's `GIT_COMMON_DIR` for the given worktree root. Returns
/// `None` when there is no `.git` entry at the root. Mirrors the resolution
/// the `ignore` crate performs internally so that `matched()` agrees with
/// `walk()` for linked worktrees.
fn resolve_git_common_dir(root: &Path) -> Result<Option<PathBuf>, IgnoreError> {
    let git_path = root.join(".git");
    let metadata = match git_path.symlink_metadata() {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(IgnoreError::Io {
                path: git_path,
                source: err,
            });
        }
    };

    if metadata.is_dir() {
        return Ok(Some(git_path));
    }

    if !metadata.is_file() {
        return Ok(None);
    }

    let file = File::open(&git_path).map_err(|err| IgnoreError::Io {
        path: git_path.clone(),
        source: err,
    })?;
    let mut lines = BufReader::new(file).lines();
    let line = match lines.next() {
        Some(Ok(line)) => line,
        Some(Err(err)) => {
            return Err(IgnoreError::Io {
                path: git_path,
                source: err,
            });
        }
        None => return Ok(None),
    };
    let Some(real_git_dir) = line.strip_prefix("gitdir: ") else {
        return Ok(None);
    };
    let real_git_dir = PathBuf::from(real_git_dir);
    if !real_git_dir.is_dir() {
        return Ok(None);
    }

    let commondir_marker = real_git_dir.join("commondir");
    if !commondir_marker.is_file() {
        return Ok(Some(real_git_dir));
    }

    let file = File::open(&commondir_marker).map_err(|err| IgnoreError::Io {
        path: commondir_marker.clone(),
        source: err,
    })?;
    let mut lines = BufReader::new(file).lines();
    let commondir_line = match lines.next() {
        Some(Ok(line)) => line,
        Some(Err(err)) => {
            return Err(IgnoreError::Io {
                path: commondir_marker,
                source: err,
            });
        }
        None => return Ok(Some(real_git_dir)),
    };
    let common = PathBuf::from(commondir_line);
    let resolved = if common.is_absolute() {
        common
    } else {
        real_git_dir.join(common)
    };
    Ok(Some(resolved))
}

fn build_built_in_overrides(root: &Path) -> Result<Override, IgnoreError> {
    let mut builder = OverrideBuilder::new(root);
    for pattern in BUILT_IN_EXCLUSIONS {
        // `OverrideBuilder` treats a leading `!` as an "ignore" rule and an
        // unprefixed glob as a whitelist rule. We want every built-in pattern
        // to act as an unconditional ignore, so prefix each with `!`.
        let inverted = format!("!{pattern}");
        builder.add(&inverted).map_err(IgnoreError::from)?;
    }
    builder.build().map_err(IgnoreError::from)
}

fn build_built_in_gitignore(root: &Path) -> Result<Gitignore, IgnoreError> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in BUILT_IN_EXCLUSIONS {
        builder.add_line(None, pattern).map_err(IgnoreError::from)?;
    }
    builder.build().map_err(IgnoreError::from)
}

fn build_gitignore(root: &Path, file: &Path) -> Result<Gitignore, IgnoreError> {
    let mut builder = GitignoreBuilder::new(root);
    if let Some(err) = builder.add(file) {
        return Err(IgnoreError::from(err));
    }
    builder.build().map_err(IgnoreError::from)
}

fn match_gitignore(matcher: &Gitignore, path: &Path, is_dir: bool) -> Option<bool> {
    // `matched_path_or_any_parents` panics if `path` is not under the
    // matcher's root, so screen with a prefix check first.
    if !path.starts_with(matcher.path()) {
        return None;
    }
    match matcher.matched_path_or_any_parents(path, is_dir) {
        Match::None => None,
        Match::Ignore(_) => Some(true),
        Match::Whitelist(_) => Some(false),
    }
}

#[derive(Clone, Copy, Debug)]
enum Decision {
    None,
    Ignore,
    Whitelist,
}

impl Decision {
    fn is_ignore(self) -> bool {
        matches!(self, Decision::Ignore)
    }

    /// Mirrors `Option::or`: returns `self` unless it is `None`, in which
    /// case it returns `other`.
    fn or(self, other: Decision) -> Decision {
        match self {
            Decision::None => other,
            _ => self,
        }
    }
}

fn decision_from_match(matched: Option<bool>) -> Decision {
    match matched {
        None => Decision::None,
        Some(true) => Decision::Ignore,
        Some(false) => Decision::Whitelist,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write file");
    }

    fn touch(path: &Path) {
        write(path, "");
    }

    fn matcher(root: &Path) -> IgnoreMatcher {
        IgnoreMatcher::new(root).expect("build matcher")
    }

    #[test]
    fn excludes_built_in_directories() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        touch(&root.join(".git/HEAD"));
        touch(&root.join(".xgraph/config.toml"));
        touch(&root.join("node_modules/foo/index.js"));
        touch(&root.join("vendor/composer.json"));
        touch(&root.join("dist/main.js"));
        touch(&root.join("build/output.o"));
        touch(&root.join("target/debug/binary"));
        touch(&root.join("__pycache__/cache.pyc"));
        touch(&root.join(".pytest_cache/v"));
        touch(&root.join("src/main.rs"));

        let m = matcher(root);

        for excluded in [
            ".git/HEAD",
            ".xgraph/config.toml",
            "node_modules/foo/index.js",
            "vendor/composer.json",
            "dist/main.js",
            "build/output.o",
            "target/debug/binary",
            "__pycache__/cache.pyc",
            ".pytest_cache/v",
        ] {
            let path = root.join(excluded);
            assert!(m.matched(&path), "expected `{excluded}` to be ignored");
        }

        assert!(
            !m.matched(&root.join("src/main.rs")),
            "expected `src/main.rs` to be kept"
        );
    }

    #[test]
    fn honors_gitignore_patterns() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join(".gitignore"), "*.log\nsecrets/\n");
        touch(&root.join("app.log"));
        touch(&root.join("notes.txt"));
        touch(&root.join("secrets/token"));

        let m = matcher(root);

        assert!(m.matched(&root.join("app.log")));
        assert!(m.matched(&root.join("secrets/token")));
        assert!(!m.matched(&root.join("notes.txt")));
        assert!(!m.matched(&root.join(".gitignore")));
    }

    #[test]
    fn honors_xgraphignore_independently_of_gitignore() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join(".xgraphignore"), "fixtures/\n");
        touch(&root.join("fixtures/sample.txt"));
        touch(&root.join("src/main.rs"));

        let m = matcher(root);

        assert!(!root.join(".gitignore").exists());
        assert!(m.matched(&root.join("fixtures/sample.txt")));
        assert!(!m.matched(&root.join("src/main.rs")));
    }

    #[test]
    fn walk_yields_only_non_ignored_files() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join(".gitignore"), "*.log\n");
        write(&root.join(".xgraphignore"), "fixtures/\n");
        touch(&root.join("src/main.rs"));
        touch(&root.join("README.md"));
        touch(&root.join("app.log"));
        touch(&root.join("node_modules/dep/index.js"));
        touch(&root.join("fixtures/sample.txt"));
        touch(&root.join("target/debug/x"));

        let m = matcher(root);

        let mut files: Vec<PathBuf> = Vec::new();
        for entry in m.walk() {
            let entry = entry.expect("walk entry");
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                files.push(entry.into_path());
            }
        }

        let rel: Vec<PathBuf> = files
            .iter()
            .map(|p| p.strip_prefix(root).expect("under root").to_path_buf())
            .collect();

        assert!(rel.contains(&PathBuf::from("src/main.rs")));
        assert!(rel.contains(&PathBuf::from("README.md")));
        assert!(rel.contains(&PathBuf::from(".gitignore")));
        assert!(rel.contains(&PathBuf::from(".xgraphignore")));
        assert!(!rel.contains(&PathBuf::from("app.log")));
        assert!(!rel.contains(&PathBuf::from("node_modules/dep/index.js")));
        assert!(!rel.contains(&PathBuf::from("fixtures/sample.txt")));
        assert!(!rel.contains(&PathBuf::from("target/debug/x")));
    }

    #[test]
    fn rebuild_picks_up_gitignore_changes() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join(".gitignore"), "*.log\n");
        touch(&root.join("app.log"));
        touch(&root.join("data.tmp"));

        let mut m = matcher(root);
        assert!(m.matched(&root.join("app.log")));
        assert!(!m.matched(&root.join("data.tmp")));

        write(&root.join(".gitignore"), "*.tmp\n");
        m.rebuild().expect("rebuild");

        assert!(!m.matched(&root.join("app.log")));
        assert!(m.matched(&root.join("data.tmp")));
    }

    #[test]
    fn rebuild_picks_up_xgraphignore_changes() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        touch(&root.join("first.txt"));
        touch(&root.join("second.txt"));

        let mut m = matcher(root);
        assert!(!m.matched(&root.join("first.txt")));

        write(&root.join(".xgraphignore"), "first.txt\n");
        m.rebuild().expect("rebuild");

        assert!(m.matched(&root.join("first.txt")));
        assert!(!m.matched(&root.join("second.txt")));
    }

    #[test]
    fn excludes_files_under_ignored_ancestor_directory() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join(".gitignore"), "private/\n");
        // A deeper `.gitignore` that tries to re-include is irrelevant per
        // git semantics: once a parent dir is excluded, nothing inside can
        // be reached.
        write(&root.join("private/.gitignore"), "!keep.txt\n");
        touch(&root.join("private/keep.txt"));
        touch(&root.join("public/keep.txt"));

        let m = matcher(root);

        assert!(m.matched(&root.join("private/keep.txt")));
        assert!(!m.matched(&root.join("public/keep.txt")));
    }

    #[test]
    fn xgraphignore_whitelist_overrides_gitignore_for_same_path() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join(".gitignore"), "*.log\n");
        write(&root.join(".xgraphignore"), "!debug.log\n");
        touch(&root.join("debug.log"));
        touch(&root.join("error.log"));

        let m = matcher(root);

        assert!(!m.matched(&root.join("debug.log")));
        assert!(m.matched(&root.join("error.log")));
    }

    #[test]
    fn honors_git_info_exclude_in_primary_worktree() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join(".git/info/exclude"), "private/\n");
        touch(&root.join("private/secret"));
        touch(&root.join("public/file.rs"));

        let m = matcher(root);

        assert!(m.matched(&root.join("private/secret")));
        assert!(!m.matched(&root.join("public/file.rs")));
    }

    #[test]
    fn honors_git_info_exclude_in_linked_worktree() {
        let tmp = TempDir::new().expect("tempdir");
        let base = tmp.path();
        // Mimic a `git worktree add` layout: a shared common dir plus a linked
        // worktree whose `.git` is a regular file pointing back at the shared
        // dir via a `commondir` marker.
        let common_dir = base.join("repo/.git");
        let worktree_git_dir = base.join("repo/.git/worktrees/linked");
        let linked_root = base.join("linked");
        fs::create_dir_all(&common_dir).expect("create common dir");
        fs::create_dir_all(common_dir.join("info")).expect("create info dir");
        fs::create_dir_all(&worktree_git_dir).expect("create per-worktree dir");
        fs::create_dir_all(&linked_root).expect("create linked root");
        write(&common_dir.join("info").join("exclude"), "shared_ignore/\n");
        write(&worktree_git_dir.join("commondir"), "../..\n");
        write(
            &linked_root.join(".git"),
            &format!("gitdir: {}\n", worktree_git_dir.display()),
        );
        touch(&linked_root.join("shared_ignore/file"));
        touch(&linked_root.join("kept/file"));

        let m = matcher(&linked_root);

        assert!(m.matched(&linked_root.join("shared_ignore/file")));
        assert!(!m.matched(&linked_root.join("kept/file")));
    }

    #[test]
    fn does_not_create_xgraphignore_when_absent() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        touch(&root.join("src/main.rs"));

        let m = matcher(root);
        // Exercise both query paths to ensure neither writes the file.
        let _ = m.matched(&root.join("src/main.rs"));
        for entry in m.walk() {
            let _ = entry.expect("walk entry");
        }

        assert!(
            !root.join(XGRAPHIGNORE_FILENAME).exists(),
            "matcher must not create `.xgraphignore`"
        );
    }
}
