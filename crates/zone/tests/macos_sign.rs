//! scripts/macos-sign.sh with stand-in uname, codesign, ditto, xcrun and gh on PATH, so
//! it runs off a Mac. The stand-in codesign appends to the binary, as a real signature changes its
//! bytes, so the tests see SHA256SUMS rewritten over the signed binaries. Real signing and
//! notarization run on a Mac with the Developer ID certificate.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const IDENTITY: &str = "Developer ID Application: Test (TEAM123456)";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Mac {
    tmp: tempfile::TempDir,
    bin: PathBuf,
    log: PathBuf,
}

fn tool(bin: &Path, name: &str, body: &str) {
    let path = bin.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Stand-in tools. Each records its arguments in `log`. NOTARY sets the notary's answer, GH_DRAFT
/// whether the release is a draft and GH_ASSETS the directory the release's assets come from.
fn mac() -> Mac {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = tmp.path().join("log");
    let rec = format!("echo \"$(basename \"$0\") $*\" >>'{}'\n", log.display());
    tool(&bin, "uname", "echo \"${FAKE_UNAME:-Darwin}\"\n");
    tool(
        &bin,
        "codesign",
        &format!(
            "{rec}case \"$1\" in --force) for f; do :; done; printf ' signed' >>\"$f\" ;; esac\n"
        ),
    );
    tool(&bin, "ditto", &format!("{rec}cp \"$4\" \"$5\"\n"));
    tool(
        &bin,
        "xcrun",
        &format!(
            "{rec}printf '{{\"id\": \"abc\", \"status\": \"%s\"}}\\n' \"${{NOTARY:-Accepted}}\"\n"
        ),
    );
    tool(
        &bin,
        "gh",
        &format!(
            "{rec}case \"$2\" in\n  view) [ -n \"${{GH_DRAFT:-}}\" ] || exit 1; echo \"$GH_DRAFT\" ;;\n  download) cp \"$GH_ASSETS\"/* \"$5\" ;;\nesac\n"
        ),
    );
    Mac { tmp, bin, log }
}

fn dist(dir: &Path) -> PathBuf {
    let dist = dir.join("dist");
    fs::create_dir(&dist).unwrap();
    for target in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-musl",
    ] {
        fs::write(dist.join(format!("zone-0.1.0-{target}")), target).unwrap();
    }
    dist
}

