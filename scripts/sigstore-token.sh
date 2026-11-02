#!/usr/bin/env bash
# Gets a Sigstore OIDC identity token by the OAuth device flow, so signing works on a machine
# with no browser: it prints a URL and a code, someone opens the URL on any device and signs in,
# and the token is written to <token file> (mode 0600). It prints the token's email on stdout.
# It talks only to oauth2.sigstore.dev; nothing is signed or logged in Rekor here.
#
#   scripts/sigstore-token.sh <token file>
#
# ZONE_SIGSTORE_OIDC replaces the issuer URL (tests). Needs curl, jq and openssl.
set -euo pipefail

out="${1:-}"
if [ -z "${out}" ]; then
  echo "usage: scripts/sigstore-token.sh <token file>" >&2
  exit 2
fi
for tool in curl jq openssl; do
  command -v "${tool}" >/dev/null || {
    echo "sigstore-token: needs ${tool}" >&2
    exit 1
  }
done

oidc="${ZONE_SIGSTORE_OIDC:-https://oauth2.sigstore.dev/auth}"
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
verifier="$(openssl rand 48 | b64url)"
challenge="$(printf '%s' "${verifier}" | openssl dgst -sha256 -binary | b64url)"

code="$(curl -fsS -X POST "${oidc}/device/code" -d client_id=sigstore \
  -d 'scope=openid email federated:id' -d "code_challenge=${challenge}" \
  -d code_challenge_method=S256)"
device="$(jq -r .device_code <<<"${code}")"
interval="$(jq -r '.interval // 5' <<<"${code}")"
expires="$(jq -r '.expires_in // 300' <<<"${code}")"
{
  echo "sigstore-token: open $(jq -r '.verification_uri_complete // .verification_uri' <<<"${code}")"
  echo "sigstore-token: code $(jq -r .user_code <<<"${code}"); choose Google and sign in as the release identity."
  echo "sigstore-token: waiting up to ${expires}s"
} >&2

deadline=$((SECONDS + expires))
while [ "${SECONDS}" -lt "${deadline}" ]; do
  sleep "${interval}"
  reply="$(curl -sS -X POST "${oidc}/token" -d client_id=sigstore \
    -d grant_type=urn:ietf:params:oauth:grant-type:device_code \
    -d "device_code=${device}" -d "code_verifier=${verifier}")"
  case "$(jq -r '.error // empty' <<<"${reply}")" in
    "")
      (
        umask 077
        jq -r .id_token <<<"${reply}" >"${out}"
      )
      payload="$(cut -d. -f2 "${out}" | tr '_-' '/+')"
      while [ $((${#payload} % 4)) -ne 0 ]; do payload="${payload}="; done
      openssl base64 -d -A <<<"${payload}" | jq -r .email
      exit 0
      ;;
    authorization_pending) ;;
    slow_down) interval=$((interval + 5)) ;;
    *)
      echo "sigstore-token: $(jq -r .error <<<"${reply}")" >&2
      exit 1
      ;;
  esac
done
echo "sigstore-token: no sign-in within ${expires}s" >&2
exit 1
