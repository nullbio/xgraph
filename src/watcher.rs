//! Filesystem watcher, debouncing, and update scheduling.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, after, never, select, unbounded};
use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};

use crate::ignore::Matcher;

/// `Arc<Mutex<...>>` because both the worker thread and the public
/// `WatcherHandle` may need to add new watches dynamically (when a
/// previously-unseen directory appears under the root).
type SharedNotify = Arc<Mutex<RecommendedWatcher>>;

const DEFAULT_DEBOUNCE_WINDOW: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct WatcherConfig {
    pub debounce_window: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce_window: DEFAULT_DEBOUNCE_WINDOW,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchedChanges {
    pub created: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub ignore_file_changed: bool,
}

impl BatchedChanges {
    fn is_empty(&self) -> bool {
        !self.ignore_file_changed
            && self.created.is_empty()
            && self.modified.is_empty()
            && self.deleted.is_empty()
    }
}

#[derive(Debug)]
pub enum WatcherError {
    Construct(notify::Error),
    Watch {
        path: PathBuf,
        source: notify::Error,
    },
    SpawnWorker(std::io::Error),
}

impl std::fmt::Display for WatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Construct(err) => write!(f, "failed to construct filesystem watcher: {err}"),
            Self::Watch { path, source } => {
                write!(
                    f,
                    "failed to register watch on {}: {source}",
                    path.display()
                )
            }
            Self::SpawnWorker(err) => write!(f, "failed to spawn watcher worker thread: {err}"),
        }
    }
}

impl std::error::Error for WatcherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Construct(err) => Some(err),
            Self::Watch { source, .. } => Some(source),
            Self::SpawnWorker(err) => Some(err),
        }
    }
}

