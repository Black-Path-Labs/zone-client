//! `zone members` and `zone grants`: who is in the joined zone and on what terms agents get in,
//! as JSON on stdout. Both read the roster this device last synced, after one sync when `--sync`
//! asks for it or the last sync is older than [`STALE_AFTER_MS`]; the SDK verifies every owner
//! signature before anything is shown. `syncedAtMs` says how old the roster is.
//!
//! A seat that only reads (no `zone run`) used to show the roster from its last write, so a
//! member that joined early kept listing the members of that moment (4 where the owner had 13).

use std::path::Path;
use std::sync::Arc;

use bp_zone_core::ZoneDevice;
use serde_json::{json, Value};

use crate::device;
use crate::record::{now_ms, SyncRecord};
use crate::transport::Http;

/// A roster last synced longer ago than this is synced again before it is shown.
pub const STALE_AFTER_MS: u64 = 60_000;

/// Whether a read must sync first: asked to, never synced, or last synced over a minute ago.
fn needs_sync(asked: bool, record: &SyncRecord, now_ms: u64) -> bool {
    asked
        || record
            .last_sync_ms
            .is_none_or(|t| now_ms.saturating_sub(t) > STALE_AFTER_MS)
}

/// The joined device and when its roster was last synced. Synced once first when `sync` asks
/// for it (a failure is an error) or the roster is stale (a failure is a warning and the local
/// roster is shown).
fn open(state: &Path, sync: bool, cmd: &str) -> Result<(Arc<ZoneDevice>, Option<u64>), String> {
    let mut record = SyncRecord::load(state);
    let fetch = needs_sync(sync, &record, now_ms());
    let device = if fetch {
        device::open_existing(state, Arc::new(Http::default()))?
    } else {
        device::open_existing(state, Arc::new(device::Offline))?
    }
    .filter(|d| d.status().zone_id.is_some())
    .ok_or("no zone joined: run `zone join` first")?;
    if fetch {
        let synced = device.sync(0);
        record.synced(synced.as_ref().map(|_| ()));
        record.save(state)?;
        if let Err(e) = synced {
            if sync {
                return Err(format!("sync failed ({}): {e}", crate::join::kind(&e)));
            }
            eprintln!(
                "zone {cmd}: sync failed ({}); showing the roster last synced",
                crate::join::kind(&e)
            );
        }
    }
    Ok((device, record.last_sync_ms))
}

/// `zone members`: each member with its kind, devices and the capabilities it offers.
pub fn members(state: &Path, sync: bool) -> Result<Value, String> {
    let (device, synced_at) = open(state, sync, "members")?;
    let caps = device.capabilities();
    let members: Vec<Value> = device
        .members()
        .into_iter()
        .map(|m| {
            let offered: Vec<Value> = caps
                .iter()
                .filter(|c| c.publisher_id == m.member_id)
                .map(|c| {
                    json!({
                        "capId": c.cap_id,
                        "kind": c.kind,
                        "label": c.label,
                        "machine": c.machine_name,
                        "online": c.online,
                    })
                })
                .collect();
            json!({
                "memberId": m.member_id,
                "name": m.name,
                "kind": m.kind,
                "admin": m.is_admin,
                "me": m.is_me,
                "devices": m.devices,
                "capabilities": offered,
            })
        })
        .collect();
    Ok(json!({
        "zoneId": device.status().zone_id,
        "admitted": device.status().admitted,
        "syncedAtMs": synced_at,
        "members": members,
    }))
}

/// `zone grants`: each admission grant's places, window and terms, and the per-agent caps.
pub fn grants(state: &Path, sync: bool) -> Result<Value, String> {
    let (device, synced_at) = open(state, sync, "grants")?;
    let grants: Vec<Value> = device
        .admission_grants()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|g| {
            json!({
                "grantId": g.grant_id,
                "admitter": g.delegate_member_id,
                "kinds": g.kinds,
                "scope": g.scope,
                "places": g.max_members,
                "used": g.admitted,
                "left": g.places_left,
                "issuedAtMs": g.issued_at_ms,
                "expiresAtMs": g.not_after_ms,
                "revoked": g.revoked,
                "inForce": g.in_force,
            })
        })
        .collect();
    let caps = device.agent_caps().map_err(|e| e.to_string())?.map(|c| {
        json!({
            "aors": c.aors,
            "rooms": c.rooms,
            "lines": c.lines,
            "admissionsPerDay": c.admissions_per_day,
        })
    });
    Ok(json!({
        "zoneId": device.status().zone_id,
        "syncedAtMs": synced_at,
        "grants": grants,
        "agentCaps": caps,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_syncs_when_asked_never_synced_or_over_a_minute_old() {
        let now = 1_000_000;
        let at = |t: Option<u64>| SyncRecord {
            last_sync_ms: t,
            ..SyncRecord::default()
        };
        assert!(needs_sync(false, &at(None), now));
        assert!(needs_sync(false, &at(Some(now - STALE_AFTER_MS - 1)), now));
        assert!(!needs_sync(false, &at(Some(now - STALE_AFTER_MS)), now));
        assert!(!needs_sync(false, &at(Some(now)), now));
        assert!(needs_sync(true, &at(Some(now)), now));
    }
}
