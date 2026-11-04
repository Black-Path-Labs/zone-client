//! `zone stream offer`, `zone stream open` and `zone send-file --live`: a stream session between
//! two members. The SDK publishes this device's `stream` capability, answers or places the call
//! through the zone's SIP edge, and runs the session encrypted between the two devices (DTLS for
//! `bytes`, DTLS-SRTP for `rtp`); a relay, when one is used, forwards only ciphertext. At the end
//! each device signs a stream receipt (byte counts, duration and, for `bytes`, a SHA-256 of the
//! bytes each way) and sends it to the other inside the session.
//!
//! This module copies stdin into a `bytes` stream and a `bytes` stream to stdout, plays media
//! files or capture commands into an `rtp` stream and records one, and prints the receipt as one
//! JSON line. Nothing here logs stream bytes or media.

use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bp_zone_core::media_files::{
    FrameReader, IvfReader, IvfWriter, OggOpusReader, OggOpusWriter, Source,
};
use bp_zone_core::{MediaTrack, SessionPath, StreamMode, StreamReceipt, StreamSession, ZoneDevice};
use serde_json::{json, Value};

use crate::items;
use crate::run;

/// How long each side waits for the other's signed receipt at the end.
const RECEIPT_WAIT_SECS: u32 = 30;
/// An `rtp` stream that carried media ends when none has arrived for this long.
const RTP_IDLE: Duration = Duration::from_secs(3);
/// The SIP port the zone's edge listens on unless `--sip-server` says otherwise.
const SIP_PORT: u16 = 5060;
/// How much of stdin goes into one `send`.
const CHUNK: usize = 64 * 1024;

/// Where the zone's SIP edge is. Both default to the host of the zone service the device uses.
#[derive(clap::Args, Clone, Default)]
pub struct SipArgs {
    /// The zone's SIP edge, HOST:PORT over UDP (default: the zone service's host, port 5060).
    #[arg(long, value_name = "HOST:PORT")]
    pub sip_server: Option<String>,
    /// The zone's SIP domain (default: the zone service's host).
    #[arg(long, value_name = "DOMAIN")]
    pub sip_domain: Option<String>,
}

/// `--mode`.
#[derive(clap::ValueEnum, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Reliable, ordered bytes: stdin on one side, stdout on the other.
    #[default]
    Bytes,
    /// Opus audio and VP8 video tracks.
    Rtp,
}

impl From<Mode> for StreamMode {
    fn from(m: Mode) -> StreamMode {
        match m {
            Mode::Bytes => StreamMode::Bytes,
            Mode::Rtp => StreamMode::Rtp,
        }
    }
}

/// The SIP server and domain for this device's zone.
fn edge(device: &ZoneDevice, sip: &SipArgs) -> Result<(String, String), String> {
    let host = || -> Result<String, String> {
        url::Url::parse(&device.status().core_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .ok_or_else(|| {
                "the zone service's address names no host; pass --sip-server and --sip-domain"
                    .to_owned()
            })
    };
    let domain = match &sip.sip_domain {
        Some(d) => d.clone(),
        None => host()?,
    };
    let server = match &sip.sip_server {
        Some(s) => s.clone(),
        None => format!("{}:{SIP_PORT}", host()?),
    };
    Ok((server, domain))
}

fn path_name(p: SessionPath) -> &'static str {
    match p {
        SessionPath::Direct => "direct",
        SessionPath::Relayed => "relayed",
    }
}

/// The receipt as the one JSON line both sides print.
pub fn receipt_line(session: &StreamSession, r: &StreamReceipt) -> Value {
    json!({
        "session_id": r.session_id,
        "zone_session_id": r.core_session_id,
        "peer": session.peer(),
        "mode": r.mode.as_str(),
        "path": r.path,
        "bytes_tx": r.bytes_tx,
        "bytes_rx": r.bytes_rx,
        "duration_ms": r.duration_ms,
        "sha256": r.sha256,
        "sha256_rx": r.sha256_rx,
        "channel": {
            "bytes_tx": r.channel.bytes_tx,
            "bytes_rx": r.channel.bytes_rx,
            "duration_ms": r.channel.duration_ms,
        },
        "receipt": r.signed,
        "peer_receipt": r.peer_receipt,
        "peer_agrees": r.peer_agrees,
        "peer_refused": r.peer_refused,
    })
}

