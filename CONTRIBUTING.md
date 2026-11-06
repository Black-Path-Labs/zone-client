# Contributing

## How changes land

No CI runs pull requests here. This repository has no GitHub Actions, and `scripts/check.sh`
fails if `.github/workflows` appears. A maintainer runs the checks on their own machine before
merging, so a pull request is reviewed and checked by hand.

Before you open a pull request, run the same gate:

```sh
scripts/check.sh
```

It runs `cargo fmt --check`, `cargo clippy --all-targets --locked -D warnings` and
`cargo test --locked`, all with the committed `Cargo.lock`.

## What belongs here

This repository holds only the client. Every Zone and Core operation goes through the
[zone-sdk](https://github.com/Black-Path-Labs/zone-sdk) crates (`bp-workspace-core`,
`bp-zone-core`, `bp-zone-rtc`), pinned by git revision in `Cargo.toml`. Do not add protocol code:
routes, HTTP methods, token schemes, JWS, HPKE or signature handling belong in zone-sdk.
`tests/repo_hygiene.rs` fails on them. A change the client needs from the SDK goes to zone-sdk
first. zone-sdk is developed in its own repository, so send the change there as a pull request.
<!-- R-081 (M15), adopted on recommendation 2026-11-06, pending Matt's ratification: zone-sdk is
developed in place, not generated from Core. -->

Documented subcommands and flags are tested: `tests/docs.rs` fails if `README.md`,
`SECURITY.md` or this file names a `zone` subcommand or flag that does not exist.

## Releases

Maintainers cut releases with `scripts/release.sh` on their own machine (see the README's Cut a
release). Contributors do not tag or publish. A release is a draft until a maintainer publishes
it with a recorded approval.

## Licence

Contributions are licensed as the repository is: MIT.
