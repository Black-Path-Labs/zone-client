//! The live session `zone run` holds: SIP over a WebSocket to the zone's edge, registered under
//! the zone's address of record, the same session a browser member holds. A note another member
//! pushes there (a SIP MESSAGE carrying the sealed body it also posts to Core) is answered 200
//! and handed to the SDK (`take_live`), which opens and checks it as a sync would. The SDK
//! applies only notes, rosters and the zone's name; everything else, and anything missed while
//! the session was down, comes with the next sync. Every (re)registration asks the run loop for
//! one sync, so a reconnect catches up.
//!
//! `zone note` hands its note to a running `zone run` over a unix socket in the state dir
//! (`live.sock`, owner only): the session seals it with the roster it already holds, pushes it,
//! then posts it to Core. No sync and no challenge stand before the push. Without a running
//! `zone run`, `zone note` posts over HTTP as before.
//!
//! The edge sees the zone's address, sealed bytes, sizes and timing; nothing here logs a body.

use std::collections::HashMap;
use std::io::ErrorKind;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use bp_zone_core::sip::{answer, LiveLeg, Message};
use bp_zone_core::{ZoneDevice, MAX_LIVE_BODY_BYTES};
use rand_core::{OsRng, RngCore};
#[cfg(unix)]
use serde_json::json;
use serde_json::Value;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message as Frame, WebSocket};

use crate::backoff::Backoff;
#[cfg(unix)]
use crate::items;
use crate::record::now_ms;
use crate::run;
use crate::transport::trace;

/// The registration's life; the edge caps a tunnel binding at 300 s.
const EXPIRES: u32 = 300;
/// Register again this long before the binding lapses.
const REFRESH: Duration = Duration::from_secs(240);
/// A WebSocket ping this often, so no proxy on the way closes an idle socket.
const KEEPALIVE: Duration = Duration::from_secs(30);
/// How long the session waits on the socket before it looks at what to send.
const POLL: Duration = Duration::from_millis(20);
/// How long a REGISTER waits for its answer.
const REGISTER_WAIT: Duration = Duration::from_secs(10);
/// How long a push waits for the edge's 2xx before the note goes by post alone.
const PUSH_WAIT: Duration = Duration::from_secs(2);
/// How long `zone note` waits for the running session's answer.
const NOTE_WAIT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "zone-cli";
const SOCKET: &str = "live.sock";

/// Where `zone run`'s live session connects. Without `--sip-ws` (or `ZONE_SIP_WS`) there is none.
#[derive(clap::Args, Clone)]
pub struct LiveArgs {
    /// The zone's SIP edge over WebSocket (`wss://…`), for the live session.
    #[arg(long, env = "ZONE_SIP_WS", value_name = "URL")]
    pub sip_ws: Option<String>,
    /// The zone's SIP domain, for the live session's registration (default: the zone service's
    /// host).
    #[arg(long, env = "ZONE_SIP_DOMAIN", value_name = "DOMAIN")]
    pub sip_domain: Option<String>,
    /// The Origin the live session's WebSocket presents; the edge admits only its zone apps'
    /// (default: the zone service's origin).
    #[arg(long, env = "ZONE_SIP_ORIGIN", value_name = "URL")]
    pub sip_origin: Option<String>,
    /// No live session, even with `--sip-ws` or `ZONE_SIP_WS` set.
    #[arg(long)]
    pub no_live: bool,
}

/// The edge's WebSocket URL, SIP domain and Origin, from the flags and the zone service's URL.
struct Edge {
    ws: String,
    domain: String,
    origin: String,
}

impl LiveArgs {
    /// The live session's edge, or None when it is off.
    pub fn wanted(&self) -> bool {
        !self.no_live && self.sip_ws.is_some()
    }

    fn edge(&self, device: &ZoneDevice) -> Result<Edge, String> {
        let ws = self.sip_ws.clone().ok_or("no --sip-ws")?;
        let core = url::Url::parse(&device.status().core_url).ok();
        let domain = match (&self.sip_domain, core.as_ref().and_then(|u| u.host_str())) {
            (Some(d), _) => d.clone(),
            (None, Some(h)) => h.to_owned(),
            (None, None) => return Err("pass --sip-domain".into()),
        };
        let origin = match (&self.sip_origin, &core) {
            (Some(o), _) => o.clone(),
            (None, Some(u)) => u.origin().ascii_serialization(),
            (None, None) => return Err("pass --sip-origin".into()),
        };
        Ok(Edge { ws, domain, origin })
    }
}

