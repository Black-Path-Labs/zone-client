//! `zone join` without a Core: what it refuses before making state, and that a Core it
//! cannot reach fails the join rather than hanging. The real join against a Core is
//! `scripts/smoke.sh` on a released binary. The test key is bytes 1..=32, a key nobody uses.
#![cfg(unix)]

use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use bp_workspace_core::space::{join_address, ZoneCode};

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

/// A magic link for the unreachable zone, made here with a fresh secret; `tamper` swaps in the
/// id another secret derives.
fn unreachable_magic_link(tamper: bool) -> String {
    use bp_workspace_core::magic_link::{new_secret, MagicLink};
    let zone = ZoneCode::decode(&unreachable_zone_code()).unwrap();
    let (secret, keys) = new_secret();
    let link_id = if tamper {
        new_secret().1.link_id
    } else {
        keys.link_id
    };
    MagicLink {
        zone,
        link_id,
        secret,
        expires_at: "2099-01-01T00:00:00.000Z".into(),
        label: "Test Person".into(),
    }
    .encode()
    .unwrap()
}

#[test]
fn a_magic_link_is_taken_from_a_private_file_and_a_tampered_one_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let file = tmp.path().join("link.txt");
    std::fs::write(&file, unreachable_magic_link(false)).unwrap();
    std::fs::set_permissions(&file, Permissions::from_mode(0o600)).unwrap();
    let path = file.to_str().unwrap();
    // A good link gets as far as Core.
    let out = zone(&state, &["join", "--no-wait", "--invite-file", path]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Core unreachable"), "{err}");
    // A link whose id is not its secret's is refused before Core.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    std::fs::write(&file, unreachable_magic_link(true)).unwrap();
    let out = zone(&state, &["join", "--no-wait", "--invite-file", path]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("Core unreachable"), "{err}");
    assert_ne!(out.status.code(), Some(0), "{out:?}");
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

/// A one-request Core on 127.0.0.1 that answers `GET /v1/join-addresses/<address>` with
/// `status` and `body`, and hands back the request line it got.
fn fake_core(status: u16, body: String) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let served = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut header = String::new();
        while reader.read_line(&mut header).unwrap() > 2 {
            header.clear();
        }
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        line.trim().to_owned()
    });
    (url, served)
}

#[test]
fn a_join_address_is_resolved_at_core_and_checked_before_any_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let code = unreachable_zone_code();
    let address = join_address(&code);

    // The zone code the address stands for: the join goes on to that zone's (unreachable) Core.
    let body = format!(r#"{{"address":"{address}","zoneCode":"{code}"}}"#);
    let (core, served) = fake_core(200, body);
    let out = zone(&state, &["join", "--no-wait", "--core", &core, &address]);
    assert_eq!(
        served.join().unwrap(),
        format!("GET /v1/join-addresses/{address} HTTP/1.1")
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Core unreachable"), "{err}");
    // A join address is a secret like an invite: on the command line it draws the warning.
    assert!(err.contains("shell history"), "{err}");
    // From a 0600 file: the same join, no warning.
    let file = tmp.path().join("address");
    std::fs::write(&file, format!("{address}\n")).unwrap();
    std::fs::set_permissions(&file, Permissions::from_mode(0o600)).unwrap();
    let body = format!(r#"{{"address":"{address}","zoneCode":"{code}"}}"#);
    let (core, served) = fake_core(200, body);
    let out = zone(
        &tmp.path().join("zone-file"),
        &[
            "join",
            "--no-wait",
            "--core",
            &core,
            "--invite-file",
            file.to_str().unwrap(),
        ],
    );
    served.join().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Core unreachable"), "{err}");
    assert!(!err.contains("shell history"), "{err}");

    // A code that does not hash to the address is refused, and nothing is made.
    let other = ZoneCode {
        workspace_id: format!("wks_{}", "3".repeat(26)),
        ..ZoneCode::decode(&code).unwrap()
    }
    .encode()
    .unwrap();
    let state = tmp.path().join("zone-2");
    let body = format!(r#"{{"address":"{address}","zoneCode":"{other}"}}"#);
    let (core, served) = fake_core(200, body);
    let out = zone(&state, &["join", "--core", &core, &address]);
    served.join().unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not hash"));
    assert!(!state.exists());

    // An address nobody publishes is Core's 404: refused, nothing made.
    let body = r#"{"error":{"code":"NO_PATH","message":"no such workspace resource"}}"#;
    let (core, served) = fake_core(404, body.to_owned());
    let out = zone(&state, &["join", "--core", &core, &address]);
    served.join().unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("core-404"));
    assert!(!state.exists());

    // A Core that cannot be reached fails the join with exit 1.
    let out = zone(&state, &["join", "--core", "http://127.0.0.1:9", &address]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Core unreachable"));
    assert!(!state.exists());
}

#[test]
fn a_bad_manifest_is_refused_before_any_state_or_core() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let code = unreachable_zone_code();
    for (body, want) in [
        (&br#"{"name":"Aly"}"#[..], "want exactly"),
        (
            br#"{"name":"Aly","capabilities":["a","a"]}"#,
            "want exactly",
        ),
        (
            br#"{"name":"Aly","capabilities":[],"price":1}"#,
            "want exactly",
        ),
        (b"not json", "not JSON"),
    ] {
        let manifest = tmp.path().join("agent.json");
        std::fs::write(&manifest, body).unwrap();
        let out = zone(
            &state,
            &["join", "--manifest", manifest.to_str().unwrap(), &code],
        );
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("manifest") && err.contains(want), "{err}");
        assert!(!err.contains("Core unreachable"), "{err}");
    }
    let out = zone(&state, &["join", "--name", "", &code]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("a name is 1 to 64 bytes"));
    let missing = tmp.path().join("missing.json");
    let out = zone(
        &state,
        &["join", "--manifest", missing.to_str().unwrap(), &code],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(!state.exists());
}

#[test]
fn name_and_agent_flags_that_cannot_work_together_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let code = unreachable_zone_code();
    let manifest = tmp.path().join("agent.json");
    std::fs::write(&manifest, br#"{"name":"Aly","capabilities":[]}"#).unwrap();
    for args in [
        // A manifest already names the seat.
        &[
            "join",
            "--manifest",
            manifest.to_str().unwrap(),
            "--name",
            "Aly",
        ][..],
        // An agent id picks one of a kit's agents: no kit, no agent id.
        &["join", "--agent-id", "aly"],
    ] {
        let mut all = args.to_vec();
        all.push(&code);
        let out = zone(&state, &all);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
    }
    assert!(!state.exists());
}
