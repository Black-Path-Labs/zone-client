//! Install.sh under dash against a release served from a directory. PATH carries a curl
//! and a gh that copy from that directory and record what they were asked for. A changed binary,
//! a bad signature and an unsigned release once a key is set all fail and install nothing.
//! minisign is ZONE_MINISIGN or minisign on PATH (scripts/release-tools.sh).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn minisign() -> String {
    let bin = std::env::var("ZONE_MINISIGN").unwrap_or_else(|_| "minisign".into());
    let found = Command::new(&bin).arg("-v").output();
    assert!(
        found.is_ok_and(|o| o.status.success()),
        "no minisign at {bin:?}: run eval \"$(scripts/release-tools.sh <dir>)\""
    );
    bin
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Release {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    release: PathBuf,
    log: PathBuf,
}

/// A release dir with a stand-in `zone` per Linux target and SHA256SUMS over them, as
/// scripts/sign.sh writes it unsigned.
fn release() -> Release {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let bin = tmp.path().join("bin");
    let release = tmp.path().join("release");
    let log = tmp.path().join("log");
    for d in [&home, &bin, &release] {
        fs::create_dir(d).unwrap();
    }
    for arch in ["x86_64", "aarch64"] {
        executable(
            &release.join(format!("zone-0.1.0-{arch}-unknown-linux-musl")),
            &format!("#!/bin/sh\necho 'zone 0.1.0 ({arch})'\n"),
        );
    }
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
    // gh release download [tag] -R <repo> -D <dir> -p <glob>...: copies every match, fails on none.
    executable(
        &bin.join("gh"),
        &format!(
            "#!/bin/sh\necho \"gh $*\" >>'{log}'\ndir=\npats=\nwhile [ $# -gt 0 ]; do\n\
             case \"$1\" in\n  -D) dir=\"$2\"; shift ;;\n  -p) pats=\"$pats $2\"; shift ;;\n\
             esac\n  shift\ndone\nn=0\nfor f in '{rel}'/*; do\n  set -f\n  for p in $pats; do\n\
             case \"${{f##*/}}\" in $p) cp \"$f\" \"$dir\"/; n=$((n + 1)) ;; esac\n  done\ndone\n\
             [ \"$n\" -gt 0 ]\n",
            log = log.display(),
            rel = release.display()
        ),
    );
    Release {
        _tmp: tmp,
        home,
        bin,
        release,
        log,
    }
}

