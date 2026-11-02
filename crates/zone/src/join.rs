//! `zone join`: join a zone through the SDK and, unless `--no-wait`, wait until admitted.
//!
//! An invite link is redeemed by the SDK's join; the owner's device admits on its next sync. A
//! zone code joins in person: the owner adds the join code this prints (Add a device). With
//! `--kit` the device also asks the zone's admitter with a join request the kit key signs (SDK
//! `request_admission`), so admission needs no human step. A request lives
//! `MAX_REQUEST_AGE_MS`; while waiting, a new one goes out when the last has expired, and after
//! a failed send (no grant in force yet: 404) the waiting backoff decides when.
//!
//! A join address (`za_…`) stands for a zone code its owner published: the SDK resolves it at
//! `--core` and refuses a code that does not hash to the address, then the join goes on as for
//! that zone code. With `--kit` that is the whole self-service join: no invite and no owner step.
//!
//! Every step is an SDK call. Running `zone join` again with the same code resumes the wait.
//! Each sync is noted for `zone status` (record.rs).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bp_workspace_core::agent_join::MAX_REQUEST_AGE_MS;
use bp_workspace_core::space::{is_join_address, InviteLink, ZoneCode, ZONE_CODE_PREFIX};
use bp_zone_core::{ZoneDevice, ZoneError};
use rand_core::{OsRng, RngCore};

use crate::backoff::{self, Backoff};
use crate::device;
use crate::record::SyncRecord;
use crate::secret::KitKey;
use crate::transport::Http;

/// The member kind a kit request asks for.
pub const KIND: &str = "agent";

/// How `zone join` ended, for its exit code.
pub enum Outcome {
    /// Admitted (or, with `--no-wait`, joined and asked).
    Done,
    /// The SDK or Core refused for good: the reason kind, and the SDK's message.
    Refused { kind: String, message: String },
    /// `--timeout` passed before admission.
    TimedOut,
}

pub struct Options<'a> {
    pub code: &'a str,
    /// The Core a join address is resolved at.
    pub core: &'a str,
    pub kit: Option<&'a KitKey>,
    pub wait: bool,
    pub timeout: Option<Duration>,
}

/// The reason kind of an SDK error: `core-<status>` for Core's answers (0: not reached), else
/// the error's name. Never the whole text, which may carry Core's reason.
pub fn kind(e: &ZoneError) -> String {
    match e {
        ZoneError::Core { status, .. } => format!("core-{status}"),
        ZoneError::NotAdmitted => "not-admitted".into(),
        ZoneError::NoZone => "no-zone".into(),
        ZoneError::BadCode { .. } => "bad-code".into(),
        ZoneError::NotAdmin => "not-admin".into(),
        ZoneError::NotFound => "not-found".into(),
        ZoneError::TooLarge { .. } => "too-large".into(),
        ZoneError::Vault { .. } => "vault".into(),
        ZoneError::Store { .. } => "store".into(),
        ZoneError::Crypto { .. } => "crypto".into(),
    }
}

/// Core not reached, a 5xx, or rate limiting: not an answer about this device.
fn transient(kind: &str) -> bool {
    matches!(kind.strip_prefix("core-"), Some(s) if s == "0" || s == "429" || (s.len() == 3 && s.starts_with('5')))
}

/// Worth another try while waiting: a "not yet" answer or a transient failure. Anything else is
/// a refusal.
fn retryable(kind: &str) -> bool {
    backoff::is_waiting(kind) || transient(kind)
}

fn refused(e: &ZoneError) -> Outcome {
    Outcome::Refused {
        kind: kind(e),
        message: e.to_string(),
    }
}

/// The code inside what the user pasted: the web app shares an invite as
/// `https://<zone app>/#join=bpzi1.…`.
fn bare_code(input: &str) -> &str {
    let input = input.trim();
    input.rsplit_once("#join=").map_or(input, |(_, c)| c)
}

