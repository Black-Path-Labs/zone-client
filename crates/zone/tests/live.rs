//! The live session end to end: two `zone run` seats registered at a stand-in SIP edge (SIP over
//! WebSocket, forking a MESSAGE to every other registered socket, as the zone's edge does),
//! a `zone note` handed to the first seat's run over its note socket, and the second seat taking
//! it from the push before any sync. The zone, its owner and the mailbox are a real zone service,
//! a throwaway one for tests (it trusts no SIP edge, so the stand-in checks nothing):
//!
//!   ZONE_TEST_CORE=http://127.0.0.1:<port> cargo test --test live -- --ignored
#![cfg(unix)]

use std::io::{BufRead, BufReader, ErrorKind};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use bp_workspace_core::mirror::{plain_http, HttpRequest, HttpTransport};
use bp_zone_core::sip::{answer, Message};
use bp_zone_core::{CoreTransport, HttpHeader, HttpReply, SecretVault, ZoneDevice, ZoneError};
use tungstenite::handshake::server::{Request, Response};
use tungstenite::http::HeaderValue;
use tungstenite::Message as Frame;

struct TestVault;

impl SecretVault for TestVault {
    fn wrap(&self, plaintext: Vec<u8>) -> Result<Vec<u8>, ZoneError> {
        Ok(plaintext.iter().map(|b| b ^ 0x5a).collect())
    }
    fn unwrap(&self, wrapped: Vec<u8>) -> Result<Vec<u8>, ZoneError> {
        Ok(wrapped.iter().map(|b| b ^ 0x5a).collect())
    }
}

struct PlainTransport;

impl CoreTransport for PlainTransport {
    fn send(
        &self,
        method: String,
        url: String,
        headers: Vec<HttpHeader>,
        body: Vec<u8>,
    ) -> Result<HttpReply, ZoneError> {
        let r = plain_http()
            .send(&HttpRequest {
                method: &method,
                url,
                headers: headers.into_iter().map(|h| (h.name, h.value)).collect(),
                body: &body,
            })
            .map_err(|reason| ZoneError::Core { status: 0, reason })?;
        Ok(HttpReply {
            status: r.status,
            body: r.body,
        })
    }
}

type Registered = Arc<Mutex<Vec<(usize, mpsc::Sender<String>)>>>;

/// A SIP-over-WebSocket edge: 200 to every REGISTER (no checks: no registrar is behind it), and every MESSAGE answered 200 and forwarded to every other registered socket.
fn edge() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let registered: Registered = Arc::default();
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let registered = Arc::clone(&registered);
            std::thread::spawn(move || serve(n, stream.unwrap(), &registered));
        }
    });
    url
}

