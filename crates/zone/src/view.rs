//! `zone members` and `zone grants`: who is in the joined zone and on what terms agents get in,
//! as JSON on stdout. Both read the roster this device last synced, after one sync when `--sync`
//! asks for it or the last sync is older than [`STALE_AFTER_MS`]; the SDK verifies every owner
//! signature before anything is shown. `syncedAtMs` says how old the roster is.
//!
//! Presence is coarse here: a capability that Core lists as registered shows `online: true`, and
//! one that is not shows `lastSeen` rounded down to the hour (UTC), never the minute. Only a
//! member that chose to offer a capability, or to run with `--presence`, has any; a member
//! offering nothing shows `presence: null`, which says nothing about whether it is around.
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

/// Unix milliseconds rounded down to the hour, as `YYYY-MM-DDTHH:00:00Z`. Last-seen is never
/// shown finer than this.
pub fn hour(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let hour = secs.rem_euclid(86_400) / 3_600;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:00:00Z")
}

/// A member's presence from the capabilities it offers: online if any is, else the newest
/// last-seen to the hour, else null (it offers none, or Core never saw them).
fn presence<'a>(offered: impl Iterator<Item = &'a bp_zone_core::CapabilityInfo> + Clone) -> Value {
    if offered.clone().any(|c| c.online) {
        return json!({"online": true});
    }
    match offered.filter_map(|c| c.last_seen_ms).max() {
        Some(ms) => json!({"online": false, "lastSeen": hour(ms)}),
        None => Value::Null,
    }
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
                    let mut v = json!({
                        "capId": c.cap_id,
                        "kind": c.kind,
                        "label": c.label,
                        "machine": c.machine_name,
                        "online": c.online,
                    });
                    if !c.online {
                        if let Some(ms) = c.last_seen_ms {
                            v["lastSeen"] = json!(hour(ms));
                        }
                    }
                    v
                })
                .collect();
            let mut v = json!({
                "memberId": m.member_id,
                "name": m.name,
                "kind": m.kind,
                "admin": m.is_admin,
                "me": m.is_me,
                "devices": m.devices,
                "capabilities": offered,
                "presence": presence(caps.iter().filter(|c| c.publisher_id == m.member_id)),
            });
            // Which grant admitted this member: the SDK fills it for the zone's owner only.
            if let Some(grant) = &m.admitted_by {
                v["admittedBy"] = json!(grant);
            }
            v
        })
        .collect();
    Ok(json!({
        "zoneId": device.status().zone_id,
        "admitted": device.status().admitted,
        "syncedAtMs": synced_at,
        "members": members,
    }))
}

/// A grant `zone grants` lists by default: in force and with a place left. A revoked or expired
/// grant, and one whose places are all taken, only clutters the list; `--all` shows them.
pub fn is_open(g: &bp_zone_core::GrantInfo) -> bool {
    g.in_force && g.places_left > 0
}

/// `zone grants`: each admission grant's places, window and terms, and the per-agent caps. The
/// grants that are full, expired or revoked are left out unless `all`; `hidden` counts them.
pub fn grants(state: &Path, sync: bool, all: bool) -> Result<Value, String> {
    let (device, synced_at) = open(state, sync, "grants")?;
    let listed = device.admission_grants().map_err(|e| e.to_string())?;
    let hidden = if all {
        0
    } else {
        listed.iter().filter(|g| !is_open(g)).count()
    };
    let grants: Vec<Value> = listed
        .into_iter()
        .filter(|g| all || is_open(g))
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
        "hidden": hidden,
        "agentCaps": caps,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_seen_is_rounded_down_to_the_hour() {
        // 2026-11-05T14:37:12.345Z
        let ms = 1_793_889_432_345_i64;
        assert_eq!(hour(ms), "2026-11-05T14:00:00Z");
        assert_eq!(hour(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            hour(951_782_400_000 + 23 * 3_600_000 + 59_999),
            "2000-02-29T23:00:00Z"
        );
        assert_eq!(hour(-1), "1969-12-31T23:00:00Z");
    }

    fn grant(in_force: bool, left: u64) -> bp_zone_core::GrantInfo {
        bp_zone_core::GrantInfo {
            grant_id: "agr_1".into(),
            delegate_member_id: None,
            delegate_fingerprint: String::new(),
            kinds: vec![],
            scope: vec![],
            max_members: 5,
            admitted: 5 - left,
            places_left: left,
            issued_at_ms: 0,
            not_after_ms: 0,
            revoked: false,
            in_force,
        }
    }

    #[test]
    fn full_expired_and_revoked_grants_are_not_open() {
        assert!(is_open(&grant(true, 1)));
        assert!(!is_open(&grant(true, 0)), "full");
        assert!(!is_open(&grant(false, 3)), "expired or revoked");
    }

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
