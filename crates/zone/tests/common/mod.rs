//! Stand-ins for cosign and a Sigstore identity token, shared by the signing tests. The real
//! cosign needs Fulcio and Rekor; these check the scripts' wiring: which identity they pin, which
//! file they sign and that a refused identity never reaches cosign.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The release identity the scripts pin.
pub const IDENTITY: &str = "admin@blackpathlabs.com";

pub fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// base64url without padding, as a JWT carries its parts.
pub fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| (n << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// A token file whose payload carries these claims. Its signature part is a stand-in: Fulcio
/// checks it, the scripts only read the claims.
pub fn token(dir: &Path, email: &str, verified: bool, connector: &str) -> PathBuf {
    let claims = format!(
        r#"{{"iss":"https://oauth2.sigstore.dev/auth","email":"{email}","email_verified":{verified},"federated_claims":{{"connector_id":"{connector}"}}}}"#
    );
    let path = dir.join(format!("{email}.{connector}.token"));
    fs::write(
        &path,
        format!(
            "{}.{}.c2ln",
            b64url(br#"{"alg":"RS256"}"#),
            b64url(claims.as_bytes())
        ),
    )
    .unwrap();
    path
}

/// A stand-in `cosign` in `dir`. `sign-blob` writes a bundle naming the token's email and the
/// blob's SHA-256; `verify-blob` passes only when the bundle names the pinned identity, the
/// issuer is Google's and the blob is the one signed. Every call goes to `dir/cosign.log`.
pub fn cosign(dir: &Path) -> PathBuf {
    let path = dir.join("cosign");
    executable(
        &path,
        &format!(
            r#"#!/bin/sh
echo "cosign $*" >>'{log}'
cmd="$1"
shift
bundle= token= ident= iss= blob=
while [ $# -gt 0 ]; do
  case "$1" in
    --bundle) bundle="$2"; shift ;;
    --identity-token) token="$2"; shift ;;
    --certificate-identity) ident="$2"; shift ;;
    --certificate-oidc-issuer) iss="$2"; shift ;;
    --yes) ;;
    *) blob="$1" ;;
  esac
  shift
done
sum="$(sha256sum <"$blob" | cut -d' ' -f1)"
case "$cmd" in
  sign-blob)
    p="$(cut -d. -f2 "$token" | tr '_-' '/+')"
    while [ $((${{#p}} % 4)) -ne 0 ]; do p="$p="; done
    email="$(printf '%s' "$p" | openssl base64 -d -A | sed -n 's/.*"email":"\([^"]*\)".*/\1/p')"
    echo "$email $sum" >"$bundle"
    ;;
  verify-blob)
    [ "$iss" = https://accounts.google.com ] && [ "$(cat "$bundle")" = "$ident $sum" ]
    ;;
  *) exit 2 ;;
esac
"#,
            log = dir.join("cosign.log").display()
        ),
    );
    path
}

/// What the stand-in `cosign sign-blob` writes for `blob` signed as `email`.
pub fn bundle(email: &str, blob: &Path) -> String {
    let out = Command::new("sha256sum").arg(blob).output().unwrap();
    let sum = String::from_utf8(out.stdout).unwrap();
    format!("{email} {}\n", &sum[..64])
}

pub fn cosign_log(dir: &Path) -> String {
    fs::read_to_string(dir.join("cosign.log")).unwrap_or_default()
}