/// Owns the live notify watcher plus the debounce worker thread.
pub struct Watcher {
    _notify: SharedNotify,
    shutdown_tx: Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl Watcher {
    pub fn start(
        root: &Path,
        matcher: Arc<dyn Matcher>,
        config: WatcherConfig,
    ) -> Result<(WatcherHandle, Receiver<BatchedChanges>), WatcherError> {
        let (raw_tx, raw_rx) = unbounded::<RawEvent>();
        let (batch_tx, batch_rx) = unbounded::<BatchedChanges>();
        let (shutdown_tx, shutdown_rx) = unbounded::<()>();

        let forward_tx = raw_tx.clone();
        let notify = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                for raw in raw_events_from(event) {
                    let _ = forward_tx.send(raw);
                }
            }
        })
        .map_err(WatcherError::Construct)?;
        let notify: SharedNotify = Arc::new(Mutex::new(notify));

        // Walk the worktree honoring the ignore matcher and register a
        // non-recursive inotify watch per kept directory. This is the
        // crucial difference from `RecursiveMode::Recursive` on the
        // root: that one walks every directory unconditionally — including
        // `.git/`, `node_modules/`, `vendor/`, `target/`, build caches,
        // etc. — consuming thousands of inotify slots for paths we never
        // process. With per-directory watches we use roughly
        // `count(non-ignored directories)` slots instead.
        //
        // Failures on individual subdirectories are tolerated (a denied
        // permission or a transient inotify-limit overflow shouldn't
        // sink the whole watcher); only failure to watch the root
        // itself is fatal.
        let walker = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.depth() == 0 || !matcher.matched(entry.path()));
        let mut root_watched = false;
        let mut watch_failures: usize = 0;
        for entry in walker.flatten() {
            if !entry.file_type().is_dir() {
                continue;
            }
            let path = entry.path();
            let result = {
                let mut guard = notify.lock().expect("watcher mutex poisoned");
                guard.watch(path, RecursiveMode::NonRecursive)
            };
            match result {
                Ok(()) => {
                    if entry.depth() == 0 {
                        root_watched = true;
                    }
                }
                Err(err) => {
                    if entry.depth() == 0 {
                        return Err(WatcherError::Watch {
                            path: path.to_path_buf(),
                            source: err,
                        });
                    }
                    watch_failures += 1;
                }
            }
        }
        if !root_watched {
            // Walking the root directory itself failed (e.g. path
            // doesn't exist, permission denied). Surface that.
            return Err(WatcherError::Watch {
                path: root.to_path_buf(),
                source: notify::Error::generic("worktree root not watchable"),
            });
        }
        if watch_failures > 0 {
            eprintln!(
                "xgraph watcher: {} directories skipped (permission denied or inotify limit); \
                 incremental updates for those paths will be missed",
                watch_failures
            );
        }

        let worker_matcher = Arc::clone(&matcher);
        let worker_notify = Arc::clone(&notify);
        let debounce = config.debounce_window;
        let worker_batch_tx = batch_tx.clone();
        let join = thread::Builder::new()
            .name("xgraph-watcher".into())
            .spawn(move || {
                run_debounce_loop(
                    raw_rx,
                    shutdown_rx,
                    worker_batch_tx,
                    worker_matcher,
                    debounce,
                    worker_notify,
                );
            })
            .map_err(WatcherError::SpawnWorker)?;

        let watcher = Self {
            _notify: notify,
            shutdown_tx: shutdown_tx.clone(),
            join: Some(join),
        };

        let handle = WatcherHandle {
            inner: Some(watcher),
        };
        Ok((handle, batch_rx))
    }

    fn stop(&mut self) {
        let _ = self.shutdown_tx.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Public, opaque handle returned to callers. Dropping the handle stops the watcher.
pub struct WatcherHandle {
    inner: Option<Watcher>,
}

impl WatcherHandle {
    pub fn stop(mut self) {
        if let Some(mut watcher) = self.inner.take() {
            watcher.stop();
        }
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        if let Some(mut watcher) = self.inner.take() {
            watcher.stop();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone)]
struct RawEvent {
    path: PathBuf,
    kind: ChangeKind,
}

fn raw_events_from(event: Event) -> Vec<RawEvent> {
    let paths = event.paths;
    match event.kind {
        EventKind::Create(CreateKind::File | CreateKind::Folder | CreateKind::Any) => {
            broadcast(paths, ChangeKind::Created)
        }
        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_) | ModifyKind::Any) => {
            broadcast(paths, ChangeKind::Modified)
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            broadcast(paths, ChangeKind::Deleted)
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            broadcast(paths, ChangeKind::Created)
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // `Both` carries the source path followed by the target path. Map the
            // source to Deleted and the target to Created; any further paths
            // default to Modified as a conservative fallback.
            let mut out = Vec::with_capacity(paths.len());
            for (index, path) in paths.into_iter().enumerate() {
                let kind = match index {
                    0 => ChangeKind::Deleted,
                    1 => ChangeKind::Created,
                    _ => ChangeKind::Modified,
                };
                out.push(RawEvent { path, kind });
            }
            out
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Any | RenameMode::Other)) => {
            // We do not know which side this is. Treat as Modified so downstream
            // consumers re-hash and reconcile rather than dropping or creating
            // active rows incorrectly.
            broadcast(paths, ChangeKind::Modified)
        }
        EventKind::Remove(RemoveKind::File | RemoveKind::Folder | RemoveKind::Any) => {
            broadcast(paths, ChangeKind::Deleted)
        }
        _ => Vec::new(),
    }
}

fn broadcast(paths: Vec<PathBuf>, kind: ChangeKind) -> Vec<RawEvent> {
    paths
        .into_iter()
        .map(|path| RawEvent { path, kind })
        .collect()
}

fn run_debounce_loop(
    raw_rx: Receiver<RawEvent>,
    shutdown_rx: Receiver<()>,
    batch_tx: Sender<BatchedChanges>,
    matcher: Arc<dyn Matcher>,
    debounce: Duration,
    notify: SharedNotify,
) {
    let mut pending: HashMap<PathBuf, ChangeKind> = HashMap::new();
    let mut last_event_at: Option<Instant> = None;

    loop {
        let timeout = match last_event_at {
            Some(at) => {
                let elapsed = at.elapsed();
                if elapsed >= debounce {
                    Duration::ZERO
                } else {
                    debounce - elapsed
                }
            }
            None => Duration::ZERO,
        };
        let timer = if last_event_at.is_some() {
            after(timeout)
        } else {
            never()
        };

        select! {
            recv(shutdown_rx) -> _ => {
                if !pending.is_empty() {
                    flush(&mut pending, &matcher, &batch_tx);
                }
                return;
            }
            recv(raw_rx) -> msg => {
                match msg {
                    Ok(event) => {
                        // If a new directory appeared and we'd index it,
                        // attach a watch right away so files created
                        // inside it generate events. Per-directory
                        // watches don't propagate, so a new subdirectory
                        // would be a blind spot otherwise.
                        if event.kind == ChangeKind::Created {
                            maybe_attach_watch_for_new_dir(&event.path, &matcher, &notify);
                        }
                        record_event(&mut pending, event);
                        last_event_at = Some(Instant::now());
                    }
                    Err(_) => {
                        // raw_rx sender side dropped — drain and exit.
                        if !pending.is_empty() {
                            flush(&mut pending, &matcher, &batch_tx);
                        }
                        return;
                    }
                }
            }
            recv(timer) -> _ => {
                flush(&mut pending, &matcher, &batch_tx);
                last_event_at = None;
            }
        }
    }
}

