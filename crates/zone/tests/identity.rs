//! The state dir, the kit key's sources, `zone whoami`, and that no subcommand ever
//! prints key material. The test key is bytes 1..=32, a key nobody uses.

use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn test_seed() -> Vec<u8> {
    (1..=32).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn b64(b: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, x)| n | (u32::from(*x) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(alphabet[(n >> (18 - 6 * i)) as usize & 63] as char);
        }
        if pad {
            out.push_str(&"=".repeat(3 - chunk.len()));
        }
    }
    out
}

/// Every printable form a 32-byte key could leak as.
fn secret_forms() -> Vec<String> {
    let seed = test_seed();
    let std = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let url = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let h = hex(&seed);
    vec![
        h.clone(),
        h.to_uppercase(),
        b64(&seed, std, true),
        b64(&seed, url, false),
        // Any 8-byte window of the hex form: a truncated echo is still a leak.
        h[..16].to_string(),
        h[24..40].to_string(),
        h[48..].to_string(),
        format!("{seed:?}"),
    ]
}

fn assert_clean(out: &Output, what: &str) {
    for (name, stream) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
        let seed = test_seed();
        assert!(
            !stream.windows(seed.len()).any(|w| w == seed.as_slice()),
            "{what}: raw key bytes on {name}"
        );
        let text = String::from_utf8_lossy(stream);
        for form in secret_forms() {
            assert!(!text.contains(&form), "{what}: {form} on {name}:\n{text}");
        }
    }
}

fn key_file(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::File::create(&path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

fn zone(state: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
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
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(stdin.unwrap_or_default()).unwrap();
    drop(pipe);
    child.wait_with_output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn whoami_without_a_device_makes_no_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let out = zone(&state, &["whoami"], None);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout(&out).contains("device: none yet"),
        "{}",
        stdout(&out)
    );
    assert!(!state.exists(), "whoami must not create state");
}

#[test]
fn whoami_prints_the_same_kit_fingerprint_from_file_and_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let raw = key_file(tmp.path(), "raw.key", &test_seed(), 0o600);
    let text = format!("{}\n", hex(&test_seed()));
    let hexed = key_file(tmp.path(), "hex.key", text.as_bytes(), 0o600);

    let kit_line = |out: Output| {
        assert!(out.status.success(), "{out:?}");
        assert_clean(&out, "whoami");
        let s = stdout(&out);
        s.lines()
            .find(|l| l.starts_with("kit: "))
            .unwrap_or_else(|| panic!("no kit line:\n{s}"))
            .to_owned()
    };
    let a = kit_line(zone(
        &state,
        &["whoami", "--key-file", raw.to_str().unwrap()],
        None,
    ));
    let b = kit_line(zone(
        &state,
        &["whoami", "--key-file", hexed.to_str().unwrap()],
        None,
    ));
    let c = kit_line(zone(
        &state,
        &["--key-file", "-", "whoami"],
        Some(&test_seed()),
    ));
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn a_key_file_others_can_read_is_refused_everywhere() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let open = key_file(tmp.path(), "open.key", &test_seed(), 0o644);
    for args in [&["whoami"][..], &["status"], &["join", "x"]] {
        let mut all = args.to_vec();
        all.extend(["--key-file", open.to_str().unwrap()]);
        let out = zone(&state, &all, None);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("chmod 600"));
        assert_clean(&out, "open key file");
    }
}

#[test]
fn a_state_dir_others_can_enter_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, Permissions::from_mode(0o755)).unwrap();
    let out = zone(&state, &["whoami"], None);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("chmod 700"));
}

#[test]
fn the_key_is_not_an_argv_or_env_option() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let h = hex(&test_seed());
    for flag in ["--key", "--kit-key", "--token"] {
        let out = zone(&state, &[flag, &h, "whoami"], None);
        assert_eq!(out.status.code(), Some(2), "{flag} must not parse");
        assert_clean(&out, flag);
    }
}

#[test]
fn no_subcommand_prints_key_material() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("zone");
    let file = key_file(tmp.path(), "kit.key", &test_seed(), 0o600);
    let mut bad = hex(&test_seed());
    bad.push_str("00");
    let malformed = key_file(tmp.path(), "bad.key", bad.as_bytes(), 0o600);
    let subcommands: [&[&str]; 8] = [
        &["--help"],
        &["--version"],
        &["join", "zone-invite-placeholder"],
        &["status"],
        &["run"],
        &["receipts", "verify"],
        &["whoami"],
        &["whoami", "--help"],
    ];
    for sub in subcommands {
        for (source, stdin) in [
            (file.to_str().unwrap(), None),
            ("-", Some(test_seed())),
            ("-", Some(bad.clone().into_bytes())),
            (malformed.to_str().unwrap(), None),
        ] {
            let mut args = vec!["--key-file", source];
            args.extend(sub);
            let out = zone(&state, &args, stdin.as_deref());
            assert_clean(&out, &format!("{args:?}"));
        }
    }
}
