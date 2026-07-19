use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(command.current_dir(repo));
    command
}

fn git(repo: &Path, args: &[&str]) {
    let output = git_command(repo).args(args).output().expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repo_toplevel(repo: &Path) -> String {
    let output = git_command(repo)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("git toplevel");
    assert!(output.status.success(), "git rev-parse failed");
    String::from_utf8(output.stdout)
        .expect("utf8 toplevel")
        .trim()
        .to_string()
}

fn git_allow_fail(repo: &Path, args: &[&str]) -> Output {
    git_command(repo).args(args).output().expect("run git")
}

fn write_file(repo: &Path, relative: &str, content: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("parent directory")).expect("create parent");
    fs::write(path, content).expect("write file");
}

fn init_repo(repo: &Path) -> String {
    git(repo, &["init"]);
    git(repo, &["config", "user.email", "aft@example.com"]);
    git(repo, &["config", "user.name", "AFT Test"]);
    let output = git_command(repo)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .expect("current branch");
    assert!(output.status.success(), "git symbolic-ref failed");
    String::from_utf8(output.stdout)
        .expect("utf8 branch")
        .trim()
        .to_string()
}

fn create_merge_conflict(repo: &Path, relative: &str) {
    let base_branch = init_repo(repo);
    write_file(repo, relative, "base\n");
    write_file(repo, "packages/b/.keep", "keep\n");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "base"]);

    git(repo, &["checkout", "-b", "ours"]);
    write_file(repo, relative, "ours\n");
    git(repo, &["add", relative]);
    git(repo, &["commit", "-m", "ours"]);

    git(repo, &["checkout", &base_branch]);
    git(repo, &["checkout", "-b", "theirs"]);
    write_file(repo, relative, "theirs\n");
    git(repo, &["add", relative]);
    git(repo, &["commit", "-m", "theirs"]);

    git(repo, &["checkout", "ours"]);
    let output = git_allow_fail(repo, &["merge", "theirs"]);
    assert!(!output.status.success(), "merge should conflict");
}

fn configure_and_conflicts(project_root: &Path) -> (AftProcess, Value) {
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(project_root)["success"], true);
    let resp = aft.send(
        &json!({
            "id": "conflicts",
            "command": "git_conflicts"
        })
        .to_string(),
    );
    (aft, resp)
}

fn response_text(resp: &Value) -> &str {
    resp["text"].as_str().expect("response text")
}

#[test]
fn conflicts_found_when_project_root_is_sibling_subdir() {
    let dir = tempfile::tempdir().unwrap();
    create_merge_conflict(dir.path(), "packages/a/x.txt");

    let project_root = dir.path().join("packages/b");
    let (aft, resp) = configure_and_conflicts(&project_root);

    assert_eq!(resp["success"], true, "conflicts response: {resp:?}");
    assert_eq!(resp["file_count"], 1, "conflicts response: {resp:?}");
    assert!(response_text(&resp).contains("packages/a/x.txt"));
    assert!(response_text(&resp).contains("<<<<<<< HEAD"));
    assert!(aft.shutdown().success());
}

#[test]
fn staged_but_still_marked_file_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    create_merge_conflict(dir.path(), "packages/a/x.txt");
    git(dir.path(), &["add", "packages/a/x.txt"]);

    let (aft, resp) = configure_and_conflicts(dir.path());

    assert_eq!(resp["success"], true, "conflicts response: {resp:?}");
    assert_eq!(resp["file_count"], 1, "conflicts response: {resp:?}");
    assert!(response_text(&resp).contains("packages/a/x.txt"));
    assert!(response_text(&resp).contains(">>>>>>> theirs"));
    assert!(aft.shutdown().success());
}

#[test]
fn conflict_during_rebase_detached_head_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let base_branch = init_repo(repo);
    write_file(repo, "src/rebased.txt", "base\n");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "base"]);

    git(repo, &["checkout", "-b", "feature"]);
    write_file(repo, "src/rebased.txt", "feature\n");
    git(repo, &["add", "src/rebased.txt"]);
    git(repo, &["commit", "-m", "feature"]);

    git(repo, &["checkout", &base_branch]);
    write_file(repo, "src/rebased.txt", "mainline\n");
    git(repo, &["add", "src/rebased.txt"]);
    git(repo, &["commit", "-m", "mainline"]);

    git(repo, &["checkout", "feature"]);
    let output = git_allow_fail(repo, &["rebase", &base_branch]);
    assert!(!output.status.success(), "rebase should conflict");

    let (aft, resp) = configure_and_conflicts(repo);

    assert_eq!(resp["success"], true, "conflicts response: {resp:?}");
    assert_eq!(resp["file_count"], 1, "conflicts response: {resp:?}");
    assert!(response_text(&resp).contains("src/rebased.txt"));
    assert!(response_text(&resp).contains("<<<<<<< HEAD"));
    assert!(aft.shutdown().success());
}

