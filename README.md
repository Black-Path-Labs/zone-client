# zone-client

The `zone` command-line client for Black Path Zones, written in Rust on top of the
[zone-sdk](https://github.com/Black-Path-Labs/zone-sdk) crates.

This repository is private until release.

## Verify a download

Each release carries `SHA256SUMS` over every binary and `SHA256SUMS.minisig`, a
[minisign](https://jedisct1.github.io/minisign/) signature of it. Check the signature, then the
binary you downloaded:

```sh
minisign -Vm SHA256SUMS -P <release public key>
sha256sum -c --ignore-missing SHA256SUMS
```

The release public key is not published yet. Until it is, releases ship `UNSIGNED` in place of
`SHA256SUMS.minisig`, and only the sums can be checked.

To check a whole `dist` directory, `scripts/verify.sh -P <public key> dist` (or
`--allow-unsigned` for an unsigned one). `scripts/sign.sh dist` writes the sums, and signs them
when `ZONE_SIGN_KEY` names a minisign secret key. `scripts/release-tools.sh <dir>` fetches the
pinned minisign 0.12 and prints the `ZONE_MINISIGN` line both scripts read.

## Cut a release

Releases are cut on a maintainer's machine; no CI runs here. From a clean checkout whose HEAD is
pushed:

```sh
scripts/release.sh v0.1.0-rc.1
```

runs `scripts/check.sh`, builds the Linux x86_64 and arm64 binaries, writes `SHA256SUMS` (signed
when `ZONE_SIGN_KEY` is set) and uploads them to a draft GitHub release. It refuses a dirty tree
or a tag or release that exists. `ZONE_BUILD_LOCK=<file>` holds `flock` on that file through the
gate and builds. A draft is published only with a recorded approval:
`ZONE_RELEASE_APPROVED=<approval id> scripts/release.sh --publish v0.1.0-rc.1`.

`scripts/repro.sh x86_64-unknown-linux-musl` builds a target twice from nothing and fails unless
both binaries have the same SHA-256. `scripts/build.sh` maps build-machine paths away, so the
binary names no home directory.
