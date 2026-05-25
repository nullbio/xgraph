use std::fs;
use std::process::Command;

use tempfile::TempDir;
use xgraph::cli::init_at;

fn init_git_repo(root: &std::path::Path) {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .arg(root)
        .status()
        .expect("git init");
    assert!(status.success());
}

#[test]
fn init_indexes_a_minimal_git_repo() {
    let tmp = TempDir::new().expect("tempdir");
    init_git_repo(tmp.path());
    fs::write(
        tmp.path().join("hello.py"),
        "def greet():\n    return 'hi'\n",
    )
    .expect("write fixture");

    let result = init_at(tmp.path()).expect("init runs to completion");
    assert_eq!(result, std::process::ExitCode::SUCCESS);

    // The persistent dir lives under $(git rev-parse --git-path xgraph).
    let xgraph_dir = tmp.path().join(".git").join("xgraph");
    assert!(
        xgraph_dir.exists(),
        "expected persistent dir at {}",
        xgraph_dir.display()
    );
    assert!(
        xgraph_dir.join("graph.cozo").exists() || xgraph_dir.join("graph.cozo.db").exists(),
        "expected Cozo DB inside {}",
        xgraph_dir.display()
    );
}
