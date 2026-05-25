mod support;

#[test]
fn temp_git_repo_helper_creates_repository() {
    let repo = support::TempGitRepo::new();

    assert!(repo.root().join(".git").exists());
}