pub fn run(state: &Path, opts: Options<'_>) -> Result<Outcome, String> {
    let mut code = bare_code(opts.code);
    let resolved;
    if is_join_address(code) {
        // Before any state: an address nobody publishes leaves nothing behind.
        match bp_zone_core::resolve_join_address(
            opts.core.to_owned(),
            code.to_owned(),
            Arc::new(Http::default()),
        ) {
            Ok(c) => resolved = c,
            Err(e) if transient(&kind(&e)) => return Err(e.to_string()),
            Err(e) => return Ok(refused(&e)),
        }
        code = &resolved;
    }
    let in_person = code.starts_with(ZONE_CODE_PREFIX);
    let zone = if in_person {
        ZoneCode::decode(code)
    } else {
        InviteLink::decode(code).map(|l| l.zone)
    }
    .map_err(|_| "not a zone code or invite link".to_owned())?;

    let device = device::open_or_create(
        state,
        &zone.core_url,
        &zone.core_zone,
        Arc::new(Http::default()),
    )?;
    // An invite is redeemed here; a spent, expired or unknown one is Core's 404, a refusal.
    match device.join(code.to_owned()) {
        Ok(()) => {}
        Err(e) if transient(&kind(&e)) => return Err(e.to_string()),
        Err(e) => return Ok(refused(&e)),
    }
    let zone_id = device.status().zone_id.unwrap_or_default();
    if device.status().admitted {
        return admitted(&device, &zone_id);
    }
    if in_person && opts.kit.is_none() {
        let join_code = device.join_code().map_err(|e| e.to_string())?;
        eprintln!("zone join: give the zone's owner this join code (Add a device): {join_code}");
    }

    let started = Instant::now();
    // As in the SDK's agent: kit requests and syncs back off apart.
    let mut backoff = Backoff::default();
    let mut request_backoff = Backoff::default();
    let mut request_due = opts.kit.is_some().then_some(Instant::now());
    let mut record = SyncRecord::load(state);
    loop {
        if let (Some(kit), Some(due)) = (opts.kit, request_due) {
            if Instant::now() >= due {
                match device.request_admission(kit.seed(), KIND) {
                    Ok(()) => {
                        request_backoff.succeeded();
                        eprintln!("zone join: asked the zone's admitter to admit this device");
                        request_due =
                            Some(Instant::now() + Duration::from_millis(MAX_REQUEST_AGE_MS));
                    }
                    Err(e) if retryable(&kind(&e)) => {
                        let wait = request_backoff.failed(&kind(&e), OsRng.next_u64());
                        eprintln!(
                            "zone join: admission request not taken ({}); asking again in {}s",
                            kind(&e),
                            wait.as_secs()
                        );
                        request_due = Some(Instant::now() + wait);
                    }
                    Err(e) => return Ok(refused(&e)),
                }
            }
        }
        if !opts.wait {
            println!("joined {zone_id}; waiting for admission (`zone join` again waits)");
            return Ok(Outcome::Done);
        }

        let synced = device.sync(0);
        record.synced(synced.as_ref().map(|_| ()));
        record.save(state)?;
        let wait = match synced {
            Ok(_) if device.status().admitted => return admitted(&device, &zone_id),
            // Synced but not on the roster yet: waiting, like Core's not-admitted.
            Ok(_) => backoff.failed("not-admitted", OsRng.next_u64()),
            Err(e) if retryable(&kind(&e)) => backoff.failed(&kind(&e), OsRng.next_u64()),
            Err(e) => return Ok(refused(&e)),
        };
        // Wake for whichever comes first: the next sync or the next kit request.
        let mut until = Instant::now() + wait;
        if let Some(due) = request_due {
            until = until.min(due);
        }
        if let Some(limit) = opts.timeout {
            if until >= started + limit {
                std::thread::sleep((started + limit).saturating_duration_since(Instant::now()));
                let synced = device.sync(0);
                record.synced(synced.as_ref().map(|_| ()));
                record.save(state)?;
                return match synced {
                    Ok(_) if device.status().admitted => admitted(&device, &zone_id),
                    _ => Ok(Outcome::TimedOut),
                };
            }
        }
        record.next_in(until.saturating_duration_since(Instant::now()));
        record.save(state)?;
        std::thread::sleep(until.saturating_duration_since(Instant::now()));
    }
}

fn admitted(device: &ZoneDevice, zone_id: &str) -> Result<Outcome, String> {
    let me = device
        .members()
        .into_iter()
        .find(|m| m.is_me)
        .map(|m| m.member_id);
    let pseudonym = match me {
        Some(p) => p,
        None => device::identity(device)?
            .and_then(|i| i.pseudonym)
            .ok_or("admitted, but the roster does not list this device yet")?,
    };
    println!("joined {zone_id} as {pseudonym}");
    Ok(Outcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_not_yet_unreachable_5xx_and_429_are_retried() {
        for k in ["core-404", "not-admitted", "core-0", "core-502", "core-429"] {
            assert!(retryable(k), "{k}");
        }
        for k in [
            "core-403", "core-401", "core-410", "bad-code", "crypto", "core-5",
        ] {
            assert!(!retryable(k), "{k}");
        }
    }

    #[test]
    fn an_invite_url_yields_its_code() {
        assert_eq!(
            bare_code(" https://zone.example/#join=bpzi1.abc \n"),
            "bpzi1.abc"
        );
        assert_eq!(bare_code("bpzc1.xyz"), "bpzc1.xyz");
        assert_eq!(bare_code(" za_abcdefghijklmnop\n"), "za_abcdefghijklmnop");
    }
}