#[allow(clippy::result_large_err)] // tungstenite's handshake callback type
fn serve(n: usize, stream: TcpStream, registered: &Registered) {
    let mut ws = tungstenite::accept_hdr(stream, |req: &Request, mut res: Response| {
        assert_eq!(req.headers()["Sec-WebSocket-Protocol"], "sip");
        assert!(req.headers().contains_key("Origin"));
        res.headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("sip"));
        Ok(res)
    })
    .unwrap();
    ws.get_mut()
        .set_read_timeout(Some(Duration::from_millis(10)))
        .unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    loop {
        while let Ok(raw) = rx.try_recv() {
            if ws.send(Frame::Text(raw)).is_err() {
                return;
            }
        }
        let text = match ws.read() {
            Ok(Frame::Text(t)) => t,
            Ok(Frame::Close(_)) => return,
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => continue,
            Err(_) => return,
        };
        let Some(m) = Message::parse(&text) else {
            continue;
        };
        match m.method() {
            Some("REGISTER") => {
                if m.header("Expires") != Some("0") {
                    let mut r = registered.lock().unwrap();
                    r.retain(|(id, _)| *id != n);
                    r.push((n, tx.clone()));
                }
                let _ = ws.send(Frame::Text(answer(&m, 200, "OK")));
            }
            Some("MESSAGE") => {
                let _ = ws.send(Frame::Text(answer(&m, 200, "OK")));
                for (id, out) in registered.lock().unwrap().iter() {
                    if *id != n {
                        let _ = out.send(text.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

fn zone(state: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_zone"));
    c.arg("--state").arg(state).env_remove("XDG_DATA_HOME");
    c
}

/// Join `state` by an invite from `owner`, the owner syncing until the seat is admitted.
fn seat(owner: &ZoneDevice, dir: &Path, name: &str) -> std::path::PathBuf {
    let state = dir.join(name);
    let invite = dir.join(format!("{name}.invite"));
    std::fs::write(&invite, owner.create_invite().unwrap().link).unwrap();
    std::fs::set_permissions(&invite, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let mut join = zone(&state)
        .args(["join", "--timeout", "60", "--invite-file"])
        .arg(&invite)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status = loop {
        if let Some(s) = join.try_wait().unwrap() {
            break s;
        }
        let _ = owner.sync(0);
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(status.success(), "{name} joins");
    state
}

/// `zone run` on `state` against the stand-in edge, its stderr lines sent to the returned channel.
fn run(state: &Path, edge: &str) -> (Child, mpsc::Receiver<String>) {
    let mut child = zone(state)
        .args(["-v", "run", "--sip-ws", edge, "--sip-domain", "sip.test"])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let err = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    (child, rx)
}

fn wait_for(rx: &mpsc::Receiver<String>, what: &str, within: Duration) -> String {
    let until = Instant::now() + within;
    while let Ok(line) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
        if line.contains(what) {
            return line;
        }
    }
    panic!("no `{what}` within {within:?}");
}

fn stop(mut child: Child) {
    // SAFETY: kill only sends SIGTERM to the child's pid.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    assert!(child.wait().unwrap().success());
}

#[test]
#[ignore = "needs a Core"]
fn a_note_reaches_a_running_seat_over_the_live_session_before_any_sync() {
    let core = std::env::var("ZONE_TEST_CORE").expect("ZONE_TEST_CORE names a zone service");
    let tmp = tempfile::tempdir().unwrap();
    let owner = ZoneDevice::open(
        tmp.path().join("owner").to_string_lossy().into_owned(),
        "Owner".into(),
        core,
        "ca-mb-1".into(),
        Arc::new(TestVault),
        Arc::new(PlainTransport),
    )
    .unwrap();
    owner.create_zone("Live".into(), None).unwrap();
    let a = seat(&owner, tmp.path(), "a");
    let b = seat(&owner, tmp.path(), "b");
    owner.sync(0).unwrap();

    let edge = edge();
    let (run_a, err_a) = run(&a, &edge);
    let (run_b, err_b) = run(&b, &edge);
    wait_for(&err_a, "live: registered", Duration::from_secs(20));
    wait_for(&err_b, "live: registered", Duration::from_secs(20));
    assert!(a.join("live.sock").exists());
    // A second run on the same state is refused while the first holds the note socket.
    let second = zone(&a).args(["run", "--sip-ws", &edge]).output().unwrap();
    assert!(String::from_utf8_lossy(&second.stderr).contains("another `zone run`"));

    let sent = Instant::now();
    let out = zone(&a)
        .args(["-v", "note", "over the live session"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let item: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = item["id"].as_str().unwrap().to_owned();
    let trace = String::from_utf8_lossy(&out.stderr);
    assert!(
        trace.contains("pushed=true") && trace.contains("posted=true"),
        "{trace}"
    );

    wait_for(&err_b, &format!("live note {id}"), Duration::from_secs(5));
    eprintln!(
        "note seen by the other seat {} ms after `zone note` started",
        sent.elapsed().as_millis()
    );

    // It was posted too: the owner, who holds no live session, reads it from the mailbox.
    let got = owner.sync(0).unwrap().new_items;
    assert!(got
        .iter()
        .any(|i| i.id == id && i.text == "over the live session"));

    stop(run_a);
    stop(run_b);
    assert!(!a.join("live.sock").exists());
    // Without a run, `zone note` posts over HTTP as before.
    let out = zone(&b).args(["note", "over HTTP"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(owner
        .sync(0)
        .unwrap()
        .new_items
        .iter()
        .any(|i| i.text == "over HTTP"));
}
