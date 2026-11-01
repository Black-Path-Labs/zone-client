//! Reads the zone-sdk revision the workspace's Cargo.lock pins and exposes it as `ZONE_SDK_REV`
//! for `zone --version`. The build fails if the lock does not pin bp-zone-core to a zone-sdk git
//! revision, so a version line never names a revision the binary was not built from.

use std::path::Path;

const SDK_SOURCE: &str = "git+https://github.com/Black-Path-Labs/zone-sdk?rev=";

fn main() {
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let text =
        std::fs::read_to_string(&lock).unwrap_or_else(|e| panic!("read {}: {e}", lock.display()));
    let rev = sdk_rev(&text).expect("Cargo.lock does not pin bp-zone-core to a zone-sdk git rev");
    println!("cargo:rustc-env=ZONE_SDK_REV={rev}");
}

/// The resolved commit (after `#`) of the `bp-zone-core` package's zone-sdk source.
fn sdk_rev(lock: &str) -> Option<String> {
    let mut in_core = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_core = false;
        } else if line == "name = \"bp-zone-core\"" {
            in_core = true;
        } else if in_core {
            if let Some(source) = line.strip_prefix("source = \"") {
                let source = source.strip_suffix('"')?;
                let (_, commit) = source.strip_prefix(SDK_SOURCE)?.split_once('#')?;
                return Some(commit.to_string());
            }
        }
    }
    None
}
