//! A stand-in for cosign and test release keys, shared by the signing tests. The stand-in signs
//! and verifies with openssl: ECDSA P-256 over the blob itself, as cosign does with a key, with
//! the signature where a cosign bundle carries it, so install.sh's openssl path reads it too. It
//! refuses any call that would reach a transparency log and records every call, so the tests see
//! which key and file the scripts use.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The passphrase the test keys "need": the stand-in only checks that cosign was given one.
pub const PASSWORD: &str = "test-passphrase";

pub fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A release key in `dir`: the private key, the public key and a passphrase file.
pub struct Key {
    pub private: PathBuf,
    pub public: PathBuf,
    pub password_file: PathBuf,
}

pub fn key(dir: &Path, name: &str) -> Key {
    let private = dir.join(format!("{name}.key"));
    let public = dir.join(format!("{name}.pub"));
    let password_file = dir.join(format!("{name}.password"));
    let ok = Command::new("openssl")
        .args([
            "ecparam",
            "-name",
            "prime256v1",
            "-genkey",
            "-noout",
            "-out",
        ])
        .arg(&private)
        .status()
        .unwrap()
        .success()
        && Command::new("openssl")
            .args(["ec", "-pubout", "-in"])
            .arg(&private)
            .arg("-out")
            .arg(&public)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
    assert!(ok, "openssl could not make a test key");
    fs::write(&password_file, PASSWORD).unwrap();
    Key {
        private,
        public,
        password_file,
    }
}

/// A stand-in `cosign` in `dir`. `sign-blob` needs a key, a passphrase and the log turned off;
/// `verify-blob` needs a key and the log check turned off. Every call goes to `dir/cosign.log`,
/// with whether a passphrase was given.
pub fn cosign(dir: &Path) -> PathBuf {
    let path = dir.join("cosign");
    executable(
        &path,
        &format!(
            r#"#!/bin/sh
echo "cosign $* (password: ${{COSIGN_PASSWORD:-none}})" >>'{log}'
cmd="$1"
shift
key= bundle= blob= tlog=
while [ $# -gt 0 ]; do
  case "$1" in
    --key) key="$2"; shift ;;
    --bundle) bundle="$2"; shift ;;
    --tlog-upload=false | --insecure-ignore-tlog=true) tlog=off ;;
    --yes | --use-signing-config=false) ;;
    -*) exit 2 ;;
    *) blob="$1" ;;
  esac
  shift
done
[ "$tlog" = off ] || exit 3
case "$cmd" in
  sign-blob)
    [ -n "${{COSIGN_PASSWORD:-}}" ] || exit 4
    sig="$(openssl dgst -sha256 -sign "$key" "$blob" | openssl base64 -A)" || exit 1
    printf '{{"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","messageSignature":{{"signature":"%s"}}}}' "$sig" >"$bundle"
    ;;
  verify-blob)
    der="$(mktemp)"
    sed -n 's/.*"signature":"\([A-Za-z0-9+/=]*\)".*/\1/p' "$bundle" | openssl base64 -d -A >"$der"
    openssl dgst -sha256 -verify "$key" -signature "$der" "$blob" >/dev/null
    ok=$?
    rm -f "$der"
    exit $ok
    ;;
  *) exit 2 ;;
esac
"#,
            log = dir.join("cosign.log").display()
        ),
    );
    path
}

/// What the stand-in `cosign sign-blob` writes for `blob` signed with `key`. The stand-in goes
/// in `dir` (and logs there).
pub fn bundle(dir: &Path, key: &Key, blob: &Path) -> String {
    let bundle = dir.join("stand-in.bundle");
    let out = Command::new(cosign(dir))
        .args(["sign-blob", "--yes", "--key"])
        .arg(&key.private)
        .args(["--tlog-upload=false", "--bundle"])
        .arg(&bundle)
        .arg(blob)
        .env("COSIGN_PASSWORD", PASSWORD)
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = fs::read_to_string(&bundle).unwrap();
    fs::remove_file(&bundle).unwrap();
    text
}

pub fn cosign_log(dir: &Path) -> String {
    fs::read_to_string(dir.join("cosign.log")).unwrap_or_default()
}
