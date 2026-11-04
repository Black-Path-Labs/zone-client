//! The CLI surface: help, version, and `status`, `run` and `receipts verify` before any join.
//! `zone join` has its own tests (join.rs); status and run against a joined zone are the server
//! repo's e2e, and receipts.rs checks receipts from a file.

use std::process::{Command, Output};

fn zone(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zone"))
        .args(args)
        .output()
        .expect("run zone")
}

#[test]
fn help_lists_every_subcommand() {
    let out = zone(&["--help"]);
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    for sub in [
        "join",
        "status",
        "run",
        "receipts",
        "whoami",
        "note",
        "notes",
        "send-file",
        "files",
        "get",
    ] {
        assert!(help.contains(sub), "--help is missing {sub}:\n{help}");
    }
}

#[test]
fn version_names_the_sdk_revision_from_the_lock() {
    let out = zone(&["--version"]);
    assert!(out.status.success());
    let version = String::from_utf8(out.stdout).unwrap();
    let lock = include_str!("../../../Cargo.lock");
    let marker = "zone-sdk?rev=";
    let start = lock.find(marker).expect("Cargo.lock pins zone-sdk") + marker.len();
    let rev = &lock[start..start + 40];
    assert_eq!(
        version.trim(),
        format!("zone {} (zone-sdk {rev})", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn receipts_verify_before_any_join_names_the_way_out_and_makes_no_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let out = zone_in(&state, &["receipts", "verify"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("no zone joined") && err.contains("--file"),
        "{err}"
    );
    assert!(!state.exists());
}

fn zone_in(state: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zone"))
        .arg("--state")
        .arg(state)
        .args(args)
        .output()
        .expect("run zone")
}

#[test]
fn status_before_any_join_is_not_joined_json_and_makes_no_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let out = zone_in(&state, &["status"]);
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "zoneId": null,
            "pseudonym": null,
            "state": "not-joined",
            "members": 0,
            "lastSyncMs": null,
            "nextSyncMs": null,
            "lastFailure": null,
        })
    );
    assert!(!state.exists());
}

#[test]
fn run_before_any_join_fails_and_makes_no_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let out = zone_in(&state, &["run", "--rounds", "1"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("run `zone join` first"));
    assert!(!state.exists());
}

#[test]
fn notes_and_files_before_any_join_name_the_way_out_and_make_no_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let file = tmp.path().join("f.txt");
    std::fs::write(&file, b"abc").unwrap();
    let file = file.to_str().unwrap();
    for args in [
        &["note", "hello"][..],
        &["notes"],
        &["send-file", file],
        &["files", "--since", "0"],
        &["get", "some-id", "-o", "out.bin"],
    ] {
        let out = zone_in(&state, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains("no zone joined"), "{args:?}: {err}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
    assert!(!state.exists());
}

#[test]
fn an_empty_note_is_refused_before_any_state_is_read() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let out = zone_in(&state, &["note", " "]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stderr).unwrap().contains("empty"));
    assert!(!state.exists());
}
