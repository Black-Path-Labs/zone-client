//! The CLI surface: help, version, and the not-implemented exit code of every subcommand that
//! has not landed yet.

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
    for args in [
        &["join", "zone-invite-placeholder"][..],
        &["status"],
        &["run"],
        &["receipts", "verify"],
    ] {
        let out = zone(args);
        assert_eq!(out.status.code(), Some(2), "zone {args:?}");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains("not implemented"), "zone {args:?}: {err}");
    }
}
