//! Head beats: `zone head` (show or set what this member is working on now), `zone head
//! MEMBER`, `zone head --watch` and `zone heads`. The SDK owns the head, its rules and the views
//! (`/head`, `/heads`, the head lines on `/who`).
//!
//! Setting a head goes through a running `zone run`: it holds the live session the head is
//! pushed over, sealed to every member's devices, and nothing is posted to Core. A head is sent
//! only when it changes; the run sends it again with each REGISTER refresh. `--watch` asks the
//! run to stream every head change it takes (the SDK's `on_head`) as JSON lines.

use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::Instant;

use bp_zone_core::head::{self, Head, HeadFilter, HeadHandler, HeadInput};
use bp_zone_core::ZoneDevice;
use serde_json::{json, Value};

use crate::live::Live;
use crate::record::now_ms;

/// What `zone head` sets. A field left out keeps its value from the current head, except
/// `blocked_on`, which goes when the state is not `blocked`; `reset` starts from an empty head.
#[derive(clap::Args, Clone, Default)]
pub struct SetArgs {
    /// The head's state: working, blocked, waiting or idle. (`--state` is the state dir.)
    #[arg(id = "set", long = "set", value_name = "STATE")]
    pub state: Option<String>,
    /// The task board id this member is on.
    #[arg(long, value_name = "ID")]
    pub task: Option<String>,
    /// One line: what this member is doing now.
    #[arg(long, value_name = "TEXT")]
    pub focus: Option<String>,
    /// One line: the next step.
    #[arg(long, value_name = "TEXT")]
    pub next: Option<String>,
    /// One line: what blocks it (with `--state blocked`).
    #[arg(long, value_name = "TEXT")]
    pub blocked_on: Option<String>,
    /// Members see only busy or idle.
    #[arg(long, conflicts_with = "public")]
    pub private: bool,
    /// Members see the whole head again.
    #[arg(long)]
    pub public: bool,
    /// Start from an empty head instead of the current one.
    #[arg(long)]
    pub reset: bool,
}

impl SetArgs {
    pub fn any(&self) -> bool {
        self.state.is_some()
            || self.task.is_some()
            || self.focus.is_some()
            || self.next.is_some()
            || self.blocked_on.is_some()
            || self.private
            || self.public
            || self.reset
    }

    fn to_json(&self) -> Value {
        json!({
            "state": self.state,
            "task": self.task,
            "focus": self.focus,
            "next": self.next,
            "blocked_on": self.blocked_on,
            "private": self.private,
            "public": self.public,
            "reset": self.reset,
        })
    }
}

/// The head `req` (a [`SetArgs::to_json`]) makes from `current`.
fn merged(current: Option<&Head>, req: &Value) -> HeadInput {
    let text = |k: &str| req[k].as_str().map(str::to_owned);
    let base = match current.filter(|_| req["reset"] != json!(true)) {
        Some(h) if !h.private => HeadInput {
            task_id: h.task_id.clone(),
            focus: h.focus.clone(),
            next: h.next.clone(),
            state: h.state.clone(),
            blocked_on: h.blocked_on.clone(),
            private: false,
        },
        Some(h) => HeadInput {
            state: if h.state == "idle" { "idle" } else { "working" }.into(),
            private: true,
            ..Default::default()
        },
        None => HeadInput {
            state: "working".into(),
            ..Default::default()
        },
    };
    let state = text("state").unwrap_or(base.state);
    let blocked_on = text("blocked_on").or(base.blocked_on);
    HeadInput {
        task_id: text("task").or(base.task_id),
        focus: text("focus").or(base.focus),
        next: text("next").or(base.next),
        blocked_on: blocked_on.filter(|_| state.eq_ignore_ascii_case("blocked")),
        state,
        private: if req["private"] == json!(true) {
            true
        } else if req["public"] == json!(true) {
            false
        } else {
            base.private
        },
    }
}

/// `zone run`'s answer to `{"head": SET}`: set the head and push it when it changed.
pub fn answer_set(device: &ZoneDevice, live: &Live, req: &Value, start: Instant) -> Value {
    let input = merged(device.my_head().as_ref(), req);
    let merged_ms = start.elapsed().as_millis();
    match device.head_set(input) {
        Ok(beat) => {
            let changed = beat.body.is_some();
            let sent_ms = now_ms();
            let sealed_ms = start.elapsed().as_millis();
            let pushed = beat.body.as_deref().is_some_and(|b| live.push(b));
            crate::transport::trace(&format!(
                "head set: merged {merged_ms} ms, sealed {sealed_ms} ms, edge took it {} ms",
                start.elapsed().as_millis()
            ));
            json!({
                "head": head::to_json(&beat.head),
                "changed": changed,
                "pushed": pushed,
                "sentMs": changed.then_some(sent_ms),
                "pushedMs": pushed.then_some(start.elapsed().as_millis() as u64),
            })
        }
        Err(e) => json!({"error": crate::items::sdk_error("head", &e)}),
    }
}

/// Sends each head change to the connection's writer.
struct ToWatcher(mpsc::Sender<Head>);

impl HeadHandler for ToWatcher {
    fn on_head(&self, head: Head) {
        let _ = self.0.send(head);
    }
}

