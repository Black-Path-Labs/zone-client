//! `zone receipts verify --file` against the zone-sdk `zone-receipt.json` contract fixture
//! (a copy of `docs/CONTRACTS/fixtures/zone/zone-receipt.json` at the pinned zone-sdk rev): the
//! signed chain verifies, as receipts and as receipt notes, and the dropped, repeated, reordered
//! and forged cases exit 5 naming the actor and seq.

use std::path::Path;
use std::process::{Command, Output};

use bp_workspace_core::receipt::receipt_note;
use serde_json::{json, Value};

const EXIT_UNVERIFIED: i32 = 5;

fn fixture() -> Value {
    let v: Value = serde_json::from_str(include_str!("fixtures/zone-receipt.json")).unwrap();
    v["cases"][0].clone()
}

/// A flattened fixture receipt in compact form.
fn compact(r: &Value) -> String {
    ["protected", "payload", "signature"]
        .map(|k| r[k].as_str().unwrap())
        .join(".")
}

fn signed() -> Vec<String> {
    fixture()["signed"]["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .map(compact)
        .collect()
}

fn actor() -> String {
    fixture()["inputs"]["device"]["fingerprint"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn verify(dir: &Path, receipts: Vec<Value>) -> Output {
    let c = fixture();
    let file = dir.join("receipts.json");
    std::fs::write(
        &file,
        json!({
            "workspace_id": c["inputs"]["workspace_id"],
            "devices": [c["inputs"]["device"]["bundle"]],
            "receipts": receipts,
        })
        .to_string(),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_zone"))
        .arg("--state")
        .arg(dir.join("zone"))
        .args(["receipts", "verify", "--file"])
        .arg(&file)
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run zone")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn the_signed_chain_verifies_as_receipts_and_as_notes() {
    let tmp = tempfile::tempdir().unwrap();
    let receipts = signed();
    let notes: Vec<Value> = receipts
        .iter()
        .map(|r| serde_json::from_str(&receipt_note(r).unwrap()).unwrap())
        .collect();
    for input in [receipts.iter().map(|r| json!(r)).collect(), notes] {
        let out = verify(tmp.path(), input);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let stdout = text(&out.stdout);
        assert!(
            stdout.contains(&format!("ok {} seq 0..=3 (4 receipts)", actor())),
            "{stdout}"
        );
        assert!(
            stdout.contains("verified 4 receipts in 1 chains"),
            "{stdout}"
        );
    }
    assert!(!tmp.path().join("zone").exists(), "--file makes no state");
}

#[test]
fn dropped_repeated_and_reordered_receipts_exit_5_naming_actor_and_seq() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fixture();
    let receipts = signed();
    // The fixture lists each case by index; the seq named is the first receipt out of place.
    for (case, seq, words) in [
        ("dropped", 2, "seq 1 is missing"),
        ("repeated", 1, "repeated or out of order: expected seq 2"),
        ("reordered", 2, "seq 1 is missing"),
    ] {
        let picked: Vec<Value> = c["inputs"][case]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| json!(receipts[n.as_u64().unwrap() as usize]))
            .collect();
        let out = verify(tmp.path(), picked);
        assert_eq!(out.status.code(), Some(EXIT_UNVERIFIED), "{case}: {out:?}");
        let err = text(&out.stderr);
        assert!(
            err.contains(&format!("actor {} seq {seq}: {words}", actor())),
            "{case}: {err}"
        );
        assert!(!text(&out.stdout).contains("verified"), "{case}");
    }
}

#[test]
fn a_forged_receipt_exits_5_naming_the_actor_it_claims() {
    let tmp = tempfile::tempdir().unwrap();
    let mut receipts: Vec<Value> = signed().into_iter().map(|r| json!(r)).collect();
    receipts[0] = json!(compact(&fixture()["signed"]["forged_jws"]));
    let out = verify(tmp.path(), receipts);
    assert_eq!(out.status.code(), Some(EXIT_UNVERIFIED), "{out:?}");
    let err = text(&out.stderr);
    assert!(
        err.contains(&format!("actor {} seq 0: bad receipt", actor())),
        "{err}"
    );
}

#[test]
fn a_receipt_from_a_device_not_in_the_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fixture();
    let file = tmp.path().join("receipts.json");
    std::fs::write(
        &file,
        json!({
            "workspace_id": c["inputs"]["workspace_id"],
            "devices": [],
            "receipts": signed(),
        })
        .to_string(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_zone"))
        .arg("--state")
        .arg(tmp.path().join("zone"))
        .args(["receipts", "verify", "--file"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_UNVERIFIED));
    assert!(text(&out.stderr).contains("signed by no device of this zone's roster"));
}

#[test]
fn a_receipt_for_another_zone_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let c = fixture();
    let file = tmp.path().join("receipts.json");
    std::fs::write(
        &file,
        json!({
            "workspace_id": format!("wks_{}", "9".repeat(26)),
            "devices": [c["inputs"]["device"]["bundle"]],
            "receipts": signed(),
        })
        .to_string(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_zone"))
        .arg("--state")
        .arg(tmp.path().join("zone"))
        .args(["receipts", "verify", "--file"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_UNVERIFIED));
    assert!(text(&out.stderr).contains("seq 0: bad receipt"), "{out:?}");
}