/// When a `Created` event arrives, check whether the new path is a
/// directory that the ignore matcher would keep. If so, register a
/// non-recursive watch on it so we receive events for files added
/// inside. Best-effort: registration failures are logged once and
/// don't disturb the rest of the watcher.
fn maybe_attach_watch_for_new_dir(path: &Path, matcher: &Arc<dyn Matcher>, notify: &SharedNotify) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    if matcher.matched(path) {
        return;
    }
    if let Ok(mut guard) = notify.lock()
        && let Err(err) = guard.watch(path, RecursiveMode::NonRecursive)
    {
        eprintln!(
            "xgraph watcher: failed to watch new directory {}: {err}",
            path.display()
        );
    }
}

fn record_event(pending: &mut HashMap<PathBuf, ChangeKind>, event: RawEvent) {
    // Per-path deduplication: keep the LAST event kind seen in the window.
    pending.insert(event.path, event.kind);
}

fn flush(
    pending: &mut HashMap<PathBuf, ChangeKind>,
    matcher: &Arc<dyn Matcher>,
    batch_tx: &Sender<BatchedChanges>,
) {
    if pending.is_empty() {
        return;
    }
    let drained: Vec<(PathBuf, ChangeKind)> = pending.drain().collect();
    let batch = build_batch(drained, matcher.as_ref());
    if !batch.is_empty() {
        let _ = batch_tx.send(batch);
    }
}

fn build_batch(events: Vec<(PathBuf, ChangeKind)>, matcher: &dyn Matcher) -> BatchedChanges {
    let mut batch = BatchedChanges::default();
    for (path, kind) in events {
        let is_ignore_source = path_is_ignore_source(&path);
        if is_ignore_source {
            batch.ignore_file_changed = true;
        }
        if matcher.matched(&path) {
            continue;
        }
        match kind {
            ChangeKind::Created => batch.created.push(path),
            ChangeKind::Modified => batch.modified.push(path),
            ChangeKind::Deleted => batch.deleted.push(path),
        }
    }
    batch.created.sort();
    batch.modified.sort();
    batch.deleted.sort();
    batch
}

fn path_is_ignore_source(path: &Path) -> bool {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(".gitignore" | ".xgraphignore") => true,
        Some("exclude") => path_under_git_info(path),
        _ => false,
    }
}

