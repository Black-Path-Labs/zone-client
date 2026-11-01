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
