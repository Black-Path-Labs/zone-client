//! `zone room`: meeting rooms of the zone. The SDK creates, lists, closes and invites under the
//! device proof, and a joined device runs the room end to end: every audio and video frame and
//! every data message is encrypted on the device under a room key that is sealed to the members
//! present and rotated on every join and leave, so the media server forwards what it cannot
//! read. Recording runs only on the requesting member's device, and only while every member
//! present has consented on their own device.
//!
//! This module prints rooms and the events of a stay as JSON lines, copies stdin to room data
//! and room data to stdout with `--data`, and answers recording requests from
//! `--consent-record` or a prompt on a terminal. An invite's token is never printed. Nothing
//! here logs room media, data or keys.

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bp_workspace_core::b64;
use bp_zone_core::{MediaTrack, RoomData, RoomSession, ZoneRoom};
use serde_json::{json, Value};

use crate::items::{self, sdk_error};
use crate::join;
use crate::run;
use crate::stream;

/// How long a joined device waits for its first room key before it asks to record.
const KEY_WAIT: Duration = Duration::from_secs(15);
/// How long a data line waits for a room key before it is dropped.
const DATA_KEY_WAIT: Duration = Duration::from_secs(10);

/// One room as a JSON line.
fn room_line(r: &ZoneRoom) -> Value {
    json!({
        "room_id": r.room_id,
        "name": r.name,
        "plugin": r.plugin,
        "created_at": r.created_at,
        "closed": r.closed,
        "mine": r.mine,
        "creator_kind": r.creator_kind,
        "acl": {
            "mode": r.join_acl.mode,
            "aors": r.join_acl.aors,
            "pseudonyms": r.join_acl.pseudonyms,
        },
    })
}

/// `zone room create`.
pub fn create(state: &Path, name: String, quote: Option<String>) -> Result<(), String> {
    let device = items::joined(state, "room create")?;
    let room = device
        .create_room(name, None, quote)
        .map_err(|e| sdk_error("room create", &e))?;
    println!("{}", room_line(&room));
    Ok(())
}

/// `zone room list`.
pub fn list(state: &Path) -> Result<(), String> {
    let device = items::joined(state, "room list")?;
    for r in device.rooms().map_err(|e| sdk_error("room list", &e))? {
        println!("{}", room_line(&r));
    }
    Ok(())
}

/// `zone room close`.
pub fn close(state: &Path, id: String) -> Result<(), String> {
    let device = items::joined(state, "room close")?;
    let room = device
        .close_room(id)
        .map_err(|e| sdk_error("room close", &e))?;
    println!("{}", room_line(&room));
    Ok(())
}

/// `zone room acl`: `mode` is `members`, `invite` or `listed` (with `aors` and `members`).
pub fn acl(
    state: &Path,
    id: String,
    mode: String,
    aors: Vec<String>,
    members: Vec<String>,
) -> Result<(), String> {
    if mode != "listed" && !(aors.is_empty() && members.is_empty()) {
        return Err("room acl: --aor and --member go with `listed`".into());
    }
    let device = items::joined(state, "room acl")?;
    let room = device
        .room_acl(id, mode, aors, members)
        .map_err(|e| sdk_error("room acl", &e))?;
    println!("{}", room_line(&room));
    Ok(())
}

/// `zone room invite`: the SDK signs the invite and sends its token to the invitee alone, as a
/// direct note. Prints the invite's id and expiry, never the token.
pub fn invite(state: &Path, id: String, invitee: String, ttl: u64) -> Result<(), String> {
    let device = items::joined(state, "room invite")?;
    let invite = device
        .room_invite(id, invitee, ttl)
        .map_err(|e| sdk_error("room invite", &e))?;
    println!(
        "{}",
        json!({
            "invite_id": invite.invite_id,
            "room_id": invite.room_id,
            "invitee": invite.invitee,
            "expires_at": invite.expires_at,
        })
    );
    Ok(())
}

/// `zone room join`'s options.
pub struct JoinOptions {
    pub audio: Option<String>,
    pub video: Option<String>,
    pub data: bool,
    pub for_secs: Option<u64>,
    pub record: Option<PathBuf>,
    /// `--consent-record yes|no`; None prompts on a terminal.
    pub consent: Option<bool>,
}

fn data_line(d: &RoomData) -> Value {
    let mut v = json!({"event": "data", "from": d.from, "member": d.member});
    match std::str::from_utf8(&d.data) {
        Ok(text) => v["text"] = json!(text),
        Err(_) => v["b64"] = json!(b64::encode(&d.data)),
    }
    v
}

/// Copy stdin to room data, one message per line, waiting for the room key when needed.
fn send_stdin(session: Arc<RoomSession>) {
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let since = Instant::now();
            loop {
                match session.send_data(line.clone().into_bytes()) {
                    Ok(true) => break,
                    Ok(false) if since.elapsed() < DATA_KEY_WAIT => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Ok(false) => {
                        eprintln!("zone room join: no room key yet; a data line was not sent");
                        break;
                    }
                    Err(e) => {
                        eprintln!(
                            "zone room join: a data line was not sent ({})",
                            join::kind(&e)
                        );
                        break;
                    }
                }
            }
        }
    });
}