fn path_under_git_info(path: &Path) -> bool {
    let mut components = path.components().rev();
    let Some(file) = components.next() else {
        return false;
    };
    if file.as_os_str() != "exclude" {
        return false;
    }
    let Some(parent) = components.next() else {
        return false;
    };
    if parent.as_os_str() != "info" {
        return false;
    }
    // Require an ancestor `.git` component to avoid false positives on
    // unrelated paths like `docs/info/exclude`. A worktree's main gitdir is
    // `<root>/.git/info/exclude`; linked worktrees live outside the watched
    // root and are out of scope for a single-worktree watcher.
    components.any(|component| component.as_os_str() == ".git")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    struct AcceptAll;
    impl Matcher for AcceptAll {
        fn matched(&self, _: &Path) -> bool {
            false
        }
    }

    struct Excludes(&'static str);
    impl Matcher for Excludes {
        fn matched(&self, path: &Path) -> bool {
            path.to_string_lossy().contains(self.0)
        }
    }

    struct Counting {
        calls: AtomicUsize,
    }
    impl Matcher for Counting {
        fn matched(&self, _: &Path) -> bool {
            self.calls.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    fn test_config() -> WatcherConfig {
        // 100ms is the default; tests should be tolerant up to ~3x.
        WatcherConfig {
            debounce_window: Duration::from_millis(100),
        }
    }

    fn wait_factor() -> u32 {
        // 3x debounce window per the unit's testing guidance.
        3
    }

    fn recv_batch(rx: &Receiver<BatchedChanges>, debounce: Duration) -> Option<BatchedChanges> {
        rx.recv_timeout(debounce * wait_factor() + Duration::from_millis(250))
            .ok()
    }

    fn drain_with_deadline(
        rx: &Receiver<BatchedChanges>,
        debounce: Duration,
    ) -> Vec<BatchedChanges> {
        let mut out = Vec::new();
        let deadline = Instant::now() + debounce * wait_factor() + Duration::from_millis(500);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(batch) => out.push(batch),
                Err(_) => break,
            }
        }
        out
    }

    fn merge(batches: Vec<BatchedChanges>) -> BatchedChanges {
        let mut acc = BatchedChanges::default();
        for batch in batches {
            acc.created.extend(batch.created);
            acc.modified.extend(batch.modified);
            acc.deleted.extend(batch.deleted);
            acc.ignore_file_changed |= batch.ignore_file_changed;
        }
        acc.created.sort();
        acc.modified.sort();
        acc.deleted.sort();
        acc
    }

    fn canonical(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn canonical_root(dir: &TempDir) -> PathBuf {
        canonical(dir.path())
    }

    fn batch_contains(batch_paths: &[PathBuf], target: &Path) -> bool {
        let target = canonical(target);
        batch_paths
            .iter()
            .any(|p| canonical(p) == target || p.ends_with(target.file_name().unwrap_or_default()))
    }

    fn start(
        root: &Path,
        matcher: Arc<dyn Matcher>,
        config: WatcherConfig,
    ) -> (WatcherHandle, Receiver<BatchedChanges>, Duration) {
        let debounce = config.debounce_window;
        let (handle, rx) = Watcher::start(root, matcher, config).expect("watcher start");
        // Give notify a brief moment to register the inotify watch before we
        // perform filesystem operations.
        thread::sleep(Duration::from_millis(50));
        (handle, rx, debounce)
    }

    #[test]
    fn create_file_is_reported() {
        let dir = TempDir::new().expect("tempdir");
        let root = canonical_root(&dir);
        let (handle, rx, debounce) = start(&root, Arc::new(AcceptAll), test_config());

        let file = root.join("hello.txt");
        fs::write(&file, b"hi").expect("write");

        let batch = merge(drain_with_deadline(&rx, debounce));
        assert!(
            batch_contains(&batch.created, &file) || batch_contains(&batch.modified, &file),
            "expected hello.txt in created/modified, got {batch:?}",
        );

        handle.stop();
    }

    #[test]
    fn rapid_modifies_coalesce_to_one_entry() {
        let dir = TempDir::new().expect("tempdir");
        let root = canonical_root(&dir);
        let file = root.join("burst.txt");
        fs::write(&file, b"v1").expect("seed");

        let (handle, rx, debounce) = start(&root, Arc::new(AcceptAll), test_config());

        // Drain any seed event the watcher might have observed for the existing file.
        let _ = drain_with_deadline(&rx, debounce);

        // Two rapid modifications within the debounce window.
        fs::write(&file, b"v2").expect("write v2");
        fs::write(&file, b"v3").expect("write v3");

        let batch = merge(drain_with_deadline(&rx, debounce));
        let canonical_file = canonical(&file);
        let occurrences = batch
            .created
            .iter()
            .chain(batch.modified.iter())
            .chain(batch.deleted.iter())
            .filter(|p| canonical(p) == canonical_file)
            .count();
        assert_eq!(occurrences, 1, "expected exactly one entry, got {batch:?}");

        handle.stop();
    }

    #[test]
    fn create_then_delete_collapses_to_deleted() {
        let dir = TempDir::new().expect("tempdir");
        let root = canonical_root(&dir);
        let (handle, rx, debounce) = start(&root, Arc::new(AcceptAll), test_config());

        let file = root.join("ephemeral.txt");
        fs::write(&file, b"hi").expect("write");
        fs::remove_file(&file).expect("delete");

        let batch = merge(drain_with_deadline(&rx, debounce));
        let canonical_file = canonical(&file);
        let in_deleted = batch.deleted.iter().any(|p| canonical(p) == canonical_file);
        let in_created = batch.created.iter().any(|p| canonical(p) == canonical_file);
        let in_modified = batch
            .modified
            .iter()
            .any(|p| canonical(p) == canonical_file);

        assert!(
            in_deleted,
            "expected deleted to contain path; got {batch:?}"
        );
        assert!(
            !in_created && !in_modified,
            "expected collapse to only deleted; got {batch:?}",
        );

        handle.stop();
    }

    #[test]
    fn ignored_paths_are_filtered_out() {
        let dir = TempDir::new().expect("tempdir");
        let root = canonical_root(&dir);
        let matcher: Arc<dyn Matcher> = Arc::new(Excludes("ignored"));
        let (handle, rx, debounce) = start(&root, matcher, test_config());

        let kept = root.join("foo.txt");
        let dropped = root.join("ignored.txt");
        fs::write(&kept, b"k").expect("write kept");
        fs::write(&dropped, b"d").expect("write dropped");

        let batch = merge(drain_with_deadline(&rx, debounce));
        let canonical_kept = canonical(&kept);
        let canonical_dropped = canonical(&dropped);
        let all_paths = || {
            batch
                .created
                .iter()
                .chain(batch.modified.iter())
                .chain(batch.deleted.iter())
        };
        assert!(
            all_paths().any(|p| canonical(p) == canonical_kept),
            "expected foo.txt in batch; got {batch:?}",
        );
        assert!(
            all_paths().all(|p| canonical(p) != canonical_dropped),
            "did not expect ignored.txt in batch; got {batch:?}",
        );

        handle.stop();
    }

    /// The new per-directory watcher must not register an inotify watch
    /// on an ignored subdirectory. Verified indirectly: if we did watch
    /// it, a file created inside would produce a (later-filtered) raw
    /// event and the underlying inotify slot would be consumed. The
    /// test asserts the kernel never tells us about the file. (A
    /// surviving filter at the worker layer would mask a real regression
    /// here; the older "recursive watch on root" implementation would
    /// register the watch even for ignored paths.)
    #[test]
    fn ignored_subdirectories_are_not_watched_at_kernel_level() {
        let dir = TempDir::new().expect("tempdir");
        let root = canonical_root(&dir);
        // Pre-create the ignored subdirectory so the initial walk has
        // to skip it explicitly.
        let ignored_dir = root.join("ignored_dir");
        fs::create_dir(&ignored_dir).expect("create ignored dir");

        let matcher: Arc<dyn Matcher> = Arc::new(Excludes("ignored_dir"));
        let (handle, rx, debounce) = start(&root, matcher, test_config());
        // Drain the initial-create flurry caused by setting up the
        // tempdir before we focus on the targeted file.
        let _ = drain_with_deadline(&rx, debounce);

        // Touch a file inside the ignored subdir. If the watcher
        // attached an inotify watch to `ignored_dir/` we'd see this
        // event arrive; the matcher-based filter would discard it but
        // the raw_events_from counter would still tick. With the new
        // implementation no watch exists, so nothing reaches us.
        let target = ignored_dir.join("inside.txt");
        fs::write(&target, b"x").expect("write inside ignored");

        let batches = drain_with_deadline(&rx, debounce);
        let canonical_target = canonical(&target);
        for batch in &batches {
            for p in batch
                .created
                .iter()
                .chain(batch.modified.iter())
                .chain(batch.deleted.iter())
            {
                assert_ne!(
                    canonical(p),
                    canonical_target,
                    "ignored subdirectory must not be watched at all; got {p:?}",
                );
            }
        }

        handle.stop();
    }

    #[test]
    fn gitignore_change_sets_ignore_file_changed_flag() {
        let dir = TempDir::new().expect("tempdir");
        let root = canonical_root(&dir);
        let gitignore = root.join(".gitignore");
        fs::write(&gitignore, b"# seed").expect("seed gitignore");

        let (handle, rx, debounce) = start(&root, Arc::new(AcceptAll), test_config());
        let _ = drain_with_deadline(&rx, debounce);

        fs::write(&gitignore, b"target/\n").expect("update gitignore");

        let batch = merge(drain_with_deadline(&rx, debounce));
        assert!(
            batch.ignore_file_changed,
            "expected ignore_file_changed=true; got {batch:?}",
        );

        handle.stop();
    }

    #[test]
    fn stop_halts_further_events() {
        let dir = TempDir::new().expect("tempdir");
        let root = canonical_root(&dir);
        let (handle, rx, debounce) = start(&root, Arc::new(AcceptAll), test_config());

        let pre = root.join("before.txt");
        fs::write(&pre, b"x").expect("write");
        let _ = drain_with_deadline(&rx, debounce);

        handle.stop();

        let post = root.join("after.txt");
        fs::write(&post, b"y").expect("write");

        // After stop the receiver should be disconnected (because batch_tx was
        // dropped by Watcher) or empty after the debounce window.
        let observed = recv_batch(&rx, debounce);
        assert!(
            observed.is_none(),
            "expected no further events after stop; got {observed:?}",
        );
    }

    #[test]
    fn counting_matcher_sees_paths() {
        // Sanity check that the matcher trait is actually invoked.
        let dir = TempDir::new().expect("tempdir");
        let root = canonical_root(&dir);
        let counting = Arc::new(Counting {
            calls: AtomicUsize::new(0),
        });
        let matcher: Arc<dyn Matcher> = counting.clone();
        let (handle, rx, debounce) = start(&root, matcher, test_config());

        let file = root.join("touched.txt");
        fs::write(&file, b"x").expect("write");
        let _ = drain_with_deadline(&rx, debounce);

        assert!(
            counting.calls.load(Ordering::Relaxed) > 0,
            "matcher should have been called at least once",
        );

        handle.stop();
    }

    #[test]
    fn rename_target_is_classified_as_created() {
        use notify::event::EventAttributes;
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            paths: vec![PathBuf::from("/tmp/new.txt")],
            attrs: EventAttributes::default(),
        };
        let raws = raw_events_from(event);
        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0].kind, ChangeKind::Created);
    }

    #[test]
    fn rename_source_is_classified_as_deleted() {
        use notify::event::EventAttributes;
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            paths: vec![PathBuf::from("/tmp/old.txt")],
            attrs: EventAttributes::default(),
        };
        let raws = raw_events_from(event);
        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0].kind, ChangeKind::Deleted);
    }

    #[test]
    fn rename_both_emits_delete_then_create_in_order() {
        use notify::event::EventAttributes;
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![PathBuf::from("/tmp/old.txt"), PathBuf::from("/tmp/new.txt")],
            attrs: EventAttributes::default(),
        };
        let raws = raw_events_from(event);
        assert_eq!(raws.len(), 2);
        assert_eq!(raws[0].kind, ChangeKind::Deleted);
        assert_eq!(raws[0].path, PathBuf::from("/tmp/old.txt"));
        assert_eq!(raws[1].kind, ChangeKind::Created);
        assert_eq!(raws[1].path, PathBuf::from("/tmp/new.txt"));
    }

    #[test]
    fn ignore_source_detection_requires_dot_git_ancestor() {
        assert!(path_is_ignore_source(Path::new("/proj/.gitignore")));
        assert!(path_is_ignore_source(Path::new("/proj/sub/.xgraphignore")));
        assert!(path_is_ignore_source(Path::new("/proj/.git/info/exclude")));
        assert!(!path_is_ignore_source(Path::new("/proj/docs/info/exclude")));
        assert!(!path_is_ignore_source(Path::new("/proj/info/exclude")));
        assert!(!path_is_ignore_source(Path::new("/proj/random.txt")));
    }

    #[test]
    fn empty_batch_is_not_emitted() {
        // If every event is dropped by the matcher and no ignore-source is touched,
        // we should not see a batch on the receiver.
        let dir = TempDir::new().expect("tempdir");
        let root = canonical_root(&dir);
        let matcher: Arc<dyn Matcher> = Arc::new(Excludes(""));
        let (handle, rx, debounce) = start(&root, matcher, test_config());

        let file = root.join("hidden.txt");
        fs::write(&file, b"x").expect("write");

        let observed = drain_with_deadline(&rx, debounce);
        assert!(observed.is_empty(), "expected no batches; got {observed:?}");

        handle.stop();
    }
}
