//! What the client last saw when it synced, for `zone status`: `<state>/client-sync.json`
//! (mode 0600 like everything in the state dir). Every command that syncs writes it (`zone join`,
//! `zone run`, the note and file commands, the roster reads); `zone status` reads it without
//! reaching Core. It holds times and a failure kind, never
//! error text: Core's reason may quote a request.
//!
//! Several processes write it (a `zone run` that lives for days, and every `zone note` or
//! `zone members` beside it), and each holds the copy it loaded. A save therefore never moves a
//! time backwards: it keeps the newer of its own and the file's, so a long-lived run's stale
//! copy cannot put an old `lastSyncMs` over a newer one (the figure `zone status` showed stuck).
//! `lastLiveMs` is when the live session last applied a pushed note or roster, which needs no
//! sync and so never moved `lastSyncMs`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bp_zone_core::ZoneError;
use serde_json::{json, Value};

use crate::join;

#[derive(Debug, Default, PartialEq)]
pub struct SyncRecord {
    /// When a sync last went through, ms since the epoch.
    pub last_sync_ms: Option<u64>,
    /// When the running client syncs next; None while no `zone run` or `zone join` is waiting.
    pub next_sync_ms: Option<u64>,
    /// The kind ([`join::kind`]) of the last try, if it failed.
    pub last_failure: Option<String>,
    /// When the live session last applied a pushed message, ms since the epoch.
    pub last_live_ms: Option<u64>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn path(state: &Path) -> PathBuf {
    state.join("client-sync.json")
}

impl SyncRecord {
    /// The record in `state`; empty if none was written yet or it cannot be read.
    pub fn load(state: &Path) -> SyncRecord {
        let Some(v) = std::fs::read(path(state))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            return SyncRecord::default();
        };
        SyncRecord {
            last_sync_ms: v["lastSyncMs"].as_u64(),
            next_sync_ms: v["nextSyncMs"].as_u64(),
            last_failure: v["lastFailure"].as_str().map(str::to_owned),
            last_live_ms: v["lastLiveMs"].as_u64(),
        }
    }

    /// Write the record whole: a temp file renamed over the old one. The times that only move
    /// forward (`lastSyncMs`, `lastLiveMs`) keep the newer of this copy's and the file's.
    pub fn save(&self, state: &Path) -> Result<(), String> {
        let on_disk = SyncRecord::load(state);
        let body = json!({
            "lastSyncMs": self.last_sync_ms.max(on_disk.last_sync_ms),
            "nextSyncMs": self.next_sync_ms,
            "lastFailure": self.last_failure,
            "lastLiveMs": self.last_live_ms.max(on_disk.last_live_ms),
        });
        let file = path(state);
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, body.to_string())
            .and_then(|()| std::fs::rename(&tmp, &file))
            .map_err(|e| format!("{}: {}", file.display(), e.kind()))
    }

    /// Note a sync's outcome. The next sync is unknown until the caller decides its wait.
    pub fn synced(&mut self, result: Result<(), &ZoneError>) {
        match result {
            Ok(_) => {
                self.last_sync_ms = Some(now_ms());
                self.last_failure = None;
            }
            Err(e) => self.last_failure = Some(join::kind(e)),
        }
        self.next_sync_ms = None;
    }

    /// The live session applied a pushed message just now.
    pub fn live_applied(state: &Path) -> Result<(), String> {
        let mut record = SyncRecord::load(state);
        record.last_live_ms = Some(now_ms());
        record.save(state)
    }

    /// The next sync is `wait` from now.
    pub fn next_in(&mut self, wait: Duration) {
        self.next_sync_ms = Some(now_ms() + wait.as_millis() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_keeps_only_the_failure_kind() {
        crate::state::restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(SyncRecord::load(tmp.path()), SyncRecord::default());

        let mut r = SyncRecord::default();
        r.synced(Err(&ZoneError::Core {
            status: 0,
            reason: "Core unreachable: secret-looking text".into(),
        }));
        r.next_in(Duration::from_secs(10));
        r.save(tmp.path()).unwrap();
        let raw = std::fs::read_to_string(path(tmp.path())).unwrap();
        assert!(!raw.contains("secret-looking"), "{raw}");
        let back = SyncRecord::load(tmp.path());
        assert_eq!(back, r);
        assert_eq!(back.last_failure.as_deref(), Some("core-0"));
        assert_eq!(back.last_sync_ms, None);

        r.synced(Ok(()));
        assert_eq!(r.last_failure, None);
        assert_eq!(r.next_sync_ms, None);
        assert!(r.last_sync_ms.is_some());
        crate::state::check(tmp.path()).unwrap();
    }

    #[test]
    fn a_stale_copy_never_moves_the_times_backwards() {
        crate::state::restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        // A long-lived `zone run` loaded its copy when nothing had synced.
        let mut run = SyncRecord::load(tmp.path());
        // Another process synced and saved since.
        let mut other = SyncRecord::load(tmp.path());
        other.synced(Ok(()));
        other.save(tmp.path()).unwrap();
        let newer = other.last_sync_ms.unwrap();
        // The run's next save (a failed round, say) must not put `None` back over it.
        run.synced(Err(&ZoneError::Core {
            status: 502,
            reason: String::new(),
        }));
        run.save(tmp.path()).unwrap();
        let back = SyncRecord::load(tmp.path());
        assert_eq!(back.last_sync_ms, Some(newer));
        assert_eq!(back.last_failure.as_deref(), Some("core-502"));
    }

    #[test]
    fn a_live_message_moves_lastlivems_and_not_lastsyncms() {
        crate::state::restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        SyncRecord::live_applied(tmp.path()).unwrap();
        let back = SyncRecord::load(tmp.path());
        assert!(back.last_live_ms.is_some());
        assert_eq!(back.last_sync_ms, None);
        // A run's later save, from its own older copy, keeps it.
        let mut run = SyncRecord::default();
        run.synced(Ok(()));
        run.save(tmp.path()).unwrap();
        assert!(SyncRecord::load(tmp.path()).last_live_ms.is_some());
    }
}
