//! `zone receipts verify`: check the zone's signed action receipts (`bp-zone-receipt-v1`) offline,
//! each one and each actor's chain, through the SDK (`receipt::verify_receipt`,
//! `receipt::verify_chain`). The client only groups receipts by actor and words the result.
//!
//! The receipts and the actor devices come from local state (the receipts this device opened and
//! the zone's verified roster) or from a file. A chain is checked from the first receipt held for
//! its actor: receipts sealed before this device joined never reached it.

use std::path::Path;
use std::sync::Arc;

use bp_workspace_core::bundle::{DeviceBundle, TrustedDevice};
use bp_workspace_core::receipt::{claimed, verify_chain, verify_receipt, ChainHead, Receipt};
use bp_workspace_core::space::{parse_note, Note};
use bp_zone_core::ZoneError;
use serde_json::Value;

use crate::device;

/// What to check: one zone's receipts, in the order they were sealed, and the devices whose
/// receipts count.
pub struct Input {
    pub workspace_id: String,
    /// (fingerprint, bundle) of each device trusted to sign receipts.
    pub devices: Vec<(String, DeviceBundle)>,
    pub receipts: Vec<String>,
}

/// One actor's chain, verified.
#[derive(Debug, PartialEq, Eq)]
pub struct Chain {
    pub actor: String,
    pub first_seq: u64,
    pub last_seq: u64,
    pub count: usize,
}

/// A receipt that failed, named by its actor and seq (as the receipt claims them when it does
/// not verify).
#[derive(Debug, PartialEq, Eq)]
pub struct Failure {
    pub actor: Option<String>,
    pub seq: Option<u64>,
    /// Its place in the input, from 0.
    pub index: usize,
    pub why: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.actor, self.seq) {
            (Some(a), Some(s)) => write!(f, "actor {a} seq {s}: {}", self.why),
            _ => write!(f, "receipt #{}: {}", self.index, self.why),
        }
    }
}

