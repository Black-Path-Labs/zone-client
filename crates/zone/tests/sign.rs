//! scripts/sign.sh, scripts/verify.sh and scripts/sigstore-token.sh with a stand-in cosign and
//! stand-in tokens (tests/common). A changed artifact or sum, an unlisted file and an unsigned
//! release fail verification, and a token for any identity but the release one is refused before
//! cosign runs, so it never reaches the public log.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{cosign, cosign_log, token, IDENTITY};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn dist(dir: &Path) -> PathBuf {
    let dist = dir.join("dist");
    fs::create_dir(&dist).unwrap();
    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"x86_64 build",
    )
    .unwrap();
    fs::write(
        dist.join("zone-0.1.0-aarch64-unknown-linux-musl"),
        b"aarch64 build",
    )
    .unwrap();
    dist
}

fn script(dir: &Path, name: &str, args: &[&str], dist: &Path, env: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(repo_root().join("scripts").join(name));
    cmd.args(args)
        .arg(dist)
        .env("ZONE_COSIGN", cosign(dir))
        .env_remove("ZONE_IDENTITY_TOKEN")
        .env_remove("ZONE_UNSIGNED");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn sign(dir: &Path, dist: &Path) {
    let token = token(dir, IDENTITY, true, "google");
    let out = script(
        dir,
        "sign.sh",
        &[],
        dist,
        &[("ZONE_IDENTITY_TOKEN", &token)],
    );
    assert!(out.status.success(), "{}", stderr(&out));
}

fn verify(dir: &Path, dist: &Path, args: &[&str]) -> Output {
    script(dir, "verify.sh", args, dist, &[])
}

#[test]
fn signed_release_verifies_and_lists_every_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = dist(tmp.path());
    sign(tmp.path(), &dist);

    let sums = fs::read_to_string(dist.join("SHA256SUMS")).unwrap();
    let names: Vec<&str> = sums.lines().map(|l| &l[66..]).collect();
    assert_eq!(
        names,
        [
            "zone-0.1.0-aarch64-unknown-linux-musl",
            "zone-0.1.0-x86_64-unknown-linux-musl"
        ]
    );
    assert!(dist.join("SHA256SUMS.sigstore.json").is_file());
    assert!(!dist.join("UNSIGNED").exists());
    let log = cosign_log(tmp.path());
    assert!(
        log.contains(&format!(
            "cosign sign-blob --yes --identity-token {}",
            tmp.path()
                .join(format!("{IDENTITY}.google.token"))
                .display()
        )),
        "{log}"
    );
    // sign.sh checks its own signature against the pinned identity.
    assert!(
        log.contains(&format!(
            "--certificate-identity {IDENTITY} --certificate-oidc-issuer https://accounts.google.com"
        )),
        "{log}"
    );

    let out = verify(tmp.path(), &dist, &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    // Signing again replaces the sums and signature rather than listing them.
    sign(tmp.path(), &dist);
    assert_eq!(fs::read_to_string(dist.join("SHA256SUMS")).unwrap(), sums);
    assert!(verify(tmp.path(), &dist, &[]).status.success());
}

#[test]
fn modified_artifact_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = dist(tmp.path());
    sign(tmp.path(), &dist);
    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"x86_64 builT",
    )
    .unwrap();
    let out = verify(tmp.path(), &dist, &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("does not match SHA256SUMS"));
}

#[test]
fn modified_sums_or_another_signer_fail() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = dist(tmp.path());
    sign(tmp.path(), &dist);

    // Sums rewritten to match a swapped artifact: the sums check passes, the signature does not.
    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"swapped",
    )
    .unwrap();
    let good_sums = fs::read_to_string(dist.join("SHA256SUMS")).unwrap();
    let swapped = Command::new("sha256sum")
        .arg("zone-0.1.0-x86_64-unknown-linux-musl")
        .current_dir(&dist)
        .output()
        .unwrap();
    let swapped = String::from_utf8(swapped.stdout).unwrap();
    let forged: String = good_sums
        .lines()
        .map(|l| {
            if l.ends_with("x86_64-unknown-linux-musl") {
                swapped.trim_end().to_string()
            } else {
                l.to_string()
            }
        })
        .map(|l| l + "\n")
        .collect();
    fs::write(dist.join("SHA256SUMS"), forged).unwrap();
    let out = verify(tmp.path(), &dist, &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("not a good signature"),
        "{}",
        stderr(&out)
    );

    // Sums restored, but the bundle names someone else.
    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"x86_64 build",
    )
    .unwrap();
    fs::write(dist.join("SHA256SUMS"), &good_sums).unwrap();
    assert!(verify(tmp.path(), &dist, &[]).status.success());
    fs::write(
        dist.join("SHA256SUMS.sigstore.json"),
        common::bundle("someone@example.com", &dist.join("SHA256SUMS")),
    )
    .unwrap();
    assert!(!verify(tmp.path(), &dist, &[]).status.success());
}

