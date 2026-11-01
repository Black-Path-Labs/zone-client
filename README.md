# zone-client

The `zone` command-line client for Black Path Zones, written in Rust on top of the
[zone-sdk](https://github.com/Black-Path-Labs/zone-sdk) crates.

This repository is private until release.

## Install

On Linux (x86_64 or arm64):

```sh
curl -fsSL https://github.com/Black-Path-Labs/zone-client/releases/latest/download/install.sh | sh
zone join <invite>
```

`install.sh` downloads the binary for this machine and `SHA256SUMS`, checks the sum (and the
minisign signature when `minisign` is installed, otherwise it prints the command to check it) and
installs to `~/.local/bin/zone`. It never uses sudo. `ZONE_VERSION=v0.1.0` pins a release and
`ZONE_INSTALL_DIR` replaces `~/.local/bin`. While the repository is private, `ZONE_GH=1`
downloads with `gh release download` and gh's login.

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