/// A body to push and where its outcome goes (the final status, or nothing on timeout).
struct Push {
    body: String,
    done: mpsc::Sender<u16>,
}

/// The session, shared between its socket thread, the note socket and the run loop.
pub struct Live {
    up: AtomicBool,
    out: Mutex<mpsc::Sender<Push>>,
    /// Set on every (re)registration and after a pushed roster: the run loop syncs once.
    catch_up: AtomicBool,
}

impl Live {
    /// Start the session's thread for `device`. It reconnects until the run stops.
    pub fn start(device: Arc<ZoneDevice>, args: LiveArgs) -> Arc<Live> {
        let (tx, rx) = mpsc::channel();
        let live = Arc::new(Live {
            up: AtomicBool::new(false),
            out: Mutex::new(tx),
            catch_up: AtomicBool::new(false),
        });
        let l = Arc::clone(&live);
        std::thread::spawn(move || session(&device, &args, &l, &rx));
        live
    }

    /// True once: the run loop should sync now (a registration came up, or a roster arrived).
    pub fn take_catch_up(&self) -> bool {
        self.catch_up.swap(false, Ordering::SeqCst)
    }

    /// Push a sealed body to the zone's other devices. True when the edge took it (2xx).
    fn push(&self, body: &str) -> bool {
        if !self.up.load(Ordering::SeqCst) || body.len() > MAX_LIVE_BODY_BYTES {
            return false;
        }
        let (done, outcome) = mpsc::channel();
        let sent = self
            .out
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(Push {
                body: body.to_owned(),
                done,
            });
        sent.is_ok()
            && outcome
                .recv_timeout(PUSH_WAIT)
                .is_ok_and(|code| (200..300).contains(&code))
    }
}

/// Connect, serve until the socket drops, wait the short retry, again; until the run stops.
fn session(device: &ZoneDevice, args: &LiveArgs, live: &Live, rx: &mpsc::Receiver<Push>) {
    let mut backoff = Backoff::default();
    let mut said = false;
    while !run::stopped() {
        let result = serve(device, args, live, rx);
        live.up.store(false, Ordering::SeqCst);
        let Err(e) = result else { return };
        let wait = backoff.failed("live", OsRng.next_u64());
        // One line per outage, not one per retry.
        if !said {
            eprintln!(
                "zone run: live session down ({e}); notes go over HTTP, retrying every {}s",
                wait.as_secs()
            );
            said = true;
        }
        let until = Instant::now() + wait;
        while !run::stopped() && Instant::now() < until {
            // A push queued while down fails at once; its note still goes by post.
            while rx.try_recv().is_ok() {}
            std::thread::sleep(POLL);
        }
    }
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

fn set_timeout(ws: &mut Socket) {
    let _ = match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(POLL)),
        MaybeTlsStream::Rustls(s) => s.get_ref().set_read_timeout(Some(POLL)),
        _ => Ok(()),
    };
}

fn send(ws: &mut Socket, raw: String) -> Result<(), String> {
    match ws.send(Frame::Text(raw)) {
        Ok(()) => Ok(()),
        Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
        Err(e) => Err(format!("send: {e}")),
    }
}

/// The next SIP message on the socket, None if none came within [`POLL`].
fn read(ws: &mut Socket) -> Result<Option<Message>, String> {
    match ws.read() {
        Ok(Frame::Text(t)) => Ok(Message::parse(&t)),
        Ok(Frame::Close(_)) => Err("the edge closed the session".into()),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e))
            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
        {
            Ok(None)
        }
        Err(e) => Err(format!("read: {e}")),
    }
}

