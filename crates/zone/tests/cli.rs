//! The CLI surface: help, version, the not-implemented exit code of every subcommand that has
//! not landed yet, and `status` and `run` before any join. `zone join` has its own tests
//! (join.rs); status and run against a joined zone run in `scripts/smoke.sh`.

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
    for sub in ["join", "status", "run", "receipts", "whoami"] {
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
fn unimplemented_subcommands_exit_2() {
    for args in [&["receipts", "verify"][..]] {
        let out = zone(args);
        assert_eq!(out.status.code(), Some(2), "zone {args:?}");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains("not implemented"), "zone {args:?}: {err}");
    }
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
