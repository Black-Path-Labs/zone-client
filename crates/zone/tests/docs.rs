//! The docs name only what exists: every `zone …` command in `README.md`, `SECURITY.md` and
//! `CONTRIBUTING.md` (in a code span or a code block) must be a real subcommand with real flags,
//! and a flag named on its own must be one of `zone`'s or a script's. Every subcommand is
//! documented in the README.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DOCS: [&str; 3] = ["README.md", "SECURITY.md", "CONTRIBUTING.md"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One command's `--help`: its subcommands and its flags (global ones included).
struct Help {
    subcommands: BTreeSet<String>,
    flags: BTreeSet<String>,
}

fn help(path: &[&str]) -> Option<Help> {
    let out = Command::new(env!("CARGO_BIN_EXE_zone"))
        .args(path)
        .arg("--help")
        .output()
        .expect("run zone");
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).unwrap();
    let mut subcommands = BTreeSet::new();
    let mut in_commands = false;
    for line in text.lines() {
        if line.ends_with(':') && !line.starts_with(' ') {
            in_commands = line == "Commands:";
            continue;
        }
        if in_commands {
            if let Some(name) = line.split_whitespace().next() {
                if line.starts_with("  ") && name != "help" {
                    subcommands.insert(name.to_owned());
                }
            }
        }
    }
    let flags = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| w.starts_with("--") && w.len() > 2)
        .map(str::to_owned)
        .collect();
    Some(Help { subcommands, flags })
}

/// Code spans and code-block lines of a Markdown file.
fn code(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            out.push(line.to_owned());
        } else {
            out.extend(line.split('`').skip(1).step_by(2).map(str::to_owned));
        }
    }
    out
}

/// Flags the scripts take, so `--allow-unsigned` and the like are not taken for `zone`'s.
fn script_text() -> String {
    let mut all = String::new();
    for entry in fs::read_dir(repo_root().join("scripts")).unwrap() {
        all.push_str(&fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    all.push_str(&fs::read_to_string(repo_root().join("install.sh")).unwrap());
    all
}

/// Check one `zone …` invocation; return the subcommand path it names.
fn check_invocation(words: &[&str], errors: &mut Vec<String>, doc: &str) -> Vec<String> {
    let mut path: Vec<String> = Vec::new();
    let mut current = help(&[]).expect("zone --help");
    for w in words {
        if w.ends_with(':') {
            // A message the client prints (`zone run: stopped`), not a command line.
            break;
        }
        if let Some(flag) = w.strip_prefix("--") {
            let flag = format!("--{}", flag.split('=').next().unwrap());
            if !current.flags.contains(&flag) {
                errors.push(format!("{doc}: `zone {}` has no {flag}", path.join(" ")));
            }
            continue;
        }
        let bare = w.chars().all(|c| c.is_ascii_lowercase() || c == '-') && !w.starts_with('-');
        if bare && !current.subcommands.is_empty() {
            path.push((*w).to_owned());
            let args: Vec<&str> = path.iter().map(String::as_str).collect();
            match help(&args) {
                Some(h) => current = h,
                None => {
                    errors.push(format!("{doc}: no subcommand `zone {}`", path.join(" ")));
                    break;
                }
            }
        }
    }
    path
}

#[test]
fn every_documented_command_and_flag_exists() {
    let top = help(&[]).expect("zone --help");
    let mut all_flags = top.flags.clone();
    for sub in &top.subcommands {
        let h = help(&[sub]).unwrap();
        all_flags.extend(h.flags.iter().cloned());
        for leaf in &h.subcommands {
            all_flags.extend(help(&[sub, leaf]).unwrap().flags);
        }
    }
    let scripts = script_text();

    let mut errors = Vec::new();
    let mut documented = BTreeSet::new();
    let mut invocations = 0;
    for doc in DOCS {
        let text = fs::read_to_string(repo_root().join(doc)).unwrap();
        for span in code(&text) {
            let words: Vec<&str> = span.split_whitespace().collect();
            // `zone` leads a command line, after at most a prompt or env assignments; elsewhere
            // it is prose (`not a zone code or invite link`).
            let lead = words.iter().position(|w| *w != "$" && !w.contains('='));
            if let Some(i) = lead.filter(|&i| words[i] == "zone") {
                invocations += 1;
                let path = check_invocation(&words[i + 1..], &mut errors, doc);
                if let Some(first) = path.first() {
                    documented.insert(first.clone());
                }
            } else if let Some(flag) = words.first().filter(|w| w.starts_with("--")) {
                let flag = flag.split('=').next().unwrap();
                if !all_flags.contains(flag) && !scripts.contains(flag) {
                    errors.push(format!("{doc}: {flag} is neither zone's nor a script's"));
                }
            }
        }
    }
    assert!(invocations > 10, "found only {invocations} `zone` commands");
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    let missing: Vec<_> = top.subcommands.difference(&documented).collect();
    assert!(missing.is_empty(), "README does not document: {missing:?}");
}

#[test]
fn the_checker_catches_a_made_up_subcommand_and_flag() {
    let mut errors = Vec::new();
    check_invocation(&["join", "--kit", "--frobnicate"], &mut errors, "t");
    check_invocation(&["receipts", "export"], &mut errors, "t");
    check_invocation(&["teleport"], &mut errors, "t");
    check_invocation(&["run:", "--not-a-flag"], &mut errors, "t");
    assert_eq!(
        errors,
        [
            "t: `zone join` has no --frobnicate",
            "t: no subcommand `zone receipts export`",
            "t: no subcommand `zone teleport`",
        ]
    );
}
