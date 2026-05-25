use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct TempGitRepo {
    root: PathBuf,
}

impl TempGitRepo {
    pub fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "xgraph-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));

        fs::create_dir_all(&root).expect("failed to create temporary repo directory");

        let status = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(&root)
            .status()
            .expect("failed to run git init");

        assert!(status.success(), "git init failed with status {status}");

        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempGitRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
