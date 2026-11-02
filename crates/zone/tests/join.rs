//! `zone join` without a Core: what it refuses before making state, and that a Core it
//! cannot reach fails the join rather than hanging. The real join against a Core is
//! `scripts/smoke.sh` on a released binary. The test key is bytes 1..=32, a key nobody uses.
#![cfg(unix)]

use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use bp_workspace_core::space::ZoneCode;

/// A zone code for a Core nobody listens on (port 9, discard).
fn unreachable_zone_code() -> String {
    ZoneCode {
        workspace_id: format!("wks_{}", "2".repeat(26)),
        owner_bpid: format!("bp:self:{}", "a".repeat(64)),
        owner_fingerprint: format!("sha256:{}", "b".repeat(64)),
        core_zone: "ca-mb-1".into(),
        core_url: "http://127.0.0.1:9".into(),
    }
    .encode()
    .unwrap()
}

fn zone(state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zone"))
        .arg("--state")
        .arg(state)
        .args(args)
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null())
        .output()
        .expect("run zone")
}

fn kit_file(dir: &Path) -> String {
    let path = dir.join("kit.key");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&(1..=32).collect::<Vec<u8>>()).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    path.to_str().unwrap().to_owned()
}

#[test]
fn not_a_code_is_refused_before_any_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    for code in [
        "zone-invite-placeholder",
        "bpzc1.",
        "https://zone.example/#join=bpzi1.x",
    ] {
        let out = zone(&state, &["join", code]);
        assert_eq!(out.status.code(), Some(1), "{code}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("not a zone code or invite link"));
    }
    assert!(!state.exists());
}

#[test]
fn kit_without_a_key_file_is_refused_before_any_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let out = zone(&state, &["join", "--kit", &unreachable_zone_code()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--key-file"));
    assert!(!state.exists());
}

#[test]
fn an_unreachable_core_fails_the_join_with_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let key = kit_file(tmp.path());
    for args in [
        &["join", "--no-wait"][..],
        &["--key-file", &key, "join", "--kit", "--timeout", "1"],
    ] {
        let mut all = args.to_vec();
        let code = unreachable_zone_code();
        all.push(&code);
        let out = zone(&state, &all);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("Core unreachable"), "{err}");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
    }
}
