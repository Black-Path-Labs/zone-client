//! Scripts/smoke.sh against a release served from a directory. PATH carries a curl that
//! copies from that directory; the release's install.sh is this repo's, and its `zone` is a
//! stand-in that records how it was called and answers `status` from a file. The real join
//! against a live Core runs `scripts/smoke.sh` on the released binary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TAG: &str = "v0.1.0-rc.9";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Release {
    tmp: tempfile::TempDir,
    bin: PathBuf,
    log: PathBuf,
    key: PathBuf,
}

/// A release dir with install.sh and a stand-in `zone` per Linux target, SHA256SUMS over them
/// as scripts/sign.sh writes it unsigned, and a curl on PATH that serves it.
fn release() -> Release {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let release = tmp.path().join("release");
    let log = tmp.path().join("log");
    for d in [&bin, &release] {
        fs::create_dir(d).unwrap();
    }
    let ctl = tmp.path().display().to_string();
    for arch in ["x86_64", "aarch64"] {
        executable(
            &release.join(format!("zone-0.1.0-{arch}-unknown-linux-musl")),
            &format!(
                "#!/bin/sh\necho \"zone $* HOME=$HOME XDG=${{XDG_DATA_HOME:-}}\" >>'{ctl}/log'\n\
                 case \"$*\" in\n  --version) echo 'zone 0.1.0 ({arch})' ;;\n\
                 *' join '*) exit \"$(cat '{ctl}/join-exit' 2>/dev/null || echo 0)\" ;;\n\
                 *' status') cat '{ctl}/status' ;;\n  *) exit 9 ;;\nesac\n"
            ),
        );
    }
    fs::copy(repo_root().join("install.sh"), release.join("install.sh")).unwrap();
    let out = Command::new(repo_root().join("scripts/sign.sh"))
        .arg(&release)
        .env_remove("ZONE_SIGN_KEY")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // curl -fsSL -o <file> <url>: serves <url>'s last path segment from the release dir.
    executable(
        &bin.join("curl"),
        &format!(
            "#!/bin/sh\nout=\nwhile [ $# -gt 1 ]; do\n  [ \"$1\" = -o ] && out=\"$2\"\n  shift\ndone\n\
             echo \"curl $1\" >>'{log}'\nf='{rel}'/\"${{1##*/}}\"\n[ -f \"$f\" ] || exit 22\n\
             cp \"$f\" \"$out\"\n",
            log = log.display(),
            rel = release.display()
        ),
    );
    let key = tmp.path().join("kit.key");
    fs::write(&key, "00").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        tmp.path().join("status"),
        r#"{"state":"admitted","zoneId":"z1"}"#,
    )
    .unwrap();
    Release { tmp, bin, log, key }
}

impl Release {
    fn smoke(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(repo_root().join("scripts/smoke.sh"));
        cmd.args(args)
            .env_clear()
            .env("HOME", self.tmp.path().join("real-home"))
            .env("XDG_DATA_HOME", self.tmp.path().join("real-data"))
            .env("PATH", path)
            .env("ZONE_SMOKE_INVITE", "bpzc1.code")
            .env("ZONE_SMOKE_KEY_FILE", &self.key);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn set(&self, file: &str, body: &str) {
        fs::write(self.tmp.path().join(file), body).unwrap();
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn installs_the_release_into_an_empty_home_and_joins() {
    let r = release();
    let out = r.smoke(&[TAG], &[]);
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    assert!(
        all.lines()
            .last()
            .unwrap()
            .starts_with(&format!("smoke: PASS {TAG} (zone 0.1.0 (")),
        "{all}"
    );
    assert!(all.contains("SHA-256 OK"), "{all}");
    let log = r.log();
    let base = format!("releases/download/{TAG}/");
    assert!(log.contains(&format!("{base}install.sh\n")), "{log}");
    assert!(log.contains(&format!("{base}SHA256SUMS\n")), "{log}");
    // The installed binary, its HOME the throwaway one, the real XDG_DATA_HOME unset.
    let key = r.key.display();
    let join = log
        .lines()
        .find(|l| l.contains(" join "))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(
        join.starts_with(&format!(
            "zone --key-file {key} join --kit --timeout 300 bpzc1.code HOME="
        )),
        "{join}"
    );
    assert!(join.ends_with(" XDG="), "{join}");
    assert!(!join.contains("real-home"), "{join}");
    assert!(
        log.contains(&format!("zone --key-file {key} status HOME=")),
        "{log}"
    );
}

#[test]
fn pending_after_join_fails() {
    let r = release();
    r.set("status", r#"{"state":"pending","zoneId":"z1"}"#);
    let out = r.smoke(&[TAG], &[("ZONE_SMOKE_TIMEOUT", "5")]);
    assert!(!out.status.success());
    assert!(text(&out).contains("smoke: not admitted"), "{}", text(&out));
    assert!(r.log().contains("--timeout 5 bpzc1.code"), "{}", r.log());
}

#[test]
fn a_join_that_times_out_fails() {
    let r = release();
    r.set("join-exit", "4");
    let out = r.smoke(&[TAG], &[]);
    assert!(!out.status.success());
    let all = text(&out);
    assert!(all.contains("smoke: zone join --kit exited 4"), "{all}");
    assert!(!all.contains("PASS"), "{all}");
}

#[test]
fn needs_an_invite_and_a_key_before_downloading() {
    let r = release();
    let out = r.smoke(&[TAG], &[("ZONE_SMOKE_INVITE", "")]);
    assert!(!out.status.success());
    assert!(text(&out).contains("ZONE_SMOKE_INVITE"), "{}", text(&out));
    let out = r.smoke(&[TAG], &[("ZONE_SMOKE_KEY_FILE", "/nonexistent")]);
    assert!(!out.status.success());
    assert!(text(&out).contains("ZONE_SMOKE_KEY_FILE"), "{}", text(&out));
    let out = r.smoke(&["main"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert_eq!(r.log(), "");
}

#[test]
fn install_only_keeps_the_home_and_refuses_a_used_one() {
    let r = release();
    let home = r.tmp.path().join("smoke-home");
    let home_s = home.display().to_string();
    let out = r.smoke(
        &["--install-only", TAG],
        &[("ZONE_SMOKE_HOME", &home_s), ("ZONE_SMOKE_INVITE", "")],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(home.join(".local/bin/zone").is_file());
    assert!(!r.log().contains(" join "), "{}", r.log());
    let out = r.smoke(&["--install-only", TAG], &[("ZONE_SMOKE_HOME", &home_s)]);
    assert!(!out.status.success());
    assert!(text(&out).contains("is not empty"), "{}", text(&out));
}

#[test]
fn a_binary_that_does_not_match_its_sum_is_not_run() {
    let r = release();
    let target = r.tmp.path().join("release");
    for arch in ["x86_64", "aarch64"] {
        let f = target.join(format!("zone-0.1.0-{arch}-unknown-linux-musl"));
        let mut body = fs::read_to_string(&f).unwrap();
        body.push_str("# changed\n");
        fs::write(&f, body).unwrap();
    }
    let out = r.smoke(&[TAG], &[]);
    assert!(!out.status.success());
    let all = text(&out);
    assert!(all.contains("does not match SHA256SUMS"), "{all}");
    assert!(all.contains("smoke: install.sh failed"), "{all}");
    assert!(!r.log().contains(" join "), "{}", r.log());
}
