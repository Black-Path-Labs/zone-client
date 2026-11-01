//! `zone run`: keep the joined zone synced until stopped. Each round is one SDK `sync`; after a
//! good one the client long-polls Core for news (SDK `wait_for_news`), after a failure it backs
//! off on the same schedule as `zone join` (backoff.rs). SIGINT or SIGTERM stops it between
//! syncs: a sync in progress finishes and is recorded, a long-poll or backoff wait is cut short,
//! and the client exits 0. A second signal exits at once.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use bp_zone_core::ZoneDevice;
use rand_core::{OsRng, RngCore};

use crate::backoff::{Backoff, RETRY};
use crate::device;
use crate::join;
use crate::record::SyncRecord;
use crate::transport::Http;

/// How long one long-poll waits for news before the next sync. Core caps it at 25 s.
const LONG_POLL_SECS: u32 = 20;
/// How often a wait checks for a stop signal.
const TICK: Duration = Duration::from_millis(100);

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    if STOP.swap(true, Ordering::SeqCst) {
        // SAFETY: _exit is async-signal-safe.
        unsafe { libc::_exit(130) }
    }
}

fn catch_signals() {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only touches an atomic and calls _exit, both async-signal-safe.
        unsafe {
            libc::signal(
                sig,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

fn stopped() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Sleep up to `wait`, or less if a stop signal arrives.
fn sleep(wait: Duration) {
    let until = std::time::Instant::now() + wait;
    while !stopped() {
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(TICK));
    }
}

/// Long-poll for news on a thread of its own, so a stop signal need not wait for Core's answer.
/// The poll holds no device lock and writes nothing, so leaving it behind at exit is safe. A
/// failed poll waits the short retry, so a Core that syncs but cannot long-poll is not spun on.
fn wait_for_news(device: &Arc<ZoneDevice>) {
    let (tx, rx) = mpsc::channel();
    let d = Arc::clone(device);
    std::thread::spawn(move || {
        let _ = tx.send(d.wait_for_news(LONG_POLL_SECS).is_ok());
    });
    while !stopped() {
        match rx.recv_timeout(TICK) {
            Ok(true) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Ok(false) => return sleep(RETRY),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Sync until stopped, or for `rounds` syncs.
pub fn run(state: &Path, rounds: Option<u32>) -> Result<(), String> {
    catch_signals();
    let device = device::open_existing(state, Arc::new(Http::default()))?
        .filter(|d| d.status().zone_id.is_some())
        .ok_or("no zone joined: run `zone join` first")?;
    let mut record = SyncRecord::load(state);
    let mut backoff = Backoff::default();
    let mut round = 0u32;
    while !stopped() {
        let synced = device.sync(0);
        record.synced(synced.as_ref().map(|_| ()));
        let wait = match &synced {
            Ok(s) => {
                if s.admitted_now {
                    eprintln!("zone run: admitted");
                }
                backoff.succeeded();
                None
            }
            Err(e) => {
                let kind = join::kind(e);
                let wait = backoff.failed(&kind, OsRng.next_u64());
                eprintln!(
                    "zone run: sync failed ({kind}); retrying in {}s",
                    wait.as_secs()
                );
                Some(wait)
            }
        };
        round += 1;
        let last = rounds.is_some_and(|n| round >= n);
        if !last {
            record.next_in(wait.unwrap_or(Duration::from_secs(LONG_POLL_SECS.into())));
        }
        record.save(state)?;
        if last {
            return Ok(());
        }
        match wait {
            Some(w) => sleep(w),
            None => wait_for_news(&device),
        }
    }
    record.next_sync_ms = None;
    record.save(state)?;
    eprintln!("zone run: stopped");
    Ok(())
}