#[test]
fn another_identity_is_refused_before_cosign_runs() {
    for (email, verified, connector) in [
        ("someone@example.com", true, "google"),
        (IDENTITY, false, "google"),
        (IDENTITY, true, "github"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let dist = dist(tmp.path());
        let token = token(tmp.path(), email, verified, connector);
        let out = script(
            tmp.path(),
            "sign.sh",
            &[],
            &dist,
            &[("ZONE_IDENTITY_TOKEN", &token)],
        );
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains(&format!(
                "sign in with Google as {IDENTITY}. Nothing was signed"
            )),
            "{}",
            stderr(&out)
        );
        assert_eq!(cosign_log(tmp.path()), "");
        assert!(!dist.join("SHA256SUMS.sigstore.json").exists());
    }
}

#[test]
fn unlisted_file_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = dist(tmp.path());
    sign(tmp.path(), &dist);
    fs::write(dist.join("zone-0.1.0-extra"), b"not signed").unwrap();
    let out = verify(tmp.path(), &dist, &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("zone-0.1.0-extra is not in SHA256SUMS"));
}

#[test]
fn an_unsigned_release_fails_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = dist(tmp.path());
    let out = script(
        tmp.path(),
        "sign.sh",
        &[],
        &dist,
        &[("ZONE_UNSIGNED", Path::new("1"))],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("UNSIGNED"));
    assert!(dist.join("SHA256SUMS").is_file());
    assert!(!dist.join("SHA256SUMS.sigstore.json").exists());
    assert!(fs::read_to_string(dist.join("UNSIGNED"))
        .unwrap()
        .contains("not signed yet"));
    assert_eq!(cosign_log(tmp.path()), "");

    let out = verify(tmp.path(), &dist, &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--allow-unsigned"));
    assert!(verify(tmp.path(), &dist, &["--allow-unsigned"])
        .status
        .success());

    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"changed",
    )
    .unwrap();
    assert!(!verify(tmp.path(), &dist, &["--allow-unsigned"])
        .status
        .success());
}

#[test]
fn the_token_comes_from_the_device_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let id_token = fs::read_to_string(token(tmp.path(), IDENTITY, true, "google")).unwrap();
    // The first token poll is pending, the second answers.
    common::executable(
        &bin.join("curl"),
        &format!(
            r#"#!/bin/sh
echo "curl $*" >>'{dir}/curl.log'
case "$*" in
  */device/code*) echo '{{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri_complete":"https://oidc.test/device?user_code=ABCD-EFGH","interval":0,"expires_in":30}}' ;;
  */token*)
    if [ -f '{dir}/polled' ]; then echo '{{"id_token":"{id_token}"}}'
    else touch '{dir}/polled'; echo '{{"error":"authorization_pending"}}'; fi ;;
esac
"#,
            dir = tmp.path().display()
        ),
    );
    let out_file = tmp.path().join("token");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(repo_root().join("scripts/sigstore-token.sh"))
        .arg(&out_file)
        .env("PATH", path)
        .env("ZONE_SIGSTORE_OIDC", "https://oidc.test/auth")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), IDENTITY);
    assert!(
        stderr(&out).contains("open https://oidc.test/device?user_code=ABCD-EFGH"),
        "{}",
        stderr(&out)
    );
    assert_eq!(fs::read_to_string(&out_file).unwrap().trim(), id_token);
    assert_eq!(
        fs::metadata(&out_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let log = fs::read_to_string(tmp.path().join("curl.log")).unwrap();
    assert!(log.contains("code_challenge_method=S256"), "{log}");
    assert!(log.contains("code_verifier="), "{log}");
}
