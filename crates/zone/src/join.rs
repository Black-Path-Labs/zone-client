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
//! Waiting is a long-poll, as in `zone run`: each sync asks Core to hold the mailbox read until
//! something is posted. A Core that lets a device with an unanswered kit request wait that way
//! answers the moment the admitter's roster makes it a member. A Core that answers at once (an
//! older one, or no kit request) is polled every `FAST_POLL` for `FAST_WINDOW` after a kit
//! request, since an admitter that is online answers within about a second; after that, and for
//! a join that waits for a person, the waiting backoff decides.
//!
//! Every step is an SDK call. Running `zone join` again with the same code resumes the wait: it
//! syncs first, so a device admitted meanwhile is done without asking again. Each sync is noted
//! for `zone status` (record.rs). `-v` times each step (transport.rs).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bp_workspace_core::agent_join::MAX_REQUEST_AGE_MS;
use bp_workspace_core::magic_link;
use bp_workspace_core::space::{is_join_address, InviteLink, ZoneCode, ZONE_CODE_PREFIX};
use bp_zone_core::{ZoneDevice, ZoneError};
use rand_core::{OsRng, RngCore};

use crate::backoff::{self, Backoff};
use crate::device;
use crate::record::SyncRecord;
use crate::secret::KitKey;
use crate::transport::{trace, Http};

/// The member kind a kit request asks for.
pub const KIND: &str = "agent";
/// How long one waiting sync asks Core to hold the mailbox read. Core caps it at 25 s.
const LONG_POLL_SECS: u64 = 20;
/// A sync that came back sooner than this was not held: Core does not long-poll this device yet.
const HELD: Duration = Duration::from_secs(1);
/// After a kit request, while Core answers at once, how often to look and for how long.
const FAST_POLL: Duration = Duration::from_millis(250);
const FAST_WINDOW: Duration = Duration::from_secs(15);

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
        ZoneError::NotAMember { .. } => "not-a-member".into(),
        ZoneError::BadRecipients { .. } => "bad-recipients".into(),
        ZoneError::NotComplete => "not-complete".into(),
        ZoneError::RelayQuota { .. } => "relay-quota".into(),
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
    // One transport for the whole join, so every call after the first reuses its connection.
    let http = Arc::new(Http::default());
    let mut code = bare_code(opts.code);
    let resolved;
    if is_join_address(code) {
        // Before any state: an address nobody publishes leaves nothing behind.
        match bp_zone_core::resolve_join_address(
            opts.core.to_owned(),
            code.to_owned(),
            http.clone(),
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
    } else if magic_link::is_magic_link(code) {
        // The SDK's join checks the link's id against its secret before spending it.
        magic_link::MagicLink::decode(code).map(|l| l.zone)
    } else {
        InviteLink::decode(code).map(|l| l.zone)
    }
    .map_err(|_| "not a zone code or invite link".to_owned())?;

    let device = device::open_or_create(state, &zone.core_url, &zone.core_zone, http)?;
    let resuming = device.status().zone_id.as_deref() == Some(zone.workspace_id.as_str());
    // An invite is redeemed here; a spent, expired or unknown one is Core's 404, a refusal.
    match device.join(code.to_owned()) {
        Ok(()) => {}
        Err(e) if transient(&kind(&e)) => return Err(e.to_string()),
        Err(e) => return Ok(refused(&e)),
    }
    trace("joined");
    let zone_id = device.status().zone_id.unwrap_or_default();
    if device.status().admitted {
        return admitted(&device, &zone_id);
    }
    let mut record = SyncRecord::load(state);
    // A resumed wait: the admitter may have let this device in since. Look before asking again,
    // or the admitter refuses the new request as from a member.
    if resuming {
        let synced = device.sync(0);
        record.synced(synced.as_ref().map(|_| ()));
        record.save(state)?;
        if synced.is_ok() && device.status().admitted {
            return admitted(&device, &zone_id);
        }
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
    // When the last kit request (or this run's invite redeem) went through: polled fast for
    // FAST_WINDOW after it, on a Core that does not hold the read for a joiner.
    let mut asked: Option<Instant> = (!in_person && !resuming).then_some(Instant::now());
    // The last sync read the mailbox (Core counts this device a member) and was not held.
    let mut read_at_once = false;
    loop {
        if let (Some(kit), Some(due)) = (opts.kit, request_due) {
            if Instant::now() >= due {
                match device.request_admission(kit.seed(), KIND) {
                    Ok(()) => {
                        request_backoff.succeeded();
                        asked = Some(Instant::now());
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

        // Hold the read no longer than the time left.
        let left = opts.timeout.map_or(LONG_POLL_SECS, |t| {
            t.saturating_sub(started.elapsed()).as_secs()
        });
        let polled = Instant::now();
        let synced = device.sync(left.min(LONG_POLL_SECS) as u32);
        let held = polled.elapsed() >= HELD;
        record.synced(synced.as_ref().map(|_| ()));
        record.save(state)?;
        let fast = asked.is_some_and(|a| a.elapsed() < FAST_WINDOW);
        let was_read_at_once = read_at_once;
        read_at_once = synced.is_ok() && !held;
        let wait = match &synced {
            Ok(_) if device.status().admitted => return admitted(&device, &zone_id),
            // Core held the read and nothing came for this device: look again at once. A read
            // that was not held moved the cursor past what was there (a member at Core whose
            // roster is not posted yet), so the next one is held: also at once, but only once.
            Ok(_) if held || !was_read_at_once => Duration::ZERO,
            Err(e) if held && backoff::is_waiting(&kind(e)) => Duration::ZERO,
            // Not on the roster yet and Core does not hold the read: soon after a kit request
            // look again shortly, otherwise back off as for Core's not-admitted.
            Ok(_) if fast => FAST_POLL,
            Ok(_) => backoff.failed("not-admitted", OsRng.next_u64()),
            Err(e) if fast && backoff::is_waiting(&kind(e)) => FAST_POLL,
            Err(e) if retryable(&kind(e)) => backoff.failed(&kind(e), OsRng.next_u64()),
            Err(e) => return Ok(refused(e)),
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
