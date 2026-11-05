//! `zone rename`, `zone leave` and `zone receipts read`: what a member does about its own place
//! in the joined zone.
//!
//! `zone rename NAME` asks to be rostered under NAME. Roster names are the owner's to write (the
//! admitter wrote this one at join), so a member's request is a small sealed note the owner's
//! device acts on at its next sync; the owner's own rename applies at once. Author names on
//! notes follow the roster, so old notes show the new name too once it lands.
//!
//! `zone leave` tells the owner, who removes the member, and deletes the zone from this device.
//! The member keeps its receipts: before anything is deleted the SDK's receipt archive (the
//! zone's action receipts, the file and note receipts members signed for what this device sent,
//! and the ones this device signed, with the devices that signed them) is written to
//! `<state>/receipts-<zone>.json`, which `zone receipts verify --file` checks offline.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::device;
use crate::items::{joined, sdk_error, write_atomic};

/// Where the archive of the zone `zone_id` is kept in `state`.
pub fn archive_path(state: &Path, zone_id: &str) -> PathBuf {
    state.join(format!("receipts-{zone_id}.json"))
}

/// A zone id that is safe in a file name: the SDK's `wks_` plus letters and digits.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// `zone rename`: ask to be rostered under `name`.
pub fn rename(state: &Path, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return Err("rename: a name is 1 to 64 bytes of text".into());
    }
    let device = joined(state, "rename")?;
    device
        .rename_me(name.to_owned())
        .map_err(|e| sdk_error("rename", &e))?;
    let now = device
        .members()
        .into_iter()
        .find(|m| m.is_me)
        .map(|m| m.name);
    let applied = now.as_deref() == Some(name);
    println!(
        "{}",
        json!({
            "requested": name,
            // True for the zone's owner; otherwise the owner's device applies it when it syncs.
            "applied": applied,
        })
    );
    if !applied {
        eprintln!(
            "zone rename: asked the zone's owner; `zone members --sync` shows the new name once \
             an owner device has synced"
        );
    }
    Ok(())
}

/// `zone leave`: keep the receipts, tell the owner, delete the zone from this device.
pub fn leave(state: &Path) -> Result<(), String> {
    let device = joined(state, "leave")?;
    let status = device.status();
    let zone_id = status
        .zone_id
        .ok_or("no zone joined: run `zone join` first")?;
    if status.is_admin {
        return Err("leave: the zone's owner cannot leave its own zone".into());
    }
    if !safe_id(&zone_id) {
        return Err("leave: the zone id is not a plain id; nothing was changed".into());
    }
    // Before anything is deleted. A device that was never admitted has no roster to check
    // receipts against, so has none to keep.
    let kept = match device.receipt_archive() {
        Ok(doc) => {
            let path = archive_path(state, &zone_id);
            write_atomic(&path, doc.as_bytes(), true).map_err(|e| {
                format!("leave: the receipts could not be kept, nothing was left: {e}")
            })?;
            Some(path)
        }
        Err(bp_zone_core::ZoneError::NotAdmitted) => None,
        Err(e) => return Err(sdk_error("leave", &e)),
    };
    device
        .leave_zone(zone_id.clone())
        .map_err(|e| sdk_error("leave", &e))?;
    println!(
        "{}",
        json!({
            "left": zone_id,
            "receipts": kept.as_ref().map(|p| p.display().to_string()),
        })
    );
    Ok(())
}

/// `zone receipts read on|off`, or with neither show it. A device setting, off until turned on.
pub fn read_receipts(state: &Path, on: Option<bool>) -> Result<(), String> {
    let device = device::open_existing(state, Arc::new(device::Offline))?
        .ok_or("no zone joined: run `zone join` first")?;
    if let Some(on) = on {
        device
            .set_read_receipts(on)
            .map_err(|e| sdk_error("receipts", &e))?;
    }
    println!("{}", json!({"readReceipts": device.read_receipts()}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_archive_sits_in_the_state_dir_under_the_zone_id() {
        let p = archive_path(Path::new("/s"), "wks_01ABC");
        assert_eq!(p, Path::new("/s/receipts-wks_01ABC.json"));
    }

    #[test]
    fn only_a_plain_zone_id_names_a_file() {
        assert!(safe_id("wks_01K0000000000000000000000A"));
        for bad in ["", "../x", "a/b", "a b", "a\u{0}b", &"x".repeat(65)] {
            assert!(!safe_id(bad), "{bad:?}");
        }
    }
}