/// Every chain that verifies, and every receipt that does not. A chain stops at its first
/// failure: the receipts after it have no verified place.
pub fn verify(input: &Input) -> (Vec<Chain>, Vec<Failure>) {
    let ws = input.workspace_id.as_str();
    let mut failures = Vec::new();
    // Per actor, in order of first appearance: the receipts that verified on their own.
    let mut by_actor: Vec<(String, Vec<(usize, &String, Receipt)>)> = Vec::new();
    let mut broken: Vec<String> = Vec::new();
    for (index, r) in input.receipts.iter().enumerate() {
        let Some((actor, seq)) = claimed(r) else {
            failures.push(Failure {
                actor: None,
                seq: None,
                index,
                why: "not a zone receipt".into(),
            });
            continue;
        };
        let fail = |why: String| Failure {
            actor: Some(actor.clone()),
            seq: Some(seq),
            index,
            why,
        };
        let Some((fp, bundle)) = input.devices.iter().find(|(fp, _)| *fp == actor) else {
            failures.push(fail("signed by no device of this zone's roster".into()));
            broken.push(actor.clone());
            continue;
        };
        let trusted = TrustedDevice {
            bundle,
            fingerprint: fp,
        };
        match verify_receipt(r, &trusted, ws) {
            Ok(got) => match by_actor.iter_mut().find(|(a, _)| *a == actor) {
                Some((_, list)) => list.push((index, r, got)),
                None => by_actor.push((actor.clone(), vec![(index, r, got)])),
            },
            Err(e) => {
                failures.push(fail(format!("bad receipt ({e})")));
                broken.push(actor.clone());
            }
        }
    }

    let mut chains = Vec::new();
    for (actor, list) in by_actor {
        if broken.contains(&actor) {
            continue;
        }
        let (fp, bundle) = input
            .devices
            .iter()
            .find(|(fp, _)| *fp == actor)
            .expect("verified against a listed device");
        let trusted = TrustedDevice {
            bundle,
            fingerprint: fp,
        };
        let first = &list[0].2;
        let mut head = ChainHead {
            seq: first.seq,
            prev: first.prev.clone(),
        };
        let mut ok = true;
        for (index, r, got) in &list {
            match verify_chain(std::slice::from_ref(*r), &trusted, ws, &head) {
                Ok((_, next)) => head = next,
                Err(e) => {
                    let why = if got.seq < head.seq {
                        format!("repeated or out of order: expected seq {} ({e})", head.seq)
                    } else if got.seq > head.seq {
                        format!("seq {} is missing: dropped or reordered ({e})", head.seq)
                    } else {
                        format!("does not follow the receipt before it ({e})")
                    };
                    failures.push(Failure {
                        actor: Some(actor.clone()),
                        seq: Some(got.seq),
                        index: *index,
                        why,
                    });
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            chains.push(Chain {
                actor,
                first_seq: first.seq,
                last_seq: head.seq - 1,
                count: list.len(),
            });
        }
    }
    failures.sort_by_key(|f| f.index);
    (chains, failures)
}

/// The current zone's receipts and verified roster from local state.
pub fn from_state(state: &Path) -> Result<Input, String> {
    let no_zone = "no zone joined; `zone join` first, or check a file with --file";
    let d = device::open_existing(state, Arc::new(device::Offline))?.ok_or(no_zone)?;
    let workspace_id = d.status().zone_id.ok_or(no_zone)?;
    let devices = d.zone_device_bundles().map_err(|e| match e {
        ZoneError::NotAdmitted => {
            "not admitted yet: there is no verified roster to check receipts against".to_owned()
        }
        e => e.to_string(),
    })?;
    Ok(Input {
        workspace_id,
        devices,
        receipts: d.zone_receipts(),
    })
}

/// A receipts file: `{"workspace_id": "wks_…", "devices": [device bundle, …], "receipts": […]}`,
/// each receipt a compact receipt or its `bp-zone-receipt-note-v1` note. The devices are trusted
/// as given, so take them from a roster you verified.
pub fn from_file(path: &Path) -> Result<Input, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let v: Value =
        serde_json::from_str(&text).map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
    let workspace_id = v["workspace_id"]
        .as_str()
        .ok_or("receipts file: no workspace_id")?
        .to_owned();
    let devices = v["devices"]
        .as_array()
        .ok_or("receipts file: no devices")?
        .iter()
        .enumerate()
        .map(|(n, b)| {
            let bundle = DeviceBundle::from_value(b)
                .map_err(|e| format!("receipts file: device #{n}: {e}"))?;
            Ok((bundle.fingerprint(), bundle))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let receipts = v["receipts"]
        .as_array()
        .ok_or("receipts file: no receipts")?
        .iter()
        .enumerate()
        .map(|(n, r)| match r {
            Value::String(s) => Ok(s.clone()),
            note @ Value::Object(_) => match parse_note(note.to_string().as_bytes()) {
                Ok(Note::Receipt { receipt }) => Ok(receipt),
                _ => Err(format!("receipts file: receipt #{n} is not a receipt note")),
            },
            _ => Err(format!("receipts file: receipt #{n} is not a receipt")),
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Input {
        workspace_id,
        devices,
        receipts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_check_is_no_chain_and_no_failure() {
        let input = Input {
            workspace_id: format!("wks_{}", "0".repeat(26)),
            devices: vec![],
            receipts: vec![],
        };
        assert_eq!(verify(&input), (vec![], vec![]));
    }

    #[test]
    fn a_string_that_is_no_receipt_is_named_by_its_place() {
        let input = Input {
            workspace_id: format!("wks_{}", "0".repeat(26)),
            devices: vec![],
            receipts: vec!["a.b.c".into()],
        };
        let (chains, failures) = verify(&input);
        assert!(chains.is_empty());
        assert_eq!(failures[0].to_string(), "receipt #0: not a zone receipt");
    }
}
