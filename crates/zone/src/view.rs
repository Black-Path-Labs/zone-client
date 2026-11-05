//! `zone members` and `zone grants`: who is in the joined zone and on what terms agents get in,
//! as JSON on stdout. Both read the roster this device last synced, after one sync when `--sync`
//! asks for it or the last sync is older than [`STALE_AFTER_MS`]; the SDK verifies every owner
//! signature before anything is shown. `syncedAtMs` says how old the roster is.
//!
//! `zone caps --sync` adds two sources side by side: the skills members advertise (`source: peer`,
//! descriptions only) and what Core lets each member do now (`source: core`, read from Core on
//! every call). A peer cannot make an entry of the other kind. `zone rename` changes the
//! advertised name and syncs once.
//!
//! A seat that only reads (no `zone run`) used to show the roster from its last write, so a
//! member that joined early kept listing the members of that moment (4 where the owner had 13).

use std::path::Path;
use std::sync::Arc;

use bp_zone_core::ZoneDevice;
use serde_json::{json, Value};

use crate::device;
use crate::profile;
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

/// One capability a member advertises, as `zone members` and `zone caps --sync` print it.
fn peer_cap(c: &bp_zone_core::CapabilityInfo) -> Value {
    json!({
        "source": "peer",
        "capId": c.cap_id,
        "kind": c.kind,
        "label": c.label,
        "machine": c.machine_name,
        "online": c.online,
    })
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
                .map(peer_cap)
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

/// The schema of Core's access view; anything else is shown as unavailable.
const CORE_ACCESS: &str = "bp.core-capability-access.v1";

/// What `zone caps --sync` shows for Core's access view: the view as Core sent it, or why it is
/// unavailable (a reason kind, never Core's text). Never an empty list standing for "none".
fn core_access(read: Result<Value, bp_zone_core::ZoneError>) -> Value {
    match read {
        Ok(v) if v["schema"] == CORE_ACCESS => v,
        Ok(_) => json!({"source": "core", "available": false, "reason": "unknown-format"}),
        Err(e) => json!({"source": "core", "available": false, "reason": crate::join::kind(&e)}),
    }
}

/// `zone caps --sync`: sync, then every advertised skill with its member, and Core's access view.
pub fn caps(state: &Path) -> Result<Value, String> {
    let (device, synced_at) = open(state, true, "caps")?;
    let members = device.members();
    let peer: Vec<Value> = device
        .capabilities()
        .iter()
        .map(|c| {
            let mut v = peer_cap(c);
            v["publisherId"] = json!(c.publisher_id);
            v["publisher"] = json!(members
                .iter()
                .find(|m| m.member_id == c.publisher_id)
                .map(|m| m.name.clone()));
            v["mine"] = json!(c.mine);
            v
        })
        .collect();
    Ok(json!({
        "zoneId": device.status().zone_id,
        "syncedAtMs": synced_at,
        "peer": peer,
        "core": core_access(device.core_capabilities()),
    }))
}

/// `zone rename NAME`: the advertised name changes, the skills stay, and one sync publishes it.
/// A seat with no saved profile starts one with the CLI's own skill.
pub fn rename(state: &Path, name: &str) -> Result<Value, String> {
    let device = device::open_existing(state, Arc::new(Http::default()))?
        .filter(|d| d.status().zone_id.is_some())
        .ok_or("no zone joined: run `zone join` first")?;
    let current = match profile::load(state)? {
        Some(p) => Some(p),
        None => device.agent_profile().map_err(|e| e.to_string())?,
    };
    let renamed = match current {
        Some(p) => profile::with_name(p, name)?,
        None => profile::named(name)?,
    };
    device
        .set_agent_profile(renamed.clone())
        .map_err(|e| format!("the name was not taken ({})", crate::join::kind(&e)))?;
    profile::save(state, &renamed)?;
    let published = if device.status().admitted {
        let mut record = SyncRecord::load(state);
        let synced = device.sync(0);
        record.synced(synced.as_ref().map(|_| ()));
        record.save(state)?;
        match synced {
            Ok(_) => true,
            Err(e) => {
                eprintln!(
                    "zone rename: sync failed ({}); the next sync publishes the name",
                    crate::join::kind(&e)
                );
                false
            }
        }
    } else {
        false
    };
    Ok(json!({
        "name": renamed.name,
        "capabilities": renamed.capabilities,
        "published": published,
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

    #[test]
    fn core_access_is_core_s_view_or_says_why_not() {
        let view = json!({
            "schema": CORE_ACCESS,
            "workspaceId": "wks_x",
            "scope": "zone",
            "members": [{"pseudonym": "bp:self:a", "role": "member", "items": []}],
        });
        assert_eq!(core_access(Ok(view.clone())), view);
        let other = core_access(Ok(json!({"schema": "something-else", "members": []})));
        assert_eq!(other["available"], false);
        assert_eq!(other["reason"], "unknown-format");
        let down = core_access(Err(bp_zone_core::ZoneError::Core {
            status: 503,
            reason: "core text never shown".into(),
        }));
        assert_eq!(down["source"], "core");
        assert_eq!(down["available"], false);
        assert_eq!(down["reason"], "core-503");
        assert!(!down.to_string().contains("core text"));
    }
}