impl Release {
    fn install(&self, env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("dash");
        cmd.arg(repo_root().join("install.sh"))
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn installed(&self) -> PathBuf {
        self.home.join(".local/bin/zone")
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Signs SHA256SUMS with a new key, puts minisign on the scripts' PATH, returns the public key.
    fn sign(&self) -> String {
        let key = self.release.join("../zone.key");
        let public = self.release.join("../zone.pub");
        let out = Command::new(minisign())
            .args(["-G", "-W", "-p"])
            .arg(&public)
            .arg("-s")
            .arg(&key)
            .output()
            .unwrap();
        assert!(out.status.success());
        fs::remove_file(self.release.join("UNSIGNED")).unwrap();
        let out = Command::new(minisign())
            .args(["-S", "-s"])
            .arg(&key)
            .arg("-m")
            .arg(self.release.join("SHA256SUMS"))
            .output()
            .unwrap();
        assert!(out.status.success());
        self.with_minisign();
        let public = fs::read_to_string(&public).unwrap();
        public.lines().nth(1).unwrap().to_string()
    }

    fn with_minisign(&self) {
        let path = Command::new("sh")
            .args(["-c", "command -v \"$1\"", "sh", &minisign()])
            .output()
            .unwrap();
        let path = String::from_utf8(path.stdout).unwrap();
        std::os::unix::fs::symlink(path.trim(), self.bin.join("minisign")).unwrap();
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        _ => "x86_64",
    }
}

#[test]
fn installs_latest_into_local_bin() {
    let r = release();
    let out = r.install(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Next: zone join <invite>"), "{stdout}");
    assert!(stderr(&out).contains("UNSIGNED"), "{}", stderr(&out));
    assert!(stderr(&out).contains("SHA-256 OK"), "{}", stderr(&out));
    assert!(stderr(&out).contains("add "), "{}", stderr(&out));

    let zone = r.installed();
    assert_eq!(
        fs::metadata(&zone).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let version = Command::new(&zone).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("zone 0.1.0 ({})", host_arch())
    );
    let log = r.log();
    assert!(
        log.contains("releases/latest/download/SHA256SUMS\n"),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "releases/latest/download/zone-0.1.0-{}-unknown-linux-musl\n",
            host_arch()
        )),
        "{log}"
    );
}

#[test]
fn pinned_version_and_install_dir() {
    let r = release();
    let dir = r.home.join("tools");
    let out = r.install(&[
        ("ZONE_VERSION", "v0.1.0"),
        ("ZONE_INSTALL_DIR", dir.to_str().unwrap()),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(dir.join("zone").is_file());
    assert!(!r.installed().exists());
    assert!(
        r.log()
            .contains("Black-Path-Labs/zone-client/releases/download/v0.1.0/SHA256SUMS"),
        "{}",
        r.log()
    );
}

#[test]
fn gh_download_reaches_a_private_release() {
    let r = release();
    let out = r.install(&[("ZONE_GH", "1"), ("ZONE_VERSION", "v0.1.0-rc.1")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(r.installed().is_file());
    let log = r.log();
    assert!(
        log.starts_with("gh release download v0.1.0-rc.1 -R Black-Path-Labs/zone-client -D "),
        "{log}"
    );
    assert!(!log.contains("curl"), "{log}");
}

#[test]
fn modified_binary_installs_nothing() {
    let r = release();
    let name = format!("zone-0.1.0-{}-unknown-linux-musl", host_arch());
    fs::write(r.release.join(name), "#!/bin/sh\necho swapped\n").unwrap();
    let out = r.install(&[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("does not match SHA256SUMS"),
        "{}",
        stderr(&out)
    );
    assert!(!r.installed().exists());
}

#[test]
fn signed_release_checks_the_signature() {
    let r = release();
    let public = r.sign();
    let out = r.install(&[("ZONE_PUBKEY", &public)]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("signature OK"), "{}", stderr(&out));

    // A signature that does not cover these sums.
    let r = release();
    r.sign();
    let other = release().sign();
    let out = r.install(&[("ZONE_PUBKEY", &other)]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("not a good signature"),
        "{}",
        stderr(&out)
    );
    assert!(!r.installed().exists());
}

#[test]
fn without_minisign_prints_how_to_check() {
    let r = release();
    let public = r.sign();
    fs::remove_file(r.bin.join("minisign")).unwrap();
    let out = r.install(&[("ZONE_PUBKEY", &public)]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!("minisign -Vm SHA256SUMS -P {public}")),
        "{}",
        stderr(&out)
    );
}

#[test]
fn unsigned_release_is_refused_once_a_key_is_set() {
    let r = release();
    let public = release().sign();
    let out = r.install(&[("ZONE_PUBKEY", &public)]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("ZONE_ALLOW_UNSIGNED=1"),
        "{}",
        stderr(&out)
    );
    assert!(!r.installed().exists());

    let out = r.install(&[("ZONE_PUBKEY", &public), ("ZONE_ALLOW_UNSIGNED", "1")]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn posix_sh_and_no_sudo() {
    let script = fs::read_to_string(repo_root().join("install.sh")).unwrap();
    assert!(script.starts_with("#!/bin/sh\n"));
    let code: Vec<&str> = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    assert!(
        !code.iter().any(|l| l.contains("sudo")),
        "install.sh uses sudo"
    );
    let out = Command::new("shellcheck")
        .args(["-s", "sh"])
        .arg(repo_root().join("install.sh"))
        .output();
    let out = out.expect("shellcheck is not installed");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
