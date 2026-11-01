//! Scripts/sign.sh and scripts/verify.sh with an ephemeral minisign key made here. A
//! changed artifact, sum or signature, a wrong key, an unlisted file and an unsigned release
//! all fail. minisign is ZONE_MINISIGN or minisign on PATH (scripts/release-tools.sh).

use std::fs;
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

struct Key {
    secret: PathBuf,
    public: String,
}

fn keygen(dir: &Path) -> Key {
    let secret = dir.join("zone.key");
    let public = dir.join("zone.pub");
    let out = Command::new(minisign())
        .args(["-G", "-W", "-p"])
        .arg(&public)
        .arg("-s")
        .arg(&secret)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let public = fs::read_to_string(&public).unwrap();
    let public = public.lines().nth(1).unwrap().to_string();
    Key { secret, public }
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

fn script(name: &str, args: &[&str], dist: &Path, key: Option<&Path>) -> Output {
    let mut cmd = Command::new(repo_root().join("scripts").join(name));
    cmd.args(args)
        .arg(dist)
        .env("ZONE_MINISIGN", minisign())
        .env_remove("ZONE_SIGN_KEY")
        .env_remove("ZONE_SIGN_PUBKEY");
    if let Some(key) = key {
        cmd.env("ZONE_SIGN_KEY", key);
    }
    cmd.output().unwrap()
}

fn sign(dist: &Path, key: &Key) {
    let out = script("sign.sh", &[], dist, Some(&key.secret));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn verify(dist: &Path, args: &[&str]) -> Output {
    script("verify.sh", args, dist, None)
}

#[test]
fn signed_release_verifies_and_lists_every_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let key = keygen(tmp.path());
    let dist = dist(tmp.path());
    sign(&dist, &key);

    let sums = fs::read_to_string(dist.join("SHA256SUMS")).unwrap();
    let names: Vec<&str> = sums.lines().map(|l| &l[66..]).collect();
    assert_eq!(
        names,
        [
            "zone-0.1.0-aarch64-unknown-linux-musl",
            "zone-0.1.0-x86_64-unknown-linux-musl"
        ]
    );
    assert!(dist.join("SHA256SUMS.minisig").is_file());
    assert!(!dist.join("UNSIGNED").exists());
    let sig = fs::read_to_string(dist.join("SHA256SUMS.minisig")).unwrap();
    assert!(
        sig.contains("trusted comment: zone-client 0.1.0 SHA256SUMS"),
        "{sig}"
    );

    let out = verify(&dist, &["-P", &key.public]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Signing again replaces the sums and signature rather than listing them.
    sign(&dist, &key);
    assert_eq!(fs::read_to_string(dist.join("SHA256SUMS")).unwrap(), sums);
    assert!(verify(&dist, &["-P", &key.public]).status.success());
}

#[test]
fn modified_artifact_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let key = keygen(tmp.path());
    let dist = dist(tmp.path());
    sign(&dist, &key);
    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"x86_64 builT",
    )
    .unwrap();
    let out = verify(&dist, &["-P", &key.public]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not match SHA256SUMS"));
}

#[test]
fn modified_sums_or_signature_fail() {
    let tmp = tempfile::tempdir().unwrap();
    let key = keygen(tmp.path());
    let dist = dist(tmp.path());
    sign(&dist, &key);

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
    let out = verify(&dist, &["-P", &key.public]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a good signature"));

    // Sums restored, signature line changed.
    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"x86_64 build",
    )
    .unwrap();
    fs::write(dist.join("SHA256SUMS"), &good_sums).unwrap();
    assert!(verify(&dist, &["-P", &key.public]).status.success());
    let sig = fs::read_to_string(dist.join("SHA256SUMS.minisig")).unwrap();
    let mut lines: Vec<String> = sig.lines().map(String::from).collect();
    // One base64 character inside the Ed25519 signature bytes.
    let mut chars: Vec<char> = lines[1].chars().collect();
    chars[50] = if chars[50] == 'A' { 'B' } else { 'A' };
    lines[1] = chars.into_iter().collect();
    fs::write(dist.join("SHA256SUMS.minisig"), lines.join("\n") + "\n").unwrap();
    assert!(!verify(&dist, &["-P", &key.public]).status.success());
}

#[test]
fn another_key_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let key = keygen(tmp.path());
    let other_dir = tmp.path().join("other");
    fs::create_dir(&other_dir).unwrap();
    let other = keygen(&other_dir);
    let dist = dist(tmp.path());
    sign(&dist, &key);
    assert!(!verify(&dist, &["-P", &other.public]).status.success());
    let out = verify(&dist, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no public key"));
}

#[test]
fn unlisted_file_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let key = keygen(tmp.path());
    let dist = dist(tmp.path());
    sign(&dist, &key);
    fs::write(dist.join("zone-0.1.0-extra"), b"not signed").unwrap();
    let out = verify(&dist, &["-P", &key.public]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("zone-0.1.0-extra is not in SHA256SUMS"));
}

#[test]
fn without_a_key_the_release_is_unsigned_and_fails_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = dist(tmp.path());
    let out = script("sign.sh", &[], &dist, None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("UNSIGNED"));
    assert!(dist.join("SHA256SUMS").is_file());
    assert!(!dist.join("SHA256SUMS.minisig").exists());
    assert!(fs::read_to_string(dist.join("UNSIGNED"))
        .unwrap()
        .contains("not signed"));

    let out = verify(&dist, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--allow-unsigned"));
    assert!(verify(&dist, &["--allow-unsigned"]).status.success());

    fs::write(
        dist.join("zone-0.1.0-x86_64-unknown-linux-musl"),
        b"changed",
    )
    .unwrap();
    assert!(!verify(&dist, &["--allow-unsigned"]).status.success());
}