#[test]
fn bare_separator_line_does_not_create_false_positive() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_repo(repo);
    write_file(repo, "README.md", "heading\n=======\nbody\n");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "clean"]);

    let (aft, resp) = configure_and_conflicts(repo);

    assert_eq!(resp["success"], true, "conflicts response: {resp:?}");
    assert_eq!(resp["file_count"], 0, "conflicts response: {resp:?}");
    assert_eq!(resp["conflict_count"], 0, "conflicts response: {resp:?}");
    assert!(response_text(&resp).starts_with("No merge conflicts found."));
    assert!(response_text(&resp).contains("Checked repo root:"));
    assert!(aft.shutdown().success());
}

#[test]
fn toplevel_relative_conflict_path_reads_nested_file_content() {
    let dir = tempfile::tempdir().unwrap();
    create_merge_conflict(dir.path(), "nested/deep/x.txt");

    let (aft, resp) = configure_and_conflicts(dir.path());

    assert_eq!(resp["success"], true, "conflicts response: {resp:?}");
    assert_eq!(resp["file_count"], 1, "conflicts response: {resp:?}");
    let text = response_text(&resp);
    assert!(text.contains("nested/deep/x.txt"));
    assert!(text.contains("ours"));
    assert!(text.contains("theirs"));
    assert!(aft.shutdown().success());
}

#[test]
fn conflicts_in_non_ascii_filename_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    create_merge_conflict(dir.path(), "nested/café.txt");

    let (aft, resp) = configure_and_conflicts(dir.path());

    assert_eq!(resp["success"], true, "conflicts response: {resp:?}");
    assert_eq!(resp["file_count"], 1, "conflicts response: {resp:?}");
    assert_eq!(resp["conflict_count"], 1, "conflicts response: {resp:?}");
    let text = response_text(&resp);
    assert!(text.contains("nested/café.txt"));
    assert!(text.contains("<<<<<<< HEAD"));
    assert!(text.contains(">>>>>>> theirs"));
    assert!(aft.shutdown().success());
}

#[test]
fn conflicts_in_newline_filename_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    create_merge_conflict(dir.path(), "nested/conflict\nname.txt");

    let (aft, resp) = configure_and_conflicts(dir.path());

    assert_eq!(resp["success"], true, "conflicts response: {resp:?}");
    assert_eq!(resp["file_count"], 1, "conflicts response: {resp:?}");
    assert_eq!(resp["conflict_count"], 1, "conflicts response: {resp:?}");
    let text = response_text(&resp);
    assert!(text.contains("nested/conflict\nname.txt"));
    assert!(text.contains("<<<<<<< HEAD"));
    assert!(text.contains(">>>>>>> theirs"));
    assert!(aft.shutdown().success());
}

#[test]
fn optional_path_can_inspect_external_repo_and_reports_checked_root() {
    let conflicted_dir = tempfile::tempdir().unwrap();
    create_merge_conflict(conflicted_dir.path(), "packages/a/x.txt");
    let r1_root = repo_toplevel(conflicted_dir.path());

    let clean_dir = tempfile::tempdir().unwrap();
    init_repo(clean_dir.path());
    write_file(clean_dir.path(), "clean.txt", "clean\n");
    git(clean_dir.path(), &["add", "."]);
    git(clean_dir.path(), &["commit", "-m", "clean"]);
    let r2_root = repo_toplevel(clean_dir.path());

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(clean_dir.path())["success"], true);

    let clean_resp = aft.send(
        &json!({
            "id": "conflicts-clean",
            "command": "git_conflicts"
        })
        .to_string(),
    );
    assert_eq!(
        clean_resp["success"], true,
        "clean response: {clean_resp:?}"
    );
    assert_eq!(
        clean_resp["file_count"], 0,
        "clean response: {clean_resp:?}"
    );
    assert_eq!(clean_resp["checked_root"], r2_root);
    assert!(response_text(&clean_resp).contains("Checked repo root:"));

    let external_subdir = conflicted_dir.path().join("packages/a");
    let conflict_resp = aft.send(
        &json!({
            "id": "conflicts-external",
            "command": "git_conflicts",
            "params": { "path": external_subdir }
        })
        .to_string(),
    );
    assert_eq!(
        conflict_resp["success"], true,
        "external response: {conflict_resp:?}"
    );
    assert_eq!(
        conflict_resp["file_count"], 1,
        "external response: {conflict_resp:?}"
    );
    assert_eq!(conflict_resp["checked_root"], r1_root);
    assert!(response_text(&conflict_resp).contains("packages/a/x.txt"));
    assert!(response_text(&conflict_resp).contains("Checked repo root:"));

    assert!(aft.shutdown().success());
}
