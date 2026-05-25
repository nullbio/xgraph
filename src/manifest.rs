//! Manifest reconciliation: diff a scanner snapshot against the active manifest.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFileInput {
    pub path: PathBuf,
    pub content_hash: [u8; 32],
    pub mtime: SystemTime,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveFileRow {
    pub path: PathBuf,
    pub content_hash: [u8; 32],
    pub mtime: SystemTime,
    pub size: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconciliationPlan {
    pub dirty: Vec<PathBuf>,
    pub missing: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}

pub fn reconcile(scanned: &[ScannedFileInput], active: &[ActiveFileRow]) -> ReconciliationPlan {
    let current_generation = active.iter().map(|row| row.generation).max();

    let active_by_path: BTreeMap<&PathBuf, &ActiveFileRow> =
        active.iter().map(|row| (&row.path, row)).collect();
    let scanned_by_path: BTreeMap<&PathBuf, &ScannedFileInput> =
        scanned.iter().map(|file| (&file.path, file)).collect();

    let mut dirty = Vec::new();
    let mut missing = Vec::new();
    let mut deleted = Vec::new();

    for (path, file) in &scanned_by_path {
        match active_by_path.get(path) {
            None => missing.push((*path).clone()),
            Some(row) => {
                if is_dirty(file, row, current_generation) {
                    dirty.push((*path).clone());
                }
            }
        }
    }

    for path in active_by_path.keys() {
        if !scanned_by_path.contains_key(path) {
            deleted.push((*path).clone());
        }
    }

    dirty.sort();
    missing.sort();
    deleted.sort();

    ReconciliationPlan {
        dirty,
        missing,
        deleted,
    }
}

fn is_dirty(
    scanned: &ScannedFileInput,
    active: &ActiveFileRow,
    current_generation: Option<u64>,
) -> bool {
    if scanned.content_hash != active.content_hash {
        return true;
    }
    if scanned.size != active.size {
        return true;
    }
    if let Some(generation) = current_generation
        && active.generation < generation
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn hash(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn epoch_plus(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn scanned(path: &str, hash_byte: u8, mtime_secs: u64, size: u64) -> ScannedFileInput {
        ScannedFileInput {
            path: PathBuf::from(path),
            content_hash: hash(hash_byte),
            mtime: epoch_plus(mtime_secs),
            size,
        }
    }

    fn active(
        path: &str,
        hash_byte: u8,
        mtime_secs: u64,
        size: u64,
        generation: u64,
    ) -> ActiveFileRow {
        ActiveFileRow {
            path: PathBuf::from(path),
            content_hash: hash(hash_byte),
            mtime: epoch_plus(mtime_secs),
            size,
            generation,
        }
    }

    fn paths(values: &[&str]) -> Vec<PathBuf> {
        values.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn empty_inputs_yield_empty_plan() {
        let plan = reconcile(&[], &[]);

        assert_eq!(plan, ReconciliationPlan::default());
    }

    #[test]
    fn all_new_scan_against_empty_active_marks_missing() {
        let scanned_files = vec![
            scanned("a.rs", 1, 10, 100),
            scanned("b.rs", 2, 20, 200),
            scanned("c.rs", 3, 30, 300),
        ];

        let plan = reconcile(&scanned_files, &[]);

        assert_eq!(plan.dirty, Vec::<PathBuf>::new());
        assert_eq!(plan.missing, paths(&["a.rs", "b.rs", "c.rs"]));
        assert_eq!(plan.deleted, Vec::<PathBuf>::new());
    }

    #[test]
    fn empty_scan_against_populated_active_marks_deleted() {
        let active_rows = vec![active("a.rs", 1, 10, 100, 1), active("b.rs", 2, 20, 200, 1)];

        let plan = reconcile(&[], &active_rows);

        assert_eq!(plan.dirty, Vec::<PathBuf>::new());
        assert_eq!(plan.missing, Vec::<PathBuf>::new());
        assert_eq!(plan.deleted, paths(&["a.rs", "b.rs"]));
    }

    #[test]
    fn matching_scan_and_active_yields_empty_plan() {
        let scanned_files = vec![scanned("a.rs", 1, 10, 100), scanned("b.rs", 2, 20, 200)];
        let active_rows = vec![active("a.rs", 1, 10, 100, 1), active("b.rs", 2, 20, 200, 1)];

        let plan = reconcile(&scanned_files, &active_rows);

        assert_eq!(plan, ReconciliationPlan::default());
    }

    #[test]
    fn differing_content_hash_marks_dirty() {
        let scanned_files = vec![scanned("a.rs", 9, 10, 100)];
        let active_rows = vec![active("a.rs", 1, 10, 100, 1)];

        let plan = reconcile(&scanned_files, &active_rows);

        assert_eq!(plan.dirty, paths(&["a.rs"]));
        assert!(plan.missing.is_empty());
        assert!(plan.deleted.is_empty());
    }

    #[test]
    fn matching_hash_with_different_mtime_is_not_dirty() {
        let scanned_files = vec![scanned("a.rs", 1, 999, 100)];
        let active_rows = vec![active("a.rs", 1, 10, 100, 1)];

        let plan = reconcile(&scanned_files, &active_rows);

        assert_eq!(plan, ReconciliationPlan::default());
    }

    #[test]
    fn matching_hash_with_different_size_marks_dirty() {
        let scanned_files = vec![scanned("a.rs", 1, 10, 500)];
        let active_rows = vec![active("a.rs", 1, 10, 100, 1)];

        let plan = reconcile(&scanned_files, &active_rows);

        assert_eq!(plan.dirty, paths(&["a.rs"]));
        assert!(plan.missing.is_empty());
        assert!(plan.deleted.is_empty());
    }

    #[test]
    fn older_generation_row_marks_dirty_even_when_metadata_matches() {
        let scanned_files = vec![scanned("a.rs", 1, 10, 100), scanned("b.rs", 2, 20, 200)];
        let active_rows = vec![active("a.rs", 1, 10, 100, 1), active("b.rs", 2, 20, 200, 2)];

        let plan = reconcile(&scanned_files, &active_rows);

        assert_eq!(plan.dirty, paths(&["a.rs"]));
        assert!(plan.missing.is_empty());
        assert!(plan.deleted.is_empty());
    }

    #[test]
    fn mixed_partitions_every_path_into_exactly_one_bucket() {
        let scanned_files = vec![
            scanned("clean.rs", 1, 10, 100),
            scanned("dirty-hash.rs", 9, 20, 200),
            scanned("dirty-size.rs", 3, 30, 999),
            scanned("dirty-generation.rs", 4, 40, 400),
            scanned("new.rs", 5, 50, 500),
        ];
        let active_rows = vec![
            active("clean.rs", 1, 10, 100, 2),
            active("dirty-hash.rs", 2, 20, 200, 2),
            active("dirty-size.rs", 3, 30, 200, 2),
            active("dirty-generation.rs", 4, 40, 400, 1),
            active("gone.rs", 6, 60, 600, 2),
        ];

        let plan = reconcile(&scanned_files, &active_rows);

        assert_eq!(
            plan.dirty,
            paths(&["dirty-generation.rs", "dirty-hash.rs", "dirty-size.rs"])
        );
        assert_eq!(plan.missing, paths(&["new.rs"]));
        assert_eq!(plan.deleted, paths(&["gone.rs"]));

        let mut all_paths = Vec::new();
        all_paths.extend(plan.dirty.iter().cloned());
        all_paths.extend(plan.missing.iter().cloned());
        all_paths.extend(plan.deleted.iter().cloned());
        let unique_count = all_paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert_eq!(unique_count, all_paths.len());
        assert_eq!(all_paths.len(), 5);
    }

    #[test]
    fn output_buckets_are_sorted_for_deterministic_comparison() {
        let scanned_files = vec![
            scanned("z.rs", 1, 10, 100),
            scanned("a.rs", 2, 20, 200),
            scanned("m.rs", 3, 30, 300),
        ];

        let plan = reconcile(&scanned_files, &[]);

        assert_eq!(plan.missing, paths(&["a.rs", "m.rs", "z.rs"]));
    }
}
