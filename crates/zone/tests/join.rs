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

/// `zone` with `stdin` as its standard input.
fn zone_stdin(state: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zone"))
        .arg("--state")
        .arg(state)
        .args(args)
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run zone");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn the_invite_comes_from_stdin_or_a_private_file_and_an_argument_is_warned_about() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let code = unreachable_zone_code();

    // stdin: the same join as the argument, without the warning.
    let out = zone_stdin(
        &state,
        &["join", "--no-wait", "-"],
        format!("{code}\n").as_bytes(),
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Core unreachable"), "{err}");
    assert!(!err.contains("shell history"), "{err}");

    // A file: refused while others can read it, taken at 0600.
    let file = tmp.path().join("invite");
    std::fs::write(&file, format!("  {code}\n")).unwrap();
    let path = file.to_str().unwrap();
    std::fs::set_permissions(&file, Permissions::from_mode(0o644)).unwrap();
    let out = zone(&state, &["join", "--no-wait", "--invite-file", path]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is open to others (mode 644); chmod 600"),
        "{out:?}"
    );
    std::fs::set_permissions(&file, Permissions::from_mode(0o600)).unwrap();
    let out = zone(&state, &["join", "--no-wait", "--invite-file", path]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Core unreachable"),
        "{out:?}"
    );

    // On the command line it still works, with a warning.
    let out = zone(&state, &["join", "--no-wait", &code]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("shows in shell history"), "{err}");
    assert!(err.contains("Core unreachable"), "{err}");
}

#[test]
fn invite_sources_that_cannot_work_are_refused_before_any_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let file = tmp.path().join("invite");
    std::fs::write(&file, unreachable_zone_code()).unwrap();
    std::fs::set_permissions(&file, Permissions::from_mode(0o600)).unwrap();

    // Both the key and the invite on stdin.
    let out = zone_stdin(&state, &["--key-file", "-", "join", "--kit", "-"], &[1; 32]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot both come from stdin"));
    // An argument and a file; neither.
    let out = zone(
        &state,
        &["join", "x", "--invite-file", file.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = zone(&state, &["join"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    // An empty stdin.
    let out = zone_stdin(&state, &["join", "-"], b"\n");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("stdin: empty"));
    assert!(!state.exists());
}
