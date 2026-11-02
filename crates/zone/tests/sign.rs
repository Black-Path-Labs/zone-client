//! scripts/sign.sh and scripts/verify.sh with a stand-in cosign and test keys (tests/common). A
//! changed artifact or sum, an unlisted file, another key's signature and an unsigned release
//! fail verification; sign.sh refuses to run without the key and its passphrase, never asks for a
//! transparency log entry, and keeps no signature that does not verify against the public key.
//! With ZONE_TEST_COSIGN naming a real cosign, one test signs and verifies with it.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{cosign, cosign_log, key, Key, PASSWORD};

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

/// Runs a script on `dist` with the stand-in cosign, `public` as the release key and `env`.
fn script(
    dir: &Path,
    name: &str,
    args: &[&str],
    dist: &Path,
    public: &Path,
    env: &[(&str, &Path)],
) -> Output {
    let mut cmd = Command::new(repo_root().join("scripts").join(name));
    cmd.args(args)
        .arg(dist)
        .env("ZONE_COSIGN", cosign(dir))
        .env("ZONE_COSIGN_PUB", public)
        .env_remove("ZONE_COSIGN_KEY")
        .env_remove("ZONE_COSIGN_PASSWORD_FILE")
        .env_remove("COSIGN_PASSWORD")
        .env_remove("ZONE_UNSIGNED");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn sign_with(dir: &Path, dist: &Path, signer: &Key, public: &Path) -> Output {
    script(
        dir,
        "sign.sh",
        &[],
        dist,
        public,
        &[
            ("ZONE_COSIGN_KEY", &signer.private),
            ("ZONE_COSIGN_PASSWORD_FILE", &signer.password_file),
        ],
    )
}

fn sign(dir: &Path, dist: &Path, k: &Key) {
    let out = sign_with(dir, dist, k, &k.public);
    assert!(out.status.success(), "{}", stderr(&out));
}

fn verify(dir: &Path, dist: &Path, k: &Key, args: &[&str]) -> Output {
    script(dir, "verify.sh", args, dist, &k.public, &[])
}

#[test]
fn signed_release_verifies_and_lists_every_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let k = key(tmp.path(), "release");
    let dist = dist(tmp.path());
    sign(tmp.path(), &dist, &k);

    let sums = fs::read_to_string(dist.join("SHA256SUMS")).unwrap();
    let names: Vec<&str> = sums.lines().map(|l| &l[66..]).collect();
    assert_eq!(
        names,
        [
            "zone-0.1.0-aarch64-unknown-linux-musl",
            "zone-0.1.0-x86_64-unknown-linux-musl"
        ]
    );
    assert!(dist.join("SHA256SUMS.cosign.bundle").is_file());
    assert!(!dist.join("UNSIGNED").exists());
    let log = cosign_log(tmp.path());
    assert!(
        log.contains(&format!(
            "cosign sign-blob --yes --key {} --use-signing-config=false --tlog-upload=false \
             --bundle {}/SHA256SUMS.cosign.bundle {}/SHA256SUMS (password: {PASSWORD})",
            k.private.display(),
            dist.display(),
            dist.display()
        )),
        "{log}"
    );
    // sign.sh checks its own signature against the public key.
    assert!(
        log.contains(&format!(
            "cosign verify-blob --key {} --bundle",
            k.public.display()
        )),
        "{log}"
    );

    let out = verify(tmp.path(), &dist, &k, &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    // Signing again replaces the sums and signature rather than listing them.
    sign(tmp.path(), &dist, &k);
    assert_eq!(fs::read_to_string(dist.join("SHA256SUMS")).unwrap(), sums);
    assert!(verify(tmp.path(), &dist, &k, &[]).status.success());
}

#[test]
fn modified_artifact_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let k = key(tmp.path(), "release");
    let dist = dist(tmp.path());
    sign(tmp.path(), &dist, &k);
    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"x86_64 builT",
    )
    .unwrap();
    let out = verify(tmp.path(), &dist, &k, &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("does not match SHA256SUMS"));
}

#[test]
fn modified_sums_or_another_key_fail() {
    let tmp = tempfile::tempdir().unwrap();
    let k = key(tmp.path(), "release");
    let dist = dist(tmp.path());
    sign(tmp.path(), &dist, &k);

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
    let out = verify(tmp.path(), &dist, &k, &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("not a good signature of SHA256SUMS by the release key"),
        "{}",
        stderr(&out)
    );

    // Sums restored, but signed with another key.
    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"x86_64 build",
    )
    .unwrap();
    fs::write(dist.join("SHA256SUMS"), &good_sums).unwrap();
    assert!(verify(tmp.path(), &dist, &k, &[]).status.success());
    let other = key(tmp.path(), "other");
    fs::write(
        dist.join("SHA256SUMS.cosign.bundle"),
        common::bundle(tmp.path(), &other, &dist.join("SHA256SUMS")),
    )
    .unwrap();
    assert!(!verify(tmp.path(), &dist, &k, &[]).status.success());
}

#[test]
fn a_key_that_is_not_the_published_one_leaves_no_signature() {
    let tmp = tempfile::tempdir().unwrap();
    let k = key(tmp.path(), "release");
    let other = key(tmp.path(), "other");
    let dist = dist(tmp.path());
    let out = sign_with(tmp.path(), &dist, &other, &k.public);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("ZONE_COSIGN_KEY is not the release key"),
        "{}",
        stderr(&out)
    );
    assert!(!dist.join("SHA256SUMS.cosign.bundle").exists());
}

