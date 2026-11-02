//! scripts/release.sh refuses before it builds or calls gh. Each case runs a copy of the
//! script in a throwaway git repository whose PATH carries a gh that only records being called.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Repo {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    bin: PathBuf,
    gh_called: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn repo() -> Repo {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    fs::create_dir_all(dir.join("scripts")).unwrap();
    fs::copy(
        repo_root().join("scripts/release.sh"),
        dir.join("scripts/release.sh"),
    )
    .unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[workspace.package]\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);

    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let gh_called = tmp.path().join("gh-called");
    let gh = bin.join("gh");
    fs::write(
        &gh,
        format!("#!/bin/sh\necho \"$@\" >>'{}'\n", gh_called.display()),
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    Repo {
        _tmp: tmp,
        dir,
        bin,
        gh_called,
    }
}

fn release(repo: &Repo, args: &[&str]) -> Output {
    let path = format!(
        "{}:{}",
        repo.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(repo.dir.join("scripts/release.sh"))
        .args(args)
        .env("PATH", path)
        .env_remove("ZONE_RELEASE_APPROVED")
        .env_remove("ZONE_BUILD_LOCK")
        .output()
        .unwrap()
}

fn refused(repo: &Repo, out: &Output, code: i32, says: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(code), "{stderr}");
    assert!(stderr.contains(says), "{stderr}");
    assert!(!repo.gh_called.exists(), "gh was called");
}

#[test]
fn a_tag_that_is_not_semver_is_a_usage_error() {
    let repo = repo();
    for tag in ["0.1.0", "v0.1", "v0.1.0-", "v0.1.0 x"] {
        refused(&repo, &release(&repo, &[tag]), 2, "usage");
    }
    refused(&repo, &release(&repo, &[]), 2, "usage");
}

#[test]
fn a_tag_for_another_version_is_refused() {
    let repo = repo();
    let out = release(&repo, &["v0.2.0-rc.1"]);
    refused(&repo, &out, 1, "not a tag for version 0.1.0");
}

#[test]
fn publish_without_an_approval_id_is_refused() {
    let repo = repo();
    let out = release(&repo, &["--publish", "v0.1.0-rc.1"]);
    refused(&repo, &out, 1, "ZONE_RELEASE_APPROVED");
}

#[test]
fn a_dirty_tree_is_refused() {
    let repo = repo();
    fs::write(repo.dir.join("stray"), b"x").unwrap();
    let out = release(&repo, &["v0.1.0-rc.1"]);
    refused(&repo, &out, 1, "not clean");
}

#[test]
fn an_existing_tag_is_refused() {
    let repo = repo();
    git(&repo.dir, &["tag", "v0.1.0-rc.1"]);
    let out = release(&repo, &["v0.1.0-rc.1"]);
    refused(&repo, &out, 1, "tag v0.1.0-rc.1 exists");
}

#[test]
fn build_names_every_release_target_and_release_builds_them_all() {
    let out = Command::new(repo_root().join("scripts/build.sh"))
        .arg("riscv64gc-unknown-linux-musl")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let usage = String::from_utf8_lossy(&out.stderr);
    for target in [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-gnu",
    ] {
        assert!(usage.contains(target), "{target} missing from: {usage}");
    }
    let script = fs::read_to_string(repo_root().join("scripts/release.sh")).unwrap();
    assert!(
        script.contains("\nscripts/build.sh all\n"),
        "release.sh must build every target"
    );
}