/// One connection: register, then serve pushes both ways until it drops. Ok only on stop.
fn serve(
    device: &ZoneDevice,
    args: &LiveArgs,
    live: &Live,
    rx: &mpsc::Receiver<Push>,
) -> Result<(), String> {
    let edge = args.edge(device)?;
    let mut req = edge
        .ws
        .as_str()
        .into_client_request()
        .map_err(|e| format!("--sip-ws: {e}"))?;
    let origin = HeaderValue::from_str(&edge.origin).map_err(|_| "--sip-origin")?;
    req.headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("sip"));
    req.headers_mut().insert("Origin", origin);
    let (mut ws, _) = tungstenite::connect(req).map_err(|e| format!("connect: {e}"))?;
    set_timeout(&mut ws);
    let mut leg = LiveLeg::new(if edge.ws.starts_with("wss:") {
        "WSS"
    } else {
        "WS"
    });
    let creds = device
        .sip_credentials(edge.domain.clone())
        .map_err(|e| format!("credentials: {}", crate::join::kind(&e)))?;
    let aor = creds.aor.clone();
    send(&mut ws, leg.register(&creds, EXPIRES, USER_AGENT))?;
    let deadline = Instant::now() + REGISTER_WAIT;
    loop {
        if Instant::now() > deadline {
            return Err("no answer to the registration".into());
        }
        let Some(m) = read(&mut ws)? else { continue };
        if m.header("Call-ID") != Some(leg.register_call_id()) {
            continue;
        }
        match m.status() {
            Some(200) => break,
            Some(c) if c >= 200 => return Err(format!("the edge refused the registration ({c})")),
            _ => {}
        }
    }
    live.up.store(true, Ordering::SeqCst);
    live.catch_up.store(true, Ordering::SeqCst);
    trace("live: registered");
    let mut refresh = Instant::now() + REFRESH;
    let mut ping = Instant::now() + KEEPALIVE;
    let mut waiting: HashMap<String, (mpsc::Sender<u16>, Instant)> = HashMap::new();
    loop {
        if run::stopped() {
            let _ = send(&mut ws, leg.register(&creds, 0, USER_AGENT));
            let _ = ws.close(None);
            return Ok(());
        }
        while let Ok(p) = rx.try_recv() {
            let (raw, call_id) = leg.message(&aor, &p.body);
            send(&mut ws, raw)?;
            waiting.insert(call_id, (p.done, Instant::now() + PUSH_WAIT));
        }
        let now = Instant::now();
        if now >= refresh {
            let fresh = device
                .sip_credentials(edge.domain.clone())
                .map_err(|e| format!("credentials: {}", crate::join::kind(&e)))?;
            send(&mut ws, leg.register(&fresh, EXPIRES, USER_AGENT))?;
            refresh = now + REFRESH;
        }
        if now >= ping {
            send_ping(&mut ws)?;
            ping = now + KEEPALIVE;
        }
        let _ = ws.flush();
        waiting.retain(|_, (_, until)| now < *until);
        let Some(m) = read(&mut ws)? else { continue };
        if let Some(code) = m.status() {
            let call_id = m.header("Call-ID").unwrap_or_default();
            if call_id == leg.register_call_id() && code >= 300 {
                return Err(format!("the edge refused the refresh ({code})"));
            }
            if code >= 200 {
                if let Some((done, _)) = waiting.remove(call_id) {
                    let _ = done.send(code);
                }
            }
            continue;
        }
        take(device, live, &mut ws, &m)?;
    }
}

fn send_ping(ws: &mut Socket) -> Result<(), String> {
    match ws.send(Frame::Ping(vec![])) {
        Ok(()) => Ok(()),
        Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
        Err(e) => Err(format!("ping: {e}")),
    }
}

/// A request from the edge: answer first, then hand a push to the SDK.
fn take(device: &ZoneDevice, live: &Live, ws: &mut Socket, m: &Message) -> Result<(), String> {
    match m.method() {
        Some("ACK") | None => return Ok(()),
        // This session serves no calls: the uniform refusal.
        Some("INVITE") => return send(ws, answer(m, 480, "Temporarily Unavailable")),
        _ => send(ws, answer(m, 200, "OK"))?,
    }
    let Some(body) = m.live_body() else {
        return Ok(());
    };
    match device.take_live(body.to_owned()) {
        Ok(Some(s)) => {
            for i in &s.new_items {
                // Ids and times only, never text.
                eprintln!("zone run: live note {} at {}", i.id, now_ms());
            }
            if s.roster_changed || s.admitted_now {
                live.catch_up.store(true, Ordering::SeqCst);
            }
        }
        Ok(None) => {}
        Err(e) => trace(&format!("live: not applied ({})", crate::join::kind(&e))),
    }
    Ok(())
}

/// The note socket's path in `state`.
fn socket_path(state: &Path) -> PathBuf {
    state.join(SOCKET)
}

/// The note socket, removed when dropped.
pub struct NoteSocket(PathBuf);