fn run(mac: &Mac, args: &[&str], env: &[(&str, &str)]) -> Output {
    let path = format!(
        "{}:{}",
        mac.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new(repo_root().join("scripts/macos-sign.sh"));
    cmd.args(args)
        .env("PATH", path)
        .env("ZONE_MACOS_IDENTITY", IDENTITY)
        .env("ZONE_NOTARY_PROFILE", "zone-notary")
        .env("ZONE_UNSIGNED", "1")
        .env_remove("ZONE_COSIGN_KEY")
        .env_remove("ZONE_COSIGN_PASSWORD_FILE")
        .env_remove("NOTARY")
        .env_remove("GH_DRAFT")
        .env_remove("FAKE_UNAME");
    for (k, v) in env {
        if v.is_empty() {
            cmd.env_remove(k);
        } else {
            cmd.env(k, v);
        }
    }
    cmd.output().unwrap()
}

fn log(mac: &Mac) -> String {
    fs::read_to_string(&mac.log).unwrap_or_default()
}

fn sums(dist: &Path) -> Vec<String> {
    fs::read_to_string(dist.join("SHA256SUMS"))
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

fn sha256(path: &Path) -> String {
    let out = Command::new("sha256sum").arg(path).output().unwrap();
    String::from_utf8(out.stdout).unwrap()[..64].to_string()
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn refused(out: &Output, says: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(says), "{stderr}");
}

#[test]
fn signs_and_notarizes_each_macos_binary_and_rewrites_the_sums() {
    let mac = mac();
    let dist = dist(mac.tmp.path());
    ok(&run(&mac, &[dist.to_str().unwrap()], &[]));

    let log = log(&mac);
    for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
        let bin = dist.join(format!("zone-0.1.0-{target}"));
        assert!(
            log.contains(&format!(
                "codesign --force --options runtime --timestamp --sign {IDENTITY} {}",
                bin.display()
            )),
            "{log}"
        );
        assert!(log.contains(&format!(
            "codesign --verify --strict --verbose=2 {}",
            bin.display()
        )));
        assert!(log.contains(&format!(
            "zone-0.1.0-{target}.zip --keychain-profile zone-notary --wait"
        )));
        assert_eq!(
            fs::read_to_string(&bin).unwrap(),
            format!("{target} signed")
        );
    }
    assert!(!log.contains("linux-musl"), "{log}");

    let sums = sums(&dist);
    assert_eq!(sums.len(), 3, "{sums:?}");
    for name in [
        "zone-0.1.0-aarch64-apple-darwin",
        "zone-0.1.0-x86_64-apple-darwin",
        "zone-0.1.0-x86_64-unknown-linux-musl",
    ] {
        let line = format!("{}  {name}", sha256(&dist.join(name)));
        assert!(sums.contains(&line), "{sums:?}");
    }
    assert!(dist.join("UNSIGNED").is_file());
}

#[test]
fn a_rejected_submission_fails_before_the_sums_are_written() {
    let mac = mac();
    let dist = dist(mac.tmp.path());
    let out = run(&mac, &[dist.to_str().unwrap()], &[("NOTARY", "Invalid")]);
    refused(&out, "not accepted by the notary service");
    assert!(!dist.join("SHA256SUMS").exists());
}

#[test]
fn refuses_off_a_mac_or_without_an_identity_or_profile() {
    let mac = mac();
    let dist = dist(mac.tmp.path());
    let d = dist.to_str().unwrap();
    refused(
        &run(&mac, &[d], &[("FAKE_UNAME", "Linux")]),
        "only on macOS",
    );
    refused(
        &run(&mac, &[d], &[("ZONE_MACOS_IDENTITY", "")]),
        "ZONE_MACOS_IDENTITY",
    );
    refused(
        &run(&mac, &[d], &[("ZONE_NOTARY_PROFILE", "")]),
        "ZONE_NOTARY_PROFILE",
    );
    assert_eq!(log(&mac), "");

    let empty = mac.tmp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    refused(
        &run(&mac, &[empty.to_str().unwrap()], &[]),
        "no zone-*-apple-darwin",
    );
    assert_eq!(run(&mac, &[], &[]).status.code(), Some(2));
    assert_eq!(
        run(&mac, &["--release", "0.1.0"], &[]).status.code(),
        Some(2)
    );
}

#[test]
fn release_mode_replaces_the_draft_assets() {
    let mac = mac();
    let assets = dist(mac.tmp.path());
    fs::write(assets.join("UNSIGNED"), "unsigned").unwrap();
    let a = assets.to_str().unwrap();

    let out = run(
        &mac,
        &["--release", "v0.1.0-rc.4"],
        &[("GH_DRAFT", "true"), ("GH_ASSETS", a)],
    );
    ok(&out);
    let log = log(&mac);
    let upload = log
        .lines()
        .find(|l| l.starts_with("gh release upload v0.1.0-rc.4 --clobber "))
        .unwrap_or_else(|| panic!("{log}"));
    let names: Vec<&str> = upload
        .split(' ')
        .skip(5)
        .map(|p| p.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "zone-0.1.0-aarch64-apple-darwin",
            "zone-0.1.0-x86_64-apple-darwin",
            "SHA256SUMS",
            "UNSIGNED"
        ]
    );
    // The fixture is the release; only the download was changed.
    assert_eq!(
        fs::read_to_string(assets.join("zone-0.1.0-x86_64-apple-darwin")).unwrap(),
        "x86_64-apple-darwin"
    );
}

#[test]
fn release_mode_refuses_a_published_release() {
    let mac = mac();
    let assets = dist(mac.tmp.path());
    let a = assets.to_str().unwrap();
    let tag = ["--release", "v0.1.0-rc.4"];

    refused(&run(&mac, &tag, &[]), "no release v0.1.0-rc.4");
    refused(
        &run(&mac, &tag, &[("GH_DRAFT", "false"), ("GH_ASSETS", a)]),
        "only a draft is re-signed",
    );
    let log = log(&mac);
    assert!(!log.contains("codesign"), "{log}");
    assert!(!log.contains("upload"), "{log}");
}

#[test]
fn release_mode_signs_the_new_sums_with_the_release_key() {
    let mac = mac();
    let assets = dist(mac.tmp.path());
    fs::write(assets.join("SHA256SUMS"), "stale").unwrap();
    fs::write(assets.join("SHA256SUMS.cosign.bundle"), "stale").unwrap();
    let a = assets.to_str().unwrap();
    let cosign = common::cosign(&mac.bin);
    let key = common::key(mac.tmp.path(), "release");

    let out = run(
        &mac,
        &["--release", "v0.1.0-rc.4"],
        &[
            ("GH_DRAFT", "true"),
            ("GH_ASSETS", a),
            ("ZONE_UNSIGNED", "1"),
            ("ZONE_COSIGN", cosign.to_str().unwrap()),
            ("ZONE_COSIGN_KEY", key.private.to_str().unwrap()),
            (
                "ZONE_COSIGN_PASSWORD_FILE",
                key.password_file.to_str().unwrap(),
            ),
            ("ZONE_COSIGN_PUB", key.public.to_str().unwrap()),
        ],
    );
    ok(&out);
    let log = log(&mac);
    let upload = log
        .lines()
        .find(|l| l.starts_with("gh release upload v0.1.0-rc.4 --clobber "))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(upload.ends_with("/SHA256SUMS.cosign.bundle"), "{upload}");
    assert!(
        log.contains("gh release delete-asset v0.1.0-rc.4 UNSIGNED"),
        "{log}"
    );
    assert!(common::cosign_log(&mac.bin).contains("cosign sign-blob"));
}
