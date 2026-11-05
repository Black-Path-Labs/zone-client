//! `zone fs`: a member's exported source tree, browsed and searched by other members. Everything
//! here is an adapter over the SDK's `bp-zone-fs`: the owner runs `zone fs serve`, which answers
//! each caller's session on this host under the exports and grants in the state dir; a requester
//! runs `zone fs <agent> ls|cat|tree|stat|grep|find`, one session per command. Path policy,
//! grants, budgets, search and receipts are the SDK's; this module parses arguments, resolves the
//! agent from the verified roster and prints.
//!
//! State (all under `<state>/fs`, 0700, files 0600): `exports.json` (label, root, denies), `grants/`
//! (one owner-signed JWS per grant, plus `revoked`), `receipts.jsonl` (one receipt per request).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use bp_zone_core::stream::FS_CAP_LABEL;
use bp_zone_core::{StreamMode, StreamSession, ZoneDevice};
use bp_zone_fs::grant::{GrantSpec, GrantStore, Limits, Subject};
use bp_zone_fs::receipt::ReceiptLog;
use bp_zone_fs::search::GrepArgs;
use bp_zone_fs::server::ExportConfig;
use bp_zone_fs::{Actor, Code, FsClient, FsError, Server, Signer};
use serde_json::{json, Value};

use crate::items;
use crate::run;
use crate::stream::{edge, SipArgs};

/// A zone fs server is a `folder` capability (read-only access to a tree, which is what it is)
/// labelled [`FS_CAP_LABEL`], answered as a bytes stream session carrying `bp-zone-fs-v1`.
const FS_CAP_KIND: &str = "folder";

/// `zone fs` subcommands. `zone fs <agent> <op> ...` is the target-first form.
#[derive(clap::Subcommand)]
pub enum FsCommand {
    /// List the members exporting to you, with their export labels, operations and expiry.
    Ls {
        #[command(flatten)]
        sip: SipArgs,
        #[arg(long)]
        json: bool,
    },
    /// Export a directory under LABEL. Only what a grant names is reachable, and the mandatory
    /// deny-list (.git, .env*, *.key, *.pem, credentials, owner-only files) always applies.
    Export {
        label: String,
        dir: PathBuf,
        /// Extra gitignore-style patterns to hide (repeatable). They only add denies.
        #[arg(long = "deny", value_name = "PATTERN")]
        deny: Vec<String>,
    },
    /// Stop exporting LABEL: its grants are revoked first.
    Unexport { label: String },
    /// Your exports.
    Exports,
    /// Grant MEMBER (a name or pseudonym; `zone` for every member) access to LABEL, for at most
    /// an hour (15 minutes by default).
    Grant {
        member: String,
        label: String,
        /// Path prefixes inside the export (repeatable; default the whole export).
        #[arg(long = "path", value_name = "PREFIX")]
        paths: Vec<String>,
        /// Operations: metadata, read, grep, find, symbols (comma-separated).
        #[arg(long, value_delimiter = ',', default_value = "metadata,read,grep,find")]
        ops: Vec<String>,
        /// Lifetime in minutes (at most 60).
        #[arg(long, default_value_t = 15)]
        minutes: u64,
        /// Per-hour budgets; defaults are the protocol's (120 metadata, 60 reads, 20 searches,
        /// 64 MiB, burst 10).
        #[arg(long)]
        metadata_per_hour: Option<u64>,
        #[arg(long)]
        reads_per_hour: Option<u64>,
        #[arg(long)]
        searches_per_hour: Option<u64>,
        #[arg(long)]
        mib_per_hour: Option<u64>,
        #[arg(long)]
        burst: Option<u64>,
    },
    /// Revoke a grant by id: every request under it stops at its next frame.
    Revoke { grant_id: String },
    /// Your grants, live and revoked.
    Grants,
    /// Answer zone fs sessions on this host until interrupted.
    Serve {
        #[command(flatten)]
        sip: SipArgs,
        /// Publish a new capability naming the current members (kept ids are still answered).
        #[arg(long)]
        republish: bool,
    },
    /// `zone fs <agent> <op> ...`.
    #[command(external_subcommand)]
    Target(Vec<String>),
}

/// The requester's operations, after the agent.
#[derive(clap::Parser)]
#[command(name = "zone fs <agent>", no_binary_name = true)]
struct TargetCli {
    agent: String,
    #[command(subcommand)]
    op: Op,
    #[command(flatten)]
    sip: SipArgs,
}