/// Sign and swap receipts, end the session, and print the receipt line, with `extra` fields.
fn finish(
    session: &StreamSession,
    cmd: &str,
    to_stderr: bool,
    extra: Option<(&str, Value)>,
) -> Result<(), String> {
    let receipt = session
        .receipt(RECEIPT_WAIT_SECS)
        .map_err(|e| format!("{cmd}: {e}"));
    session.end();
    let receipt = receipt?;
    let mut line = receipt_line(session, &receipt);
    if let Some((k, v)) = extra {
        line[k] = v;
    }
    if to_stderr {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
    Ok(())
}

/// Copy `r` into a `bytes` stream until it ends (or a stop signal).
fn copy_in(session: &StreamSession, mut r: impl Read, cmd: &str) -> Result<(), String> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("{cmd}: reading input: {}", e.kind())),
        };
        session
            .send(buf[..n].to_vec())
            .map_err(|e| format!("{cmd}: {e}"))?;
        if run::stopped() {
            return Ok(());
        }
    }
}

/// A media source opened for reading: a file, or a capture command's stdout (no shell).
fn open_source(src: &Source) -> Result<(Box<dyn Read + Send>, Option<Child>), String> {
    match src {
        Source::File(p) => fs::File::open(p)
            .map(|f| (Box::new(BufReader::new(f)) as Box<dyn Read + Send>, None))
            .map_err(|e| format!("{}: {e}", p.display())),
        Source::Exec(argv) => {
            let mut child = Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("exec:{}: {e}", argv[0]))?;
            let out = child
                .stdout
                .take()
                .ok_or_else(|| format!("exec:{}: no stdout", argv[0]))?;
            Ok((Box::new(BufReader::new(out)), Some(child)))
        }
    }
}

/// Parse `--audio` or `--video` before any network call.
pub fn source(flag: &str, text: &str) -> Result<Source, String> {
    Source::parse(text).map_err(|e| format!("{flag}: {e}"))
}