/// Answer a recording request: from `--consent-record`, else by a prompt on a terminal, else
/// not at all (no consent, so nothing is recorded).
fn answer(
    session: &Arc<RoomSession>,
    request_id: &str,
    requester: &str,
    consent: Option<bool>,
    can_prompt: bool,
) {
    let give = |s: &RoomSession, yes: bool| match s.consent_recording(request_id.to_owned(), yes) {
        Ok(()) => println!(
            "{}",
            json!({"event": "recording-consent", "request_id": request_id, "consent": yes})
        ),
        Err(e) => eprintln!(
            "zone room join: the recording answer was not sent ({})",
            join::kind(&e)
        ),
    };
    match consent {
        Some(yes) => give(session.as_ref(), yes),
        None if can_prompt => {
            let (s, id, who) = (session.clone(), request_id.to_owned(), requester.to_owned());
            std::thread::spawn(move || {
                eprint!("zone room join: {who} asks to record this room. Consent? [y/N] ");
                let mut line = String::new();
                let yes = std::io::stdin().read_line(&mut line).is_ok()
                    && matches!(line.trim(), "y" | "Y" | "yes");
                match s.consent_recording(id.clone(), yes) {
                    Ok(()) => println!(
                        "{}",
                        json!({"event": "recording-consent", "request_id": id, "consent": yes})
                    ),
                    Err(e) => eprintln!(
                        "zone room join: the recording answer was not sent ({})",
                        join::kind(&e)
                    ),
                }
            });
        }
        None => eprintln!(
            "zone room join: {requester} asks to record this room; no consent given (answer with \
             --consent-record yes|no)"
        ),
    }
}

/// One room event line from the SDK as a JSON line.
fn event_line(line: &str) -> Value {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["participant", action @ ("joined" | "left"), display, kind] => json!({
            "event": format!("participant-{action}"),
            "display": display,
            "kind": kind,
        }),
        ["epoch", n] => json!({"event": "epoch", "epoch": n.parse::<u64>().ok()}),
        ["recording-request", id, requester] => {
            json!({"event": "recording-request", "request_id": id, "requester": requester})
        }
        ["recording:", state] => json!({"event": "recording", "on": *state == "on"}),
        _ => json!({"event": "room", "line": line}),
    }
}

/// `zone room join`: stay in the room until `--for` runs out, a stop signal, or the room ends,
/// then leave and print the leave line.
pub fn join_room(state: &Path, id: String, o: JoinOptions) -> Result<(), String> {
    const CMD: &str = "room join";
    // Sources and the record dir are checked before any network call.
    if let Some(a) = &o.audio {
        stream::source("--audio", a)?;
    }
    if let Some(v) = &o.video {
        stream::source("--video", v)?;
    }
    let record = o
        .record
        .as_deref()
        .map(|d| {
            d.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{CMD}: {} is not a UTF-8 path", d.display()))
        })
        .transpose()?;
    // stdin answers a prompt only when it is a terminal and not carrying room data.
    let can_prompt = !o.data && std::io::stdin().is_terminal();
    let device = items::joined(state, CMD)?;
    run::catch_signals();
    let session = device
        .clone()
        .join_room(
            id.clone(),
            None,
            o.audio.is_some(),
            o.video.is_some(),
            o.for_secs,
        )
        .map_err(|e| sdk_error(CMD, &e))?;
    let j = session.join_info();
    println!(
        "{}",
        json!({
            "event": "join",
            "room_id": j.room_id,
            "plugin": j.plugin,
            "via": j.via,
            "kind": j.kind,
            "display": session.display(),
            "expires_at": j.expires_at,
        })
    );
    if let Some(a) = o.audio {
        session
            .play(MediaTrack::Audio, a)
            .map_err(|e| sdk_error(CMD, &e))?;
    }
    if let Some(v) = o.video {
        session
            .play(MediaTrack::Video, v)
            .map_err(|e| sdk_error(CMD, &e))?;
    }
    if o.data {
        send_stdin(session.clone());
    }
    let joined_at = Instant::now();
    let mut to_record = record;
    let mut my_request: Option<String> = None;
    while !run::stopped() && session.is_open() {
        // Ask once this device holds the room key, so the request is sealed for this epoch.
        if to_record.is_some() && (session.keyed() || joined_at.elapsed() >= KEY_WAIT) {
            let dir = to_record.take().unwrap_or_default();
            match session.request_recording(dir) {
                Ok(rid) => {
                    println!(
                        "{}",
                        json!({"event": "recording-requested", "request_id": rid})
                    );
                    my_request = Some(rid);
                }
                Err(e) => eprintln!(
                    "zone {CMD}: the recording request was not sent ({})",
                    join::kind(&e)
                ),
            }
        }
        if let Some(line) = session.next_event(200) {
            let event = event_line(&line);
            println!("{event}");
            if event["event"] == "recording" {
                eprintln!("{line}");
            }
            if event["event"] == "recording-request" {
                let rid = event["request_id"].as_str().unwrap_or_default();
                if my_request.as_deref() != Some(rid) {
                    let who = event["requester"].as_str().unwrap_or_default();
                    answer(&session, rid, who, o.consent, can_prompt);
                }
            }
        }
        while let Some(d) = session.recv_data(0) {
            if o.data {
                println!("{}", data_line(&d));
            }
        }
        // Frames are recorded inside the SDK when this device records; the copies here go.
        while session.recv_frame(0).is_some() {}
    }
    let out = session.leave();
    println!(
        "{}",
        json!({
            "event": "leave",
            "room_id": id,
            "reason": out.reason,
            "epoch": out.epoch,
            "sent_audio": out.sent_audio,
            "sent_video": out.sent_video,
            "sent_data": out.sent_data,
            "recv_audio": out.recv_audio,
            "recv_video": out.recv_video,
            "recv_data": out.recv_data,
            "undecryptable": out.undecryptable,
        })
    );
    Ok(())
}