impl Drop for NoteSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
/// Listen for `zone note` on the state dir's socket. Refused when another `zone run` holds it.
pub fn listen(
    state: &Path,
    device: Arc<ZoneDevice>,
    live: Arc<Live>,
) -> Result<NoteSocket, String> {
    let path = socket_path(state);
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err("another `zone run` is running on this state dir".into());
        }
        // Left by a run that did not stop cleanly.
        std::fs::remove_file(&path).map_err(|e| format!("{}: {}", path.display(), e.kind()))?;
    }
    let listener =
        UnixListener::bind(&path).map_err(|e| format!("{}: {}", path.display(), e.kind()))?;
    let guard = NoteSocket(path.clone());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {}", path.display(), e.kind()))?;
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (device, live) = (Arc::clone(&device), Arc::clone(&live));
            std::thread::spawn(move || {
                let _ = answer_note(conn, &device, &live);
            });
        }
    });
    Ok(guard)
}

#[cfg(unix)]
/// One `zone note` request: `{"note": TEXT, "to": [PSEUDONYM, …]}`, answered with the item, whether
/// the push and the post went through, and how long each took from the request.
fn answer_note(conn: UnixStream, device: &ZoneDevice, live: &Live) -> std::io::Result<()> {
    let mut line = String::new();
    BufReader::new(&conn).read_line(&mut line)?;
    let start = Instant::now();
    let reply = match serde_json::from_str::<Value>(&line) {
        Ok(req) => {
            let text = req["note"].as_str().unwrap_or_default().to_owned();
            let to = req["to"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            match device.add_note_live(to, text) {
                Ok(n) => {
                    let pushed = live.push(&n.body);
                    let pushed_ms = start.elapsed().as_millis() as u64;
                    let posted = post(device, &n.body);
                    json!({
                        "item": items::item_line(&n.item),
                        "pushed": pushed,
                        "pushed_ms": pushed.then_some(pushed_ms),
                        "posted": posted.is_ok(),
                        "posted_ms": posted.as_ref().ok().map(|()| start.elapsed().as_millis() as u64),
                        "error": posted.err(),
                    })
                }
                Err(e) => json!({"error": items::sdk_error("note", &e)}),
            }
        }
        Err(_) => json!({"error": "note: a malformed request on the note socket"}),
    };
    let mut conn = conn;
    writeln!(conn, "{reply}")
}

#[cfg(unix)]
/// Post the body to Core: three tries, a second apart. The error is a kind, never Core's text.
fn post(device: &ZoneDevice, body: &str) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        match device.post_live(body.to_owned()) {
            Ok(()) => return Ok(()),
            Err(e) => last = crate::join::kind(&e),
        }
    }
    Err(format!(
        "note: sent to the members online, but the zone service did not keep it ({last})"
    ))
}

#[cfg(unix)]
/// `zone note` through a running `zone run`: the item line, or None when no run answers.
pub fn send_note(state: &Path, text: &str, to: &[String]) -> Result<Option<Value>, String> {
    let Ok(mut conn) = UnixStream::connect(socket_path(state)) else {
        return Ok(None);
    };
    let _ = conn.set_read_timeout(Some(NOTE_WAIT));
    let started = Instant::now();
    writeln!(conn, "{}", json!({"note": text, "to": to})).map_err(|e| format!("note: {e}"))?;
    let mut line = String::new();
    BufReader::new(&conn)
        .read_line(&mut line)
        .map_err(|e| format!("note: no answer from `zone run` ({})", e.kind()))?;
    let reply: Value =
        serde_json::from_str(&line).map_err(|_| "note: `zone run` answered nothing readable")?;
    trace(&format!(
        "note via zone run: pushed={} pushed_ms={} posted={} posted_ms={} total_ms={}",
        reply["pushed"],
        reply["pushed_ms"],
        reply["posted"],
        reply["posted_ms"],
        started.elapsed().as_millis()
    ));
    if let Some(item) = reply.get("item").filter(|i| !i.is_null()) {
        if let Some(e) = reply["error"].as_str() {
            println!("{item}");
            return Err(e.to_owned());
        }
        return Ok(Some(item.clone()));
    }
    Err(reply["error"]
        .as_str()
        .unwrap_or("note: `zone run` refused the note")
        .to_owned())
}

/// Windows has no unix socket here: `zone note` posts over HTTP, and the live session still
/// receives.
#[cfg(not(unix))]
pub fn listen(state: &Path, _: Arc<ZoneDevice>, _: Arc<Live>) -> Result<NoteSocket, String> {
    Ok(NoteSocket(socket_path(state)))
}

#[cfg(not(unix))]
pub fn send_note(_: &Path, _: &str, _: &[String]) -> Result<Option<Value>, String> {
    Ok(None)
}
