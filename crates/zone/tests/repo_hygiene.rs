//! The repo holds only the client: no GitHub Actions workflows, and no file that names a path in
//! the private server repo or a private crate. The SDK comes from zone-sdk, nothing else.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The names to keep out of the repo come from a private list (one per line, `#` comments), so
/// this public file does not carry them. Without `ZONE_FORBIDDEN_NAMES` there is nothing to find.
fn forbidden() -> Vec<String> {
    let Ok(path) = std::env::var("ZONE_FORBIDDEN_NAMES") else {
        return Vec::new();
    };
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name == ".git" || name == "target" {
            continue;
        }
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[test]
fn no_github_workflows() {
    assert!(
        !repo_root().join(".github/workflows").exists(),
        ".github/workflows must not exist; this repo runs no GitHub Actions"
    );
}

#[test]
fn no_private_paths_or_crates() {
    let mut all = Vec::new();
    files(&repo_root(), &mut all);
    let patterns = forbidden();
    let mut hits = Vec::new();
    for path in &all {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let text = text.to_lowercase();
        for p in &patterns {
            if text.contains(p.as_str()) {
                hits.push(format!("{}: {p}", path.display()));
            }
        }
    }
    assert!(hits.is_empty(), "private references:\n{}", hits.join("\n"));
}

#[test]
fn lock_has_only_sdk_crates_from_zone_sdk() {
    let lock = fs::read_to_string(repo_root().join("Cargo.lock")).unwrap();
    for line in lock.lines().filter(|l| l.starts_with("source = \"git+")) {
        assert!(
            line.starts_with("source = \"git+https://github.com/Black-Path-Labs/zone-sdk?rev="),
            "git source outside zone-sdk: {line}"
        );
    }
}