#[test]
fn without_the_key_or_its_passphrase_nothing_is_signed() {
    let tmp = tempfile::tempdir().unwrap();
    let k = key(tmp.path(), "release");
    let dist = dist(tmp.path());
    for (env, want) in [
        (vec![], "set ZONE_COSIGN_KEY"),
        (
            vec![("ZONE_COSIGN_KEY", k.private.as_path())],
            "set COSIGN_PASSWORD or ZONE_COSIGN_PASSWORD_FILE",
        ),
        (
            vec![
                ("ZONE_COSIGN_KEY", k.private.as_path()),
                ("ZONE_COSIGN_PASSWORD_FILE", Path::new("/nonexistent")),
            ],
            "set COSIGN_PASSWORD or ZONE_COSIGN_PASSWORD_FILE",
        ),
    ] {
        let out = script(tmp.path(), "sign.sh", &[], &dist, &k.public, &env);
        assert!(!out.status.success());
        assert!(stderr(&out).contains(want), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("Nothing was signed"),
            "{}",
            stderr(&out)
        );
        assert_eq!(cosign_log(tmp.path()), "");
        assert!(!dist.join("SHA256SUMS.cosign.bundle").exists());
    }
}

#[test]
fn unlisted_file_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let k = key(tmp.path(), "release");
    let dist = dist(tmp.path());
    sign(tmp.path(), &dist, &k);
    fs::write(dist.join("zone-0.1.0-extra"), b"not signed").unwrap();
    let out = verify(tmp.path(), &dist, &k, &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("zone-0.1.0-extra is not in SHA256SUMS"));
}

#[test]
fn an_unsigned_release_fails_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let k = key(tmp.path(), "release");
    let dist = dist(tmp.path());
    let out = script(
        tmp.path(),
        "sign.sh",
        &[],
        &dist,
        &k.public,
        &[("ZONE_UNSIGNED", Path::new("1"))],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("UNSIGNED"));
    assert!(dist.join("SHA256SUMS").is_file());
    assert!(!dist.join("SHA256SUMS.cosign.bundle").exists());
    assert!(fs::read_to_string(dist.join("UNSIGNED"))
        .unwrap()
        .contains("not signed yet"));
    assert_eq!(cosign_log(tmp.path()), "");

    let out = verify(tmp.path(), &dist, &k, &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--allow-unsigned"));
    assert!(verify(tmp.path(), &dist, &k, &["--allow-unsigned"])
        .status
        .success());

    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"changed",
    )
    .unwrap();
    assert!(!verify(tmp.path(), &dist, &k, &["--allow-unsigned"])
        .status
        .success());
}

/// install.sh carries the same release key as cosign.pub, and it is a P-256 public key.
#[test]
fn install_sh_embeds_the_published_key() {
    let published = fs::read_to_string(repo_root().join("cosign.pub")).unwrap();
    let install = fs::read_to_string(repo_root().join("install.sh")).unwrap();
    assert!(
        install.contains(&format!("release_key=\"{}\"", published.trim_end())),
        "install.sh's release_key is not cosign.pub"
    );
    let out = Command::new("openssl")
        .args(["pkey", "-pubin", "-noout", "-text", "-in"])
        .arg(repo_root().join("cosign.pub"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("prime256v1") || text.contains("P-256"),
        "{text}"
    );
}

/// The real cosign (ZONE_TEST_COSIGN): a key it made signs with no log entry, and the bundle
/// verifies with cosign and with the openssl recipe install.sh and the README use.
#[test]
fn real_cosign_signs_and_verifies_without_a_log() {
    let Some(real) = std::env::var_os("ZONE_TEST_COSIGN") else {
        eprintln!("NOTICE: real cosign test skipped: set ZONE_TEST_COSIGN to a cosign binary");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let made = Command::new(&real)
        .args(["generate-key-pair", "--output-key-prefix", "release"])
        .current_dir(tmp.path())
        .env("COSIGN_PASSWORD", PASSWORD)
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", stderr(&made));
    let password_file = tmp.path().join("release.password");
    fs::write(&password_file, PASSWORD).unwrap();
    let k = Key {
        private: tmp.path().join("release.key"),
        public: tmp.path().join("release.pub"),
        password_file,
    };
    let dist = dist(tmp.path());
    let real = PathBuf::from(real);
    let run = |name: &str, args: &[&str], env: &[(&str, &Path)]| {
        let mut cmd = Command::new(repo_root().join("scripts").join(name));
        cmd.args(args)
            .arg(&dist)
            .env("ZONE_COSIGN", &real)
            .env("ZONE_COSIGN_PUB", &k.public)
            .env_remove("COSIGN_PASSWORD")
            .env_remove("ZONE_UNSIGNED");
        for (key, v) in env {
            cmd.env(key, v);
        }
        cmd.output().unwrap()
    };
    let out = run(
        "sign.sh",
        &[],
        &[
            ("ZONE_COSIGN_KEY", &k.private),
            ("ZONE_COSIGN_PASSWORD_FILE", &k.password_file),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let out = run("verify.sh", &[], &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    let bundle = fs::read_to_string(dist.join("SHA256SUMS.cosign.bundle")).unwrap();
    assert!(!bundle.contains("tlogEntries\":[{"), "{bundle}");
    let script = format!(
        "tr -d '\\r\\n ' <SHA256SUMS.cosign.bundle | \
         sed -n 's/.*\"signature\":\"\\([A-Za-z0-9+/=]*\\)\".*/\\1/p' | openssl base64 -d -A >sig && \
         openssl dgst -sha256 -verify '{}' -signature sig SHA256SUMS",
        k.public.display()
    );
    let out = Command::new("sh")
        .args(["-c", &script])
        .current_dir(&dist)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
}