/// Play `src` once into an `rtp` stream on `track`, each frame at its own media time.
fn play(
    session: Arc<StreamSession>,
    track: MediaTrack,
    src: Source,
) -> std::thread::JoinHandle<Result<(), String>> {
    std::thread::spawn(move || {
        let (r, mut child) = open_source(&src)?;
        let mut frames: Box<dyn FrameReader> = match track {
            MediaTrack::Audio => Box::new(OggOpusReader::new(r)),
            MediaTrack::Video => Box::new(IvfReader::new(r)),
        };
        let started = Instant::now();
        let result = loop {
            if run::stopped() {
                break Ok(());
            }
            match frames.next_frame() {
                Ok(Some((at, data))) => {
                    let due = started + at;
                    let now = Instant::now();
                    if due > now {
                        std::thread::sleep(due - now);
                    }
                    if let Err(e) = session.send_media(track, at.as_millis() as u64, data) {
                        break Err(e.to_string());
                    }
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(format!("reading the media source: {e}")),
            }
        };
        if let Some(c) = child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        result
    })
}

/// `zone stream open`: call `member`'s stream and send stdin (`bytes`) or the sources (`rtp`).
pub fn open(
    state: &Path,
    member: String,
    mode: Mode,
    quote: Option<String>,
    audio: Option<String>,
    video: Option<String>,
    sip: &SipArgs,
) -> Result<(), String> {
    const CMD: &str = "stream open";
    let audio = audio.map(|a| source("--audio", &a)).transpose()?;
    let video = video.map(|v| source("--video", &v)).transpose()?;
    match mode {
        Mode::Bytes if audio.is_some() || video.is_some() => {
            return Err(format!("{CMD}: --audio and --video need --mode rtp"))
        }
        Mode::Rtp if audio.is_none() && video.is_none() => {
            return Err(format!(
                "{CMD}: --mode rtp needs --audio SRC or --video SRC"
            ))
        }
        _ => {}
    }
    let device = items::joined(state, CMD)?;
    let (server, domain) = edge(&device, sip)?;
    run::catch_signals();
    let session = device
        .clone()
        .open_stream(member, mode.into(), quote, server, domain)
        .map_err(|e| format!("{CMD}: {e}"))?;
    eprintln!(
        "zone {CMD}: connected to {} ({})",
        session.peer(),
        path_name(session.path())
    );
    let sent = match mode {
        Mode::Bytes => copy_in(&session, std::io::stdin().lock(), CMD),
        Mode::Rtp => {
            let players: Vec<_> = [(MediaTrack::Audio, audio), (MediaTrack::Video, video)]
                .into_iter()
                .filter_map(|(t, s)| s.map(|s| play(session.clone(), t, s)))
                .collect();
            players
                .into_iter()
                .try_for_each(|p| {
                    p.join()
                        .unwrap_or_else(|_| Err("a media player failed".into()))
                })
                .map_err(|e| format!("{CMD}: {e}"))
        }
    };
    // The receipt is printed either way: it records what did go through.
    let done = finish(&session, CMD, false, None);
    sent.and(done)
}

/// What an `rtp` offer writes under `--record DIR`: `audio.opus` (Ogg Opus) and `video.ivf`
/// (IVF VP8, from the first keyframe), 0600, never replacing a file.
struct Recorder {
    dir: PathBuf,
    audio: Option<OggOpusWriter<BufWriter<fs::File>>>,
    video: Option<(IvfWriter, u64)>,
    files: Vec<String>,
}

impl Recorder {
    fn new(dir: PathBuf) -> Result<Recorder, String> {
        fs::create_dir_all(&dir).map_err(|e| format!("--record {}: {e}", dir.display()))?;
        Ok(Recorder {
            dir,
            audio: None,
            video: None,
            files: vec![],
        })
    }

    fn create(&mut self, name: &str) -> std::io::Result<fs::File> {
        let path = self.dir.join(name);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let f = opts.open(&path)?;
        self.files.push(path.display().to_string());
        Ok(f)
    }

    fn write(&mut self, track: MediaTrack, time_ms: u64, data: &[u8]) -> Result<(), String> {
        let dir = self.dir.clone();
        let err = |e: std::io::Error| format!("--record {}: {e}", dir.display());
        match track {
            MediaTrack::Audio => {
                if self.audio.is_none() {
                    let f = self.create("audio.opus").map_err(err)?;
                    self.audio = Some(OggOpusWriter::new(BufWriter::new(f)).map_err(err)?);
                }
                if let Some(w) = self.audio.as_mut() {
                    w.write_packet(data).map_err(err)?;
                }
            }
            MediaTrack::Video => {
                if let Some((w, first)) = self.video.as_mut() {
                    let at = Duration::from_millis(time_ms.saturating_sub(*first));
                    return w.write_frame(at, data).map_err(err);
                }
                let path = dir.join("video.ivf");
                if fs::symlink_metadata(&path).is_ok() {
                    return Err(format!("--record: {} exists", path.display()));
                }
                // Starts at the first keyframe; frames before it cannot be decoded.
                if let Some(w) = IvfWriter::create(&path, data).map_err(err)? {
                    self.files.push(path.display().to_string());
                    self.video = Some((w, time_ms));
                }
            }
        }
        Ok(())
    }

    fn close(self) -> Result<Vec<String>, String> {
        let dir = self.dir.clone();
        let err = |e: std::io::Error| format!("--record {}: {e}", dir.display());
        if let Some(w) = self.audio {
            w.finish().and_then(|mut b| b.flush()).map_err(err)?;
        }
        if let Some((w, _)) = self.video {
            w.finish().map_err(err)?;
        }
        Ok(self.files)
    }
}

/// `zone stream offer`: publish this device's stream, answer one caller, write what arrives to
/// stdout (`bytes`) or to `--record DIR` (`rtp`), and print the receipt.
pub fn offer(
    state: &Path,
    allow: Vec<String>,
    mode: Mode,
    record: Option<PathBuf>,
    sip: &SipArgs,
) -> Result<(), String> {
    const CMD: &str = "stream offer";
    if record.is_some() && mode == Mode::Bytes {
        return Err(format!(
            "{CMD}: --record needs --mode rtp; a bytes stream goes to stdout"
        ));
    }
    let allow = items::recipients(CMD, allow)?;
    let mut recorder = record.map(Recorder::new).transpose()?;
    let device = items::joined(state, CMD)?;
    let (server, domain) = edge(&device, sip)?;
    let cap = device
        .publish_stream(allow.clone(), "stream".into())
        .map_err(|e| items::sdk_error(CMD, &e))?;
    let offer = device
        .clone()
        .offer_stream(cap, allow, mode.into(), server, domain)
        .map_err(|e| format!("{CMD}: {e}"))?;
    run::catch_signals();
    eprintln!(
        "zone {CMD}: waiting for a caller ({})",
        StreamMode::from(mode).as_str()
    );
    let session = loop {
        if run::stopped() {
            offer.stop();
            return Ok(());
        }
        match offer.accept(1) {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => eprintln!("zone {CMD}: a caller could not connect: {e}"),
        }
    };
    // One caller per run.
    offer.stop();
    eprintln!(
        "zone {CMD}: {} connected ({})",
        session.peer(),
        path_name(session.path())
    );
    let received = match mode {
        Mode::Bytes => {
            let mut out = std::io::stdout().lock();
            loop {
                match session.recv() {
                    Ok(Some(b)) => {
                        if let Err(e) = out.write_all(&b).and_then(|()| out.flush()) {
                            break Err(format!("{CMD}: stdout: {}", e.kind()));
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(format!("{CMD}: {e}")),
                }
            }
        }
        Mode::Rtp => {
            let mut last: Option<Instant> = None;
            let mut result = Ok(());
            while !run::stopped() && session.is_open() {
                match session.recv_media(250) {
                    Ok(Some(f)) => {
                        last = Some(Instant::now());
                        if let Some(r) = recorder.as_mut() {
                            if let Err(e) = r.write(f.track, f.time_ms, &f.data) {
                                result = Err(format!("{CMD}: {e}"));
                                break;
                            }
                        }
                    }
                    Ok(None) if last.is_some_and(|t| t.elapsed() >= RTP_IDLE) => break,
                    Ok(None) => {}
                    Err(e) => {
                        result = Err(format!("{CMD}: {e}"));
                        break;
                    }
                }
            }
            result
        }
    };
    let files = recorder.map(Recorder::close).transpose();
    // With `bytes`, stdout carries the stream, so the receipt goes to stderr.
    let done = finish(
        &session,
        CMD,
        mode == Mode::Bytes,
        files
            .as_ref()
            .ok()
            .and_then(|f| f.as_ref())
            .map(|f| ("recorded", json!(f))),
    );
    received.and(files.map(|_| ())).and(done)
}

/// `zone send-file --live`: the file's bytes over a `bytes` stream to `member`, who runs
/// `zone stream offer`. Any size; both members online.
pub fn send_live(state: &Path, member: String, path: &Path, sip: &SipArgs) -> Result<(), String> {
    const CMD: &str = "send-file --live";
    let meta = fs::metadata(path).map_err(|e| format!("send-file: {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("send-file: {} is not a file", path.display()));
    }
    let file = fs::File::open(path).map_err(|e| format!("send-file: {}: {e}", path.display()))?;
    let device = items::joined(state, "send-file")?;
    let (server, domain) = edge(&device, sip)?;
    run::catch_signals();
    let session = device
        .clone()
        .open_stream(member, StreamMode::Bytes, None, server, domain)
        .map_err(|e| format!("{CMD}: {e}"))?;
    let sent = copy_in(&session, BufReader::new(file), CMD);
    let done = finish(&session, CMD, false, Some(("size", json!(meta.len()))));
    sent.and(done)
}