#[derive(clap::Subcommand)]
enum Op {
    /// A directory's entries (no path: the agent's exports).
    Ls {
        #[arg(default_value = "")]
        path: String,
        #[arg(long)]
        json: bool,
    },
    /// A file's bytes to stdout.
    Cat {
        path: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        length: Option<u64>,
    },
    /// Every path under a directory.
    Tree {
        path: String,
        #[arg(long)]
        depth: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// One path's metadata, as JSON.
    Stat { path: String },
    /// Lines matching a regular expression, ripgrep style, searched on the owner's host.
    Grep {
        pattern: String,
        /// Where to search (default: every export).
        #[arg(default_value = "")]
        path: String,
        /// A ripgrep file type (repeatable): rust, ts, py...
        #[arg(long = "type", short = 't', value_name = "TYPE")]
        types: Vec<String>,
        /// A ripgrep glob over the export path (repeatable; `!GLOB` excludes).
        #[arg(long = "glob", short = 'g', value_name = "GLOB")]
        globs: Vec<String>,
        #[arg(long, short = 'F')]
        fixed_strings: bool,
        #[arg(long, short = 'i')]
        ignore_case: bool,
        #[arg(long)]
        max_count: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Paths containing NAME (or matching it with --glob).
    Find {
        name: String,
        #[arg(default_value = "")]
        path: String,
        #[arg(long)]
        glob: bool,
        #[arg(long, short = 'i')]
        ignore_case: bool,
    },
    /// Mount an export read-only at DIR until interrupted (Linux, with FUSE).
    Mount {
        dir: PathBuf,
        /// The remote path to mount (default: the list of exports).
        #[arg(default_value = "")]
        root: String,
    },
    /// Time ls, cat and grep over one session (20 runs each by default), as JSON.
    #[command(hide = true)]
    Bench {
        #[arg(long)]
        ls: String,
        #[arg(long)]
        cat: String,
        #[arg(long)]
        grep: String,
        #[arg(long, default_value = "")]
        grep_path: String,
        #[arg(long, default_value_t = 20)]
        runs: usize,
    },
}

/// A `zone fs` refusal: the stable code first, then plain words.
fn refusal(e: &FsError) -> String {
    match e.retry_after_s {
        Some(s) => format!(
            "zone fs: {}: {} (retry in {s} s)",
            e.code.as_str(),
            e.message
        ),
        None => format!("zone fs: {}: {}", e.code.as_str(), e.message),
    }
}

fn fs_dir(state: &Path) -> Result<PathBuf, String> {
    let d = state.join("fs");
    fs::create_dir_all(&d).map_err(|e| format!("zone fs: {}: {e}", d.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&d, fs::Permissions::from_mode(0o700));
    }
    Ok(d)
}

fn load_exports(state: &Path) -> Result<Vec<ExportConfig>, String> {
    let p = fs_dir(state)?.join("exports.json");
    let Ok(text) = fs::read_to_string(&p) else {
        return Ok(vec![]);
    };
    let v: Value = serde_json::from_str(&text).map_err(|_| "zone fs: exports.json is corrupt")?;
    Ok(v.as_array()
        .map(|a| a.iter().filter_map(ExportConfig::from_json).collect())
        .unwrap_or_default())
}

fn save_exports(state: &Path, exports: &[ExportConfig]) -> Result<(), String> {
    let p = fs_dir(state)?.join("exports.json");
    let v = Value::Array(exports.iter().map(ExportConfig::to_json).collect());
    fs::write(&p, v.to_string().as_bytes()).map_err(|e| format!("zone fs: {}: {e}", p.display()))
}

/// The zone device as a `bp-zone-fs` signer: its rostered key signs, the verified roster checks.
pub struct DeviceSigner {
    device: Arc<ZoneDevice>,
    member: String,
}

impl DeviceSigner {
    pub fn new(device: Arc<ZoneDevice>) -> Result<DeviceSigner, String> {
        let member = device
            .members()
            .into_iter()
            .find(|m| m.is_me)
            .map(|m| m.member_id)
            .ok_or("zone fs: this device is not on the zone's roster yet")?;
        Ok(DeviceSigner { device, member })
    }
}

impl Signer for DeviceSigner {
    fn member(&self) -> String {
        self.member.clone()
    }

    fn sign(&self, payload: &Value, typ: &str) -> bp_zone_fs::Result<String> {
        self.device
            .sign_member_document(typ, payload)
            .map_err(|e| FsError::new(Code::Grant, format!("could not sign: {e}")))
    }

    fn verify(&self, jws: &str, typ: &str) -> Option<(String, Value)> {
        self.device.verify_member_document(jws, typ)
    }
}

/// A member by full pseudonym, or by the one roster card whose name matches case-insensitively
/// (ZF-D16).
pub fn resolve_member(device: &ZoneDevice, who: &str) -> Result<String, FsError> {
    let members = device.members();
    if let Some(m) = members.iter().find(|m| m.member_id == who) {
        return Ok(m.member_id.clone());
    }
    let named: Vec<_> = members
        .iter()
        .filter(|m| m.name.eq_ignore_ascii_case(who))
        .collect();
    match named.as_slice() {
        [m] => Ok(m.member_id.clone()),
        [] => Err(FsError::new(
            Code::AgentNotFound,
            format!("no member named {who:?}"),
        )),
        many => Err(FsError::new(
            Code::AgentAmbiguous,
            format!(
                "{} members are named {who:?}; use a pseudonym: {}",
                many.len(),
                many.iter()
                    .map(|m| m.member_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// The owner side, built from the state dir.
fn server(state: &Path, device: &Arc<ZoneDevice>) -> Result<Server, String> {
    let signer = Arc::new(DeviceSigner::new(device.clone())?);
    let dir = fs_dir(state)?;
    let grants =
        GrantStore::open(&dir.join("grants"), &signer.member()).map_err(|e| refusal(&e))?;
    let zone = device.status().zone_id.unwrap_or_default();
    let shares = bp_zone_fs::share::ShareStore::open(&dir.join("shares"), &signer.member())
        .map_err(|e| refusal(&e))?;
    let srv = Server::new(
        &zone,
        signer,
        grants,
        ReceiptLog::file(dir.join("receipts.jsonl")),
    )
    .with_shares(shares);
    for cfg in load_exports(state)? {
        let label = cfg.label.clone();
        if let Err(e) = srv.add_export(cfg) {
            eprintln!("zone fs: export {label} skipped: {}", refusal(&e));
        }
    }
    Ok(srv)
}

/// A stream session as the byte pipe `bp-zone-fs` frames ride on.
pub struct SessionConn(pub Arc<StreamSession>);

impl bp_zone_fs::Conn for SessionConn {
    fn send(&self, bytes: Vec<u8>) -> std::io::Result<()> {
        self.0
            .send(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string()))
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        self.0
            .recv()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string()))
    }
}

/// Open a zone fs session to `member`'s server.
pub fn connect(
    device: &Arc<ZoneDevice>,
    member: &str,
    sip: &SipArgs,
) -> Result<(Arc<StreamSession>, FsClient), String> {
    let caps = device
        .refresh_capabilities()
        .unwrap_or_else(|_| device.capabilities());
    let cap = caps
        .iter()
        .filter(|c| {
            c.kind == FS_CAP_KIND && c.label == FS_CAP_LABEL && c.publisher_id == member && !c.mine
        })
        .max_by_key(|c| (c.online, c.last_seen_ms))
        .ok_or_else(|| {
            refusal(&FsError::new(
                Code::ExportNotFound,
                "that member runs no zone fs server",
            ))
        })?;
    let (server, domain) = edge(device, sip)?;
    let session = device
        .clone()
        .open_stream_cap(
            cap.cap_id.clone(),
            member.to_owned(),
            StreamMode::Bytes,
            server,
            domain,
        )
        .map_err(|e| format!("zone fs: {e}"))?;
    let client = FsClient::new(Arc::new(SessionConn(session.clone())));
    Ok((session, client))
}

fn close(session: &StreamSession) {
    let _ = session.finish();
    session.end();
}

pub fn command(state: &Path, command: FsCommand) -> Result<(), String> {
    match command {
        FsCommand::Export { label, dir, deny } => {
            let mut exports = load_exports(state)?;
            if exports.iter().any(|e| e.label == label) {
                return Err(format!("zone fs: an export named {label} exists"));
            }
            let root =
                fs::canonicalize(&dir).map_err(|e| format!("zone fs: {}: {e}", dir.display()))?;
            let cfg = ExportConfig::new(&label, root, deny).map_err(|e| refusal(&e))?;
            // Open it once now, so a bad root or pattern fails here and not at serve time.
            bp_zone_fs::policy::ExportRoot::open(&cfg.root, &cfg.deny).map_err(|e| refusal(&e))?;
            println!(
                "{}",
                json!({"export_id": cfg.export_id, "label": cfg.label})
            );
            exports.push(cfg);
            save_exports(state, &exports)
        }
        FsCommand::Unexport { label } => {
            let mut exports = load_exports(state)?;
            let Some(i) = exports.iter().position(|e| e.label == label) else {
                return Err(format!("zone fs: no export named {label}"));
            };
            let device = items::joined(state, "fs unexport")?;
            let srv = server(state, &device)?;
            srv.grants
                .revoke_export(srv.signer(), &exports[i].export_id)
                .map_err(|e| refusal(&e))?;
            exports.remove(i);
            save_exports(state, &exports)
        }
        FsCommand::Exports => {
            for e in load_exports(state)? {
                println!(
                    "{}",
                    json!({"export_id": e.export_id, "label": e.label, "root": e.root, "deny": e.deny})
                );
            }
            Ok(())
        }
        FsCommand::Grant {
            member,
            label,
            paths,
            ops,
            minutes,
            metadata_per_hour,
            reads_per_hour,
            searches_per_hour,
            mib_per_hour,
            burst,
        } => {
            let export = load_exports(state)?
                .into_iter()
                .find(|e| e.label == label)
                .ok_or_else(|| format!("zone fs: no export named {label}"))?;
            let device = items::joined(state, "fs grant")?;
            let subject = if member == "zone" {
                Subject::Zone
            } else {
                Subject::Members(vec![
                    resolve_member(&device, &member).map_err(|e| refusal(&e))?
                ])
            };
            let d = Limits::default();
            let limits = Limits {
                metadata_per_hour: metadata_per_hour.unwrap_or(d.metadata_per_hour),
                reads_per_hour: reads_per_hour.unwrap_or(d.reads_per_hour),
                searches_per_hour: searches_per_hour.unwrap_or(d.searches_per_hour),
                bytes_per_hour: mib_per_hour.map_or(d.bytes_per_hour, |m| m * 1024 * 1024),
                burst: burst.unwrap_or(d.burst),
            };
            let srv = server(state, &device)?;
            let (_, g) = srv
                .grants
                .issue(
                    srv.signer(),
                    &GrantSpec {
                        zone: srv.zone().to_owned(),
                        export_id: export.export_id,
                        subject,
                        paths: if paths.is_empty() {
                            vec![String::new()]
                        } else {
                            paths
                        },
                        ops,
                        ttl_ms: Some(minutes * 60 * 1000),
                        limits,
                        parent: None,
                    },
                    now_ms(),
                )
                .map_err(|e| refusal(&e))?;
            println!(
                "{}",
                json!({"grant_id": g.grant_id, "subject": g.subject.to_json(), "paths": g.paths, "ops": g.ops, "expires_at_ms": g.expires_at_ms})
            );
            Ok(())
        }
        FsCommand::Revoke { grant_id } => {
            let device = items::joined(state, "fs revoke")?;
            let srv = server(state, &device)?;
            srv.grants.revoke(&grant_id).map_err(|e| refusal(&e))
        }
        FsCommand::Grants => {
            let device = items::joined(state, "fs grants")?;
            let srv = server(state, &device)?;
            let now = now_ms();
            for (_, g) in srv.grants.all(srv.signer()) {
                let live = srv.grants.check(srv.signer(), &g, now).is_ok();
                println!(
                    "{}",
                    json!({"grant_id": g.grant_id, "export_id": g.export_id, "subject": g.subject.to_json(), "paths": g.paths, "ops": g.ops, "expires_at_ms": g.expires_at_ms, "live": live})
                );
            }
            Ok(())
        }
        FsCommand::Serve { sip, republish } => serve(state, &sip, republish),
        FsCommand::Ls { sip, json: as_json } => list_exporters(state, &sip, as_json),
        FsCommand::Target(args) => {
            let cli =
                // Help and usage errors print and exit as clap's own do.
                <TargetCli as clap::Parser>::try_parse_from(args).unwrap_or_else(|e| e.exit());
            target(state, cli)
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `zone fs serve`: publish this device's zone fs capability and answer each caller on its own
/// thread, as the member the session's signed offer names.
fn serve(state: &Path, sip: &SipArgs, republish: bool) -> Result<(), String> {
    const CMD: &str = "fs serve";
    let device = items::joined(state, CMD)?;
    let srv = Arc::new(server(state, &device)?);
    if srv.exports().is_empty() {
        return Err("zone fs serve: nothing exported; run `zone fs export LABEL DIR` first".into());
    }
    let (server_addr, domain) = edge(&device, sip)?;
    // One capability for this server, kept across restarts (Core has no unpublish); earlier
    // ones this device published are still answered. Every member may call (Core wants them
    // named, so a member admitted later needs `--republish`); each request is authorized by its
    // grant, not by the capability.
    let cap_file = fs_dir(state)?.join("capability");
    let kept = fs::read_to_string(&cap_file)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && !republish);
    let cap = match kept {
        Some(c) => c,
        None => {
            let others: Vec<String> = device
                .members()
                .into_iter()
                .filter(|m| !m.is_me)
                .map(|m| m.member_id)
                .collect();
            let c = device
                .publish_capability(FS_CAP_KIND, &others, FS_CAP_LABEL)
                .map_err(|e| items::sdk_error(CMD, &e))?;
            fs::write(&cap_file, &c)
                .map_err(|e| format!("zone {CMD}: {}: {e}", cap_file.display()))?;
            c
        }
    };
    let mut caps = vec![cap];
    caps.extend(
        device
            .capabilities()
            .into_iter()
            .filter(|c| c.mine && c.kind == FS_CAP_KIND && c.label == FS_CAP_LABEL)
            .map(|c| c.cap_id),
    );
    caps.dedup();
    let offer = device
        .clone()
        .offer_stream_caps(caps, vec![], StreamMode::Bytes, server_addr, domain)
        .map_err(|e| format!("zone {CMD}: {e}"))?;
    run::catch_signals();
    eprintln!(
        "zone {CMD}: serving {} export(s); Ctrl+C to stop",
        srv.exports().len()
    );
    while !run::stopped() {
        match offer.accept(1) {
            Ok(Some(session)) => {
                let srv = srv.clone();
                std::thread::spawn(move || {
                    let actor = Actor::member(&session.peer());
                    srv.serve(Arc::new(SessionConn(session.clone())), &actor);
                    close(&session);
                });
            }
            Ok(None) => {}
            Err(e) => eprintln!("zone {CMD}: a caller could not connect: {e}"),
        }
    }
    offer.stop();
    Ok(())
}

/// `zone fs ls`: every member running a zone fs server, asked for the exports this member may
/// reach; members with none are not shown.
fn list_exporters(state: &Path, sip: &SipArgs, as_json: bool) -> Result<(), String> {
    let device = items::joined(state, "fs ls")?;
    let caps = device
        .refresh_capabilities()
        .unwrap_or_else(|_| device.capabilities());
    let mut owners: Vec<(String, String)> = caps
        .iter()
        .filter(|c| c.kind == FS_CAP_KIND && c.label == FS_CAP_LABEL && !c.mine && c.online)
        .map(|c| (c.publisher_id.clone(), c.machine_name.clone()))
        .collect();
    owners.sort();
    owners.dedup_by(|a, b| a.0 == b.0);
    let names: std::collections::HashMap<String, String> = device
        .members()
        .into_iter()
        .map(|m| (m.member_id, m.name))
        .collect();
    for (member, _) in owners {
        let Ok((session, client)) = connect(&device, &member, sip) else {
            continue;
        };
        let exports = client.exports();
        close(&session);
        let Ok(exports) = exports else { continue };
        if exports.is_empty() {
            continue;
        }
        let name = names.get(&member).cloned().unwrap_or_default();
        if as_json {
            println!(
                "{}",
                json!({"member": member, "name": name, "exports": exports})
            );
        } else {
            let suffix = &member[member.len().saturating_sub(8)..];
            for e in exports {
                println!(
                    "{name} (…{suffix})\t{}\t{}\texpires {}",
                    e["label"].as_str().unwrap_or_default(),
                    e["ops"]
                        .as_array()
                        .map(|a| a
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(","))
                        .unwrap_or_default(),
                    e["expires_at_ms"]
                );
            }
        }
    }
    Ok(())
}

fn entry_line(e: &bp_zone_fs::policy::Entry) -> String {
    let kind = match e.kind {
        bp_zone_fs::policy::Kind::Dir => "d",
        bp_zone_fs::policy::Kind::File if e.exec => "x",
        bp_zone_fs::policy::Kind::File => "-",
    };
    format!("{kind} {:>10} {}", e.size, e.path)
}

fn target(state: &Path, cli: TargetCli) -> Result<(), String> {
    if matches!(cli.op, Op::Mount { .. }) {
        // Before any network activity.
        crate::fsmount::available().map_err(|e| format!("zone fs mount: {e}"))?;
    }
    let device = items::joined(state, "fs")?;
    let member = resolve_member(&device, &cli.agent).map_err(|e| refusal(&e))?;
    run::catch_signals();
    let t = Instant::now();
    let (session, client) = connect(&device, &member, &cli.sip)?;
    if matches!(cli.op, Op::Bench { .. }) {
        eprintln!(
            "{}",
            json!({"session_open_ms": t.elapsed().as_secs_f64() * 1e3})
        );
    }
    let result = match cli.op {
        Op::Mount { dir, root } => crate::fsmount::mount(Arc::new(client), &root, &dir),
        other => op(&client, other),
    };
    close(&session);
    result
}

fn op(client: &FsClient, op: Op) -> Result<(), String> {
    match op {
        Op::Ls {
            path,
            json: as_json,
        } => {
            for e in client.ls(&path).map_err(|e| refusal(&e))? {
                if as_json {
                    println!("{}", e.to_json());
                } else {
                    println!("{}", entry_line(&e));
                }
            }
            Ok(())
        }
        Op::Cat {
            path,
            offset,
            length,
        } => {
            let mut out = std::io::stdout().lock();
            let end = client
                .cat(&path, offset, length, &mut out)
                .map_err(|e| refusal(&e))?;
            out.flush()
                .map_err(|e| format!("zone fs: stdout: {}", e.kind()))?;
            if end.truncated {
                eprintln!(
                    "zone fs: cut at {} bytes; continue with --offset {}",
                    end.bytes,
                    offset + end.bytes
                );
            }
            Ok(())
        }
        Op::Tree {
            path,
            depth,
            json: as_json,
        } => {
            let mut cursor: Option<String> = None;
            loop {
                let (entries, end) = client
                    .tree(&path, depth, cursor.as_deref())
                    .map_err(|e| refusal(&e))?;
                for e in entries {
                    if as_json {
                        println!("{}", e.to_json());
                    } else {
                        println!("{}", entry_line(&e));
                    }
                }
                if !end.truncated || run::stopped() {
                    return Ok(());
                }
                cursor = end.cursor;
            }
        }
        Op::Stat { path } => {
            let e = client.stat(&path).map_err(|e| refusal(&e))?;
            println!("{}", e.to_json());
            Ok(())
        }
        Op::Grep {
            pattern,
            path,
            types,
            globs,
            fixed_strings,
            ignore_case,
            max_count,
            json: as_json,
        } => {
            let args = GrepArgs {
                pattern,
                fixed_strings,
                case_insensitive: ignore_case,
                types,
                globs,
            };
            let mut out = std::io::stdout().lock();
            let mut n = 0u64;
            let mut failed = false;
            let end = client.grep(&path, &args, None, &mut |m| {
                n += 1;
                let line = if as_json {
                    json!({"path": m.path, "line": m.line, "column": m.column, "text": m.text})
                        .to_string()
                } else {
                    format!("{}:{}:{}:{}", m.path, m.line, m.column, m.text)
                };
                if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                    failed = true;
                    return false;
                }
                !run::stopped() && max_count.is_none_or(|c| n < c)
            });
            match end {
                Ok(end) if end.truncated => {
                    eprintln!(
                        "zone fs: stopped at the match bound after {} matches",
                        end.items
                    );
                    Ok(())
                }
                Ok(_) => Ok(()),
                Err(e) if e.code == Code::Cancelled && (failed || max_count.is_some()) => Ok(()),
                Err(e) => Err(refusal(&e)),
            }
        }
        Op::Find {
            name,
            path,
            glob,
            ignore_case,
        } => {
            let (entries, _) = client
                .find(&path, &name, glob, ignore_case)
                .map_err(|e| refusal(&e))?;
            for e in entries {
                println!("{}", e.path);
            }
            Ok(())
        }
        Op::Bench {
            ls,
            cat,
            grep,
            grep_path,
            runs,
        } => bench(client, &ls, &cat, &grep, &grep_path, runs),
        Op::Mount { .. } => Err("zone fs mount: handled by the caller".into()),
    }
}

fn stats(ms: &[f64]) -> Value {
    let mut s = ms.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| s[((s.len() as f64 - 1.0) * p).round() as usize];
    json!({
        "n": s.len(),
        "p50_ms": pct(0.5),
        "p95_ms": pct(0.95),
        "max_ms": s[s.len() - 1],
        "samples_ms": ms.iter().map(|m| (m * 100.0).round() / 100.0).collect::<Vec<_>>(),
    })
}

fn bench(
    client: &FsClient,
    ls: &str,
    cat: &str,
    pattern: &str,
    grep_path: &str,
    runs: usize,
) -> Result<(), String> {
    let runs = runs.max(1);
    let time = |f: &mut dyn FnMut() -> Result<(), FsError>| -> Result<Vec<f64>, String> {
        let mut out = vec![];
        for _ in 0..runs {
            let t = Instant::now();
            f().map_err(|e| refusal(&e))?;
            out.push(t.elapsed().as_secs_f64() * 1e3);
        }
        Ok(out)
    };
    let ls_ms = time(&mut || client.ls(ls).map(|_| ()))?;
    let cat_ms = time(&mut || {
        let mut sink = Vec::new();
        client.cat(cat, 0, None, &mut sink).map(|_| ())
    })?;
    let args = GrepArgs {
        pattern: pattern.to_owned(),
        ..Default::default()
    };
    let (mut first, mut full) = (vec![], vec![]);
    let mut first_path = String::new();
    let mut matches = 0;
    for _ in 0..runs {
        let t = Instant::now();
        let mut f: Option<f64> = None;
        let end = client
            .grep(grep_path, &args, None, &mut |m| {
                if f.is_none() {
                    f = Some(t.elapsed().as_secs_f64() * 1e3);
                    first_path = m.path;
                }
                true
            })
            .map_err(|e| refusal(&e))?;
        full.push(t.elapsed().as_secs_f64() * 1e3);
        first.push(f.unwrap_or(f64::NAN));
        matches = end.items;
    }
    println!(
        "{}",
        json!({
            "ls": stats(&ls_ms),
            "cat": stats(&cat_ms),
            "grep_first_match": stats(&first),
            "grep_full_scan": stats(&full),
            "grep_first_path": first_path,
            "grep_matches": matches,
        })
    );
    Ok(())
}

// ── Shares ──────────────────────────────────────────────────────────────────────────────────

/// `zone share` subcommands; `zone share NAME PATH` publishes or updates one.
#[derive(clap::Subcommand)]
pub enum ShareCommand {
    /// Your live shares.
    List,
    /// Withdraw a share: a signed tombstone, announced to the members who could see it.
    Remove { name: String },
    #[command(external_subcommand)]
    Publish(Vec<String>),
}

#[derive(clap::Parser)]
#[command(name = "zone share", no_binary_name = true)]
struct ShareCli {
    /// The alias readers use (`snippets/turn-retry`).
    name: String,
    /// A file or directory inside one of your exports (a local path, or `LABEL/PATH`).
    path: String,
    /// Only these members (repeatable; a name or pseudonym). Default: every member.
    #[arg(long = "to", value_name = "MEMBER")]
    to: Vec<String>,
}

/// Tell the members a share change is for, with a sealed note only they can open (the zone event
/// stream will carry the same signed event once it exists).
fn announce(device: &ZoneDevice, p: &bp_zone_fs::share::Published) {
    let Some(event) = &p.event else { return };
    let roster: Vec<Actor> = device
        .members()
        .into_iter()
        .filter(|m| !m.is_me)
        .map(|m| Actor::member(&m.member_id))
        .collect();
    let text = bp_zone_fs::share_event::note_text(event);
    let sent = match bp_zone_fs::share_event::audience(&p.share.visibility, &roster) {
        None => device.add_note(text).map(|_| ()),
        Some(to) if to.is_empty() => Ok(()),
        Some(to) => to
            .chunks(items::MAX_RECIPIENTS)
            .try_for_each(|c| device.add_note_to(c.to_vec(), text.clone()).map(|_| ())),
    };
    if let Err(e) = sent {
        eprintln!("{}", items::sdk_error("share: announcing", &e));
    }
}

pub fn share_command(state: &Path, command: ShareCommand) -> Result<(), String> {
    let device = items::joined(state, "share")?;
    let srv = server(state, &device)?;
    match command {
        ShareCommand::List => {
            for (_, s) in srv.shares.live(srv.signer()) {
                println!(
                    "{}",
                    json!({"name": s.name, "version": s.version, "target": s.target, "visibility": s.visibility.to_json(), "digest": s.digest})
                );
            }
            Ok(())
        }
        ShareCommand::Remove { name } => {
            let p = srv.unshare(&name).map_err(|e| refusal(&e))?;
            announce(&device, &p);
            println!(
                "{}",
                json!({"name": p.share.name, "version": p.share.version, "removed": true})
            );
            Ok(())
        }
        ShareCommand::Publish(args) => {
            let cli = <ShareCli as clap::Parser>::try_parse_from(args).unwrap_or_else(|e| e.exit());
            let path = match srv.export_path(Path::new(&cli.path)) {
                Some(p) => p,
                None if !Path::new(&cli.path).exists() => cli.path.clone(),
                None => {
                    return Err(format!(
                        "zone share: {} is not inside one of your exports",
                        cli.path
                    ))
                }
            };
            let visibility = if cli.to.is_empty() {
                Subject::Zone
            } else {
                Subject::Members(
                    cli.to
                        .iter()
                        .map(|m| resolve_member(&device, m))
                        .collect::<Result<_, _>>()
                        .map_err(|e| refusal(&e))?,
                )
            };
            let p = srv
                .share(&cli.name, &path, visibility)
                .map_err(|e| refusal(&e))?;
            announce(&device, &p);
            println!(
                "{}",
                json!({"name": p.share.name, "version": p.share.version, "target": p.share.target, "digest": p.share.digest})
            );
            Ok(())
        }
    }
}

// ── MCP ─────────────────────────────────────────────────────────────────────────────────────

/// Sessions the MCP tools keep open, one per exporting member, so a tool call after the first
/// costs a round trip and not a session setup.
static SESSIONS: std::sync::Mutex<Vec<Kept>> = std::sync::Mutex::new(Vec::new());
/// An open session: the member, the stream and the client over it.
type Kept = (String, Arc<StreamSession>, Arc<FsClient>);
/// The most file bytes one `zone_fs_read` returns.
const MCP_READ_MAX: u64 = 1024 * 1024;

/// `zone_fs_list`, `zone_fs_read`, `zone_fs_search` and `zone_fs_stat`: the same SDK requests
/// as the CLI, with the same bounds and error codes. No recursive fetch-all, no model search.
pub fn mcp_tools() -> Vec<Value> {
    let agent = json!({"type": "string", "description": "The exporting member: a pseudonym, or a name only one member has"});
    let path = json!({"type": "string", "description": "A path: export label first (`core/src/lib.rs`); empty lists the exports"});
    let tool = |name: &str, description: &str, props: Value, required: Value| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {"type": "object", "properties": props, "required": required},
            "annotations": {"readOnlyHint": true, "openWorldHint": true},
        })
    };
    vec![
        tool(
            "zone_fs_list",
            "List a directory another member exports to you (zone fs ls), run on their host.",
            json!({"agent": agent, "path": path}),
            json!(["agent"]),
        ),
        tool(
            "zone_fs_stat",
            "One exported path's metadata (zone fs stat).",
            json!({"agent": agent, "path": path}),
            json!(["agent", "path"]),
        ),
        tool(
            "zone_fs_read",
            "Read an exported file (zone fs cat), at most 1 MiB per call; resume with offset.",
            json!({"agent": agent, "path": path, "offset": {"type": "integer"}, "length": {"type": "integer"}}),
            json!(["agent", "path"]),
        ),
        tool(
            "zone_fs_search",
            "Search an export on its owner's host: `pattern` greps lines (ripgrep rules, `type`, `glob`), or `name` finds paths.",
            json!({
                "agent": agent,
                "path": path,
                "pattern": {"type": "string"},
                "name": {"type": "string"},
                "type": {"type": "array", "items": {"type": "string"}},
                "glob": {"type": "array", "items": {"type": "string"}},
                "ignore_case": {"type": "boolean"},
                "max_matches": {"type": "integer"},
            }),
            json!(["agent"]),
        ),
    ]
}

fn mcp_client(state: &Path, agent: &str) -> Result<Arc<FsClient>, String> {
    let device = items::joined(state, "fs")?;
    let member = resolve_member(&device, agent).map_err(|e| refusal(&e))?;
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    sessions.retain(|(_, s, _)| s.is_open());
    if let Some((_, _, c)) = sessions.iter().find(|(m, _, _)| *m == member) {
        return Ok(c.clone());
    }
    let (session, client) = connect(&device, &member, &SipArgs::from_env())?;
    let client = Arc::new(client);
    sessions.push((member, session, client.clone()));
    Ok(client)
}

/// A zone fs tool call's MCP result, or None when `params` names another tool.
pub fn mcp_call(state: &Path, params: &Value) -> Option<Value> {
    let name = params["name"].as_str()?;
    if !name.starts_with("zone_fs_") {
        return None;
    }
    let a = &params["arguments"];
    let result = (|| -> Result<Value, String> {
        let agent = a["agent"].as_str().ok_or("agent is required")?;
        let path = a["path"].as_str().unwrap_or_default();
        let client = mcp_client(state, agent)?;
        let fail = |e: FsError| refusal(&e);
        match name {
            "zone_fs_list" => {
                let entries = client.ls(path).map_err(fail)?;
                Ok(json!({"entries": entries.iter().map(|e| e.to_json()).collect::<Vec<_>>()}))
            }
            "zone_fs_stat" => Ok(client.stat(path).map_err(fail)?.to_json()),
            "zone_fs_read" => {
                let offset = a["offset"].as_u64().unwrap_or(0);
                let length = a["length"]
                    .as_u64()
                    .map_or(MCP_READ_MAX, |l| l.min(MCP_READ_MAX));
                let mut buf = vec![];
                let end = client
                    .cat(path, offset, Some(length), &mut buf)
                    .map_err(fail)?;
                let (text, encoding) = match String::from_utf8(buf) {
                    Ok(t) => (t, "utf-8"),
                    Err(e) => (bp_workspace_core::b64::encode(e.as_bytes()), "base64url"),
                };
                Ok(
                    json!({"text": text, "encoding": encoding, "offset": offset, "bytes": end.bytes, "more": end.truncated || end.bytes == length}),
                )
            }
            "zone_fs_search" => {
                let max = a["max_matches"].as_u64().unwrap_or(200).min(1000);
                let strings = |k: &str| -> Vec<String> {
                    a[k].as_array()
                        .map(|v| {
                            v.iter()
                                .filter_map(|x| x.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let ci = a["ignore_case"].as_bool().unwrap_or(false);
                if let Some(pattern) = a["pattern"].as_str() {
                    let args = GrepArgs {
                        pattern: pattern.to_owned(),
                        fixed_strings: false,
                        case_insensitive: ci,
                        types: strings("type"),
                        globs: strings("glob"),
                    };
                    let mut matches = vec![];
                    let end = client.grep(path, &args, None, &mut |m| {
                        matches.push(json!({"path": m.path, "line": m.line, "column": m.column, "text": m.text}));
                        (matches.len() as u64) < max
                    });
                    match end {
                        Ok(_) => {}
                        Err(e) if e.code == Code::Cancelled => {}
                        Err(e) => return Err(refusal(&e)),
                    }
                    Ok(json!({"matches": matches}))
                } else if let Some(n) = a["name"].as_str() {
                    let (entries, _) = client.find(path, n, false, ci).map_err(fail)?;
                    Ok(
                        json!({"paths": entries.iter().take(max as usize).map(|e| e.path.clone()).collect::<Vec<_>>()}),
                    )
                } else {
                    Err("give a pattern or a name".into())
                }
            }
            _ => Err(format!("unknown tool {name}")),
        }
    })();
    Some(match result {
        Ok(v) => {
            json!({"content": [{"type": "text", "text": v.to_string()}], "structuredContent": v, "isError": false})
        }
        Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn target_first_commands_parse() {
        let c = TargetCli::try_parse_from(["aly", "ls", "core/src"]).unwrap();
        assert_eq!(c.agent, "aly");
        assert!(matches!(c.op, Op::Ls { ref path, .. } if path == "core/src"));
        let c = TargetCli::try_parse_from([
            "bp:self:ab",
            "grep",
            "-t",
            "rust",
            "-g",
            "!target/**",
            "-i",
            "fn main",
            "core",
        ])
        .unwrap();
        match c.op {
            Op::Grep {
                pattern,
                path,
                types,
                globs,
                ignore_case,
                ..
            } => {
                assert_eq!((pattern.as_str(), path.as_str()), ("fn main", "core"));
                assert_eq!(types, ["rust"]);
                assert_eq!(globs, ["!target/**"]);
                assert!(ignore_case);
            }
            _ => panic!("not grep"),
        }
        assert!(
            TargetCli::try_parse_from(["aly", "rm", "x"]).is_err(),
            "no write op"
        );
    }

    #[test]
    fn mcp_tools_are_the_four_read_only_ones() {
        let names: Vec<String> = mcp_tools()
            .iter()
            .map(|t| {
                assert_eq!(t["annotations"]["readOnlyHint"], true);
                t["name"].as_str().unwrap().to_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "zone_fs_list",
                "zone_fs_stat",
                "zone_fs_read",
                "zone_fs_search"
            ]
        );
        assert!(mcp_call(Path::new("/nonexistent"), &json!({"name": "zone_who"})).is_none());
        let r = mcp_call(
            Path::new("/nonexistent"),
            &json!({"name": "zone_fs_list", "arguments": {}}),
        )
        .unwrap();
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn refusals_lead_with_the_stable_code() {
        let mut e = FsError::new(Code::Rate, "rate limit reached");
        e.retry_after_s = Some(30);
        assert_eq!(
            refusal(&e),
            "zone fs: fs-rate: rate limit reached (retry in 30 s)"
        );
    }
}
