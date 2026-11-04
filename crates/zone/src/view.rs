//! `zone members` and `zone grants`: who is in the joined zone and on what terms agents get in,
//! as JSON on stdout. Both read the roster this device last synced (`--sync` syncs once first);
//! the SDK verifies every owner signature before anything is shown.

use std::path::Path;
use std::sync::Arc;

use bp_zone_core::ZoneDevice;
use serde_json::{json, Value};

use crate::device;
use crate::record::SyncRecord;
use crate::transport::Http;

/// The joined device, synced once first when `sync`.
fn open(state: &Path, sync: bool) -> Result<Arc<ZoneDevice>, String> {
    let device = if sync {
        device::open_existing(state, Arc::new(Http::default()))?
    } else {
        device::open_existing(state, Arc::new(device::Offline))?
    }
    .filter(|d| d.status().zone_id.is_some())
    .ok_or("no zone joined: run `zone join` first")?;
    if sync {
        let synced = device.sync(0);
        let mut record = SyncRecord::load(state);
        record.synced(synced.as_ref().map(|_| ()));
        record.save(state)?;
        synced.map_err(|e| format!("sync failed ({}): {e}", crate::join::kind(&e)))?;
    }
    Ok(device)
}

/// `zone members`: each member with its kind, devices and the capabilities it offers.
pub fn members(state: &Path, sync: bool) -> Result<Value, String> {
    let device = open(state, sync)?;
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
        "members": members,
    }))
}

/// `zone grants`: each admission grant's places, window and terms, and the per-agent caps.
pub fn grants(state: &Path, sync: bool) -> Result<Value, String> {
    let device = open(state, sync)?;
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
        "grants": grants,
        "agentCaps": caps,
    }))
}