#[cfg(unix)]
/// `zone run`'s answer to `{"watch": {"members": [...], "states": [...]}}`: one JSON line per
/// head change until the watcher goes. Starts with the heads the run already holds.
pub fn answer_watch(
    conn: std::os::unix::net::UnixStream,
    device: &ZoneDevice,
    filter: &Value,
) -> std::io::Result<()> {
    use std::io::Write;
    let list = |k: &str| -> Vec<String> {
        filter[k]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let filter = HeadFilter {
        members: list("members"),
        states: list("states"),
    };
    let (tx, rx) = mpsc::channel();
    let id = device.on_head(filter.clone(), Arc::new(ToWatcher(tx)));
    let mut conn = conn;
    let mut out = Ok(());
    for h in device.heads().iter().rev().filter(|h| filter.matches(h)) {
        out = writeln!(conn, "{}", line(h, None));
        if out.is_err() {
            break;
        }
    }
    if out.is_ok() {
        for h in rx {
            let applied = now_ms();
            if let Err(e) = writeln!(conn, "{}", line(&h, Some(applied))) {
                out = Err(e);
                break;
            }
        }
    }
    device.off_head(id);
    out
}

/// A head as a `--watch` line: the head, its one-line text, and when this run applied it.
fn line(h: &Head, applied_ms: Option<u64>) -> Value {
    let mut v = head::to_json(h);
    v["line"] = json!(head::line(h));
    v["appliedMs"] = json!(applied_ms);
    v
}

#[cfg(unix)]
fn connect(state: &Path) -> Result<std::os::unix::net::UnixStream, String> {
    std::os::unix::net::UnixStream::connect(crate::live::socket_path(state)).map_err(|_| {
        "head: no live session; start `zone run` first (a head travels only over it)".into()
    })
}

#[cfg(unix)]
/// Set this member's head through the running `zone run` (`req` as [`SetArgs::to_json`] makes
/// it): its answer, `{head, changed, pushed, sentMs, pushedMs}`.
pub fn set_value(state: &Path, req: &Value) -> Result<Value, String> {
    use std::io::{BufRead, BufReader, Write};
    let started = Instant::now();
    let mut conn = connect(state)?;
    writeln!(conn, "{}", json!({"head": req})).map_err(|e| format!("head: {e}"))?;
    let mut answer = String::new();
    BufReader::new(&conn)
        .read_line(&mut answer)
        .map_err(|e| format!("head: no answer from `zone run` ({})", e.kind()))?;
    let reply: Value =
        serde_json::from_str(&answer).map_err(|_| "head: `zone run` answered nothing readable")?;
    if let Some(e) = reply["error"].as_str() {
        return Err(e.to_owned());
    }
    crate::transport::trace(&format!(
        "head via zone run: changed={} pushed={} total_ms={}",
        reply["changed"],
        reply["pushed"],
        started.elapsed().as_millis()
    ));
    Ok(reply)
}

/// `zone head --set … --focus …`: set through the running `zone run`; prints its answer.
pub fn set(state: &Path, args: &SetArgs) -> Result<(), String> {
    println!("{}", set_value(state, &args.to_json())?);
    Ok(())
}

#[cfg(unix)]
/// `zone head --watch`: print each head change the running `zone run` takes, as JSON lines,
/// until interrupted or the run stops.
pub fn watch(state: &Path, members: &[String], states: &[String]) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Write};
    let mut conn = connect(state)?;
    writeln!(
        conn,
        "{}",
        json!({"watch": {"members": members, "states": states}})
    )
    .map_err(|e| format!("head: {e}"))?;
    for line in BufReader::new(&conn).lines() {
        let line = line.map_err(|e| format!("head: {}", e.kind()))?;
        println!("{line}");
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn set_value(_: &Path, _: &Value) -> Result<Value, String> {
    Err("head: needs a unix socket to reach `zone run`".into())
}

#[cfg(not(unix))]
pub fn watch(_: &Path, _: &[String], _: &[String]) -> Result<(), String> {
    Err("head: needs a unix socket to reach `zone run`".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> Head {
        Head {
            member_id: "m".into(),
            name: "M".into(),
            me: true,
            fingerprint: "f".into(),
            state: "blocked".into(),
            private: false,
            task_id: Some("T-1".into()),
            focus: Some("ship".into()),
            next: Some("merge".into()),
            blocked_on: Some("CI".into()),
            updated_ms: 1,
            seen_ms: 1,
        }
    }

    #[test]
    fn a_set_keeps_what_it_does_not_name() {
        let i = merged(Some(&current()), &json!({"focus": "review"}));
        assert_eq!(i.focus.as_deref(), Some("review"));
        assert_eq!(i.task_id.as_deref(), Some("T-1"));
        assert_eq!(i.blocked_on.as_deref(), Some("CI"));
        // Leaving blocked drops blocked_on.
        let i = merged(Some(&current()), &json!({"state": "working"}));
        assert_eq!((i.state.as_str(), i.blocked_on), ("working", None));
        // Reset starts empty.
        let i = merged(Some(&current()), &json!({"reset": true, "state": "idle"}));
        assert_eq!((i.focus, i.task_id), (None, None));
        // Private sticks until public.
        let i = merged(Some(&current()), &json!({"private": true}));
        assert!(i.private);
        let mut p = current();
        p.private = true;
        p.state = "busy".into();
        assert!(merged(Some(&p), &json!({"focus": "x"})).private);
        assert!(!merged(Some(&p), &json!({"public": true})).private);
    }
}
