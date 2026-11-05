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
use bp_zone_fs::coding::{self, PatchInbox};
use bp_zone_fs::grant::{GrantSpec, GrantStore, Limits, Subject};
use bp_zone_fs::patch::{self, ClaimedCheck, PatchSpec, SessionRef};
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
        /// Operations: metadata, read, grep, find, symbols, semantic, mount, patch
        /// (comma-separated).
        /// `patch` lets the member send signed patches to your inbox (`zone fs inbox`).
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
    /// Patches coding sessions sent you: list, show, review in a worktree with your checks,
    /// accept (a signed receipt and one payment reference) or reject.
    Inbox {
        #[command(subcommand)]
        command: InboxCommand,
    },
    /// `zone fs <agent> <op> ...`.
    #[command(external_subcommand)]
    Target(Vec<String>),
}

/// `zone fs inbox` subcommands. Nothing here applies a patch to your tree: review happens in a
/// detached worktree, and merging an accepted commit is yours to do.
#[derive(clap::Subcommand)]
pub enum InboxCommand {
    /// Every patch filed, oldest first, as JSON lines (no patch text).
    List,
    /// One patch's summary; `--diff` writes the patch itself to stdout.
    Show {
        patch_id: String,
        #[arg(long)]
        diff: bool,
    },
    /// Apply a patch in a fresh worktree at its base and run your checks there.
    Review {
        patch_id: String,
        /// `NAME=COMMAND ARGS...`, run in the worktree without a shell (repeatable; at least one).
        #[arg(long = "check", value_name = "NAME=COMMAND", required = true)]
        checks: Vec<String>,
        /// Seconds each check may run.
        #[arg(long, default_value_t = 900)]
        timeout: u64,
    },
    /// Accept a reviewed patch whose checks all passed: a signed accepted-patch receipt and a
    /// payment reference, sent to its author. Accepting again returns the same receipt.
    Accept { patch_id: String },
    /// Reject a patch with a stable reason, sent to its author with a signed receipt.
    Reject {
        patch_id: String,
        /// owner-rejected, checks-failed, apply-failed, base-missing, denied-path...
        #[arg(long, default_value = "owner-rejected")]
        reason: String,
    },
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
        /// Stop after this many matches.
        #[arg(long, short = 'm')]
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
    /// Definitions named NAME (functions, types...), from the owner's tree-sitter index.
    Symbols {
        name: String,
        #[arg(default_value = "")]
        path: String,
        /// function, method, class, interface, module...
        #[arg(long)]
        kind: Option<String>,
        /// The whole name, not a part of it.
        #[arg(long)]
        exact: bool,
        #[arg(long, short = 'i')]
        ignore_case: bool,
    },
    /// Windows nearest to QUERY by meaning, from the owner's local embedding model (when the
    /// owner runs one; otherwise `fs-index-unavailable`). No LLM, nothing leaves their host but
    /// the ranked paths, lines and short snippets your grant allows.
    Semantic {
        query: String,
        #[arg(default_value = "")]
        path: String,
        /// How many results (at most 100).
        #[arg(long, default_value_t = 10)]
        top: u64,
        #[arg(long)]
        json: bool,
    },
    /// Send a patch to the agent's inbox for export LABEL, signed by this device. Needs a live
    /// grant naming `patch`. The owner reviews it with its own checks; nothing is applied here.
    Patch {
        label: String,
        /// The patch file (`git diff` output), or `-` for stdin.
        file: PathBuf,
        /// The full commit id the patch is against.
        #[arg(long)]
        base: String,
        /// A check you ran, `NAME=pass|fail|skip` (repeatable). Recorded, never trusted.
        #[arg(long = "check", value_name = "NAME=RESULT")]
        checks: Vec<String>,
        /// The PR carrying the same change, if any (plain https URL).
        #[arg(long)]
        pr: Option<String>,
    },
    /// Ask the agent a question about export LABEL while you work on it: a note to that member
    /// only, marked blocked/question. QUESTION may be `-` for stdin.
    Ask {
        label: String,
        question: String,
        #[arg(long)]
        base: Option<String>,
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
    let message = clean(&e.message);
    match e.retry_after_s {
        Some(s) => format!("zone fs: {}: {message} (retry in {s} s)", e.code.as_str()),
        None => format!("zone fs: {}: {message}", e.code.as_str()),
    }
}

/// Text the other member chose (names, match lines, messages), with control characters shown
/// as `\xNN` so it cannot drive the terminal it is printed to.
fn clean(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(char::is_control) {
        return std::borrow::Cow::Borrowed(s);
    }
    std::borrow::Cow::Owned(
        s.chars()
            .map(|c| {
                if c.is_control() {
                    format!("\\x{:02x}", c as u32)
                } else {
                    c.to_string()
                }
            })
            .collect(),
    )
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
/// (never a guess between several).
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
    .with_shares(shares)
    .with_patches(Arc::new(
        PatchInbox::open(&dir.join("patches")).map_err(|e| format!("zone fs: {e}"))?,
    ));
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
    let pick = |caps: Vec<bp_zone_core::CapabilityInfo>| {
        caps.into_iter()
            .filter(|c| {
                c.kind == FS_CAP_KIND
                    && c.label == FS_CAP_LABEL
                    && c.publisher_id == member
                    && !c.mine
            })
            .max_by_key(|c| (c.online, c.last_seen_ms))
    };
    // The command's own sync just read the zone's capabilities: ask Core again only when they
    // name no server for `member` (one presence read less on every session open).
    let cap = pick(device.capabilities())
        .or_else(|| {
            pick(
                device
                    .refresh_capabilities()
                    .unwrap_or_else(|_| device.capabilities()),
            )
        })
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
        .map_err(|e| format!("zone fs: fs-transport: {e}"))?;
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
            // Its shares go too, each with a tombstone announced to its members: a share is
            // its own authority and would outlive the export otherwise.
            for (_, s) in srv.shares.live(srv.signer()) {
                if s.export_id == exports[i].export_id {
                    let p = srv.unshare(&s.name).map_err(|e| refusal(&e))?;
                    announce(&device, &p);
                }
            }
            exports.remove(i);
            // A running `zone fs serve` sees the file change and stops serving it.
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
            if !srv
                .grants
                .all(srv.signer())
                .iter()
                .any(|(_, g)| g.grant_id == grant_id)
            {
                return Err(refusal(&FsError::new(
                    Code::Grant,
                    format!("no grant {grant_id}; `zone fs grants` lists yours"),
                )));
            }
            srv.grants.revoke(&grant_id).map_err(|e| refusal(&e))?;
            println!("{}", json!({"grant_id": grant_id, "revoked": true}));
            Ok(())
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
        FsCommand::Inbox { command } => inbox(state, command),
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
    let exports_file = fs_dir(state)?.join("exports.json");
    let mut stamp = fs::metadata(&exports_file).and_then(|m| m.modified()).ok();
    while !run::stopped() {
        // `zone fs export` and `unexport` in another process take effect without a restart.
        let now = fs::metadata(&exports_file).and_then(|m| m.modified()).ok();
        if now != stamp {
            stamp = now;
            let want = load_exports(state)?;
            for e in srv.exports() {
                if !want.iter().any(|w| w.export_id == e.cfg.export_id) {
                    let _ = srv.disable_export(&e.cfg.export_id);
                }
            }
            for w in want {
                if !srv.exports().iter().any(|e| e.cfg.export_id == w.export_id) {
                    let label = w.label.clone();
                    if let Err(e) = srv.add_export(w) {
                        eprintln!("zone {CMD}: export {label} skipped: {}", refusal(&e));
                    }
                }
            }
        }
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
                    "{} (…{suffix})\t{}\t{}\texpires {}",
                    clean(&name),
                    clean(e["label"].as_str().unwrap_or_default()),
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
    format!("{kind} {:>10} {}", e.size, clean(&e.path))
}

fn target(state: &Path, cli: TargetCli) -> Result<(), String> {
    if matches!(cli.op, Op::Mount { .. }) {
        // Before any network activity.
        crate::fsmount::available().map_err(|e| format!("zone fs mount: {e}"))?;
    }
    let device = items::joined(state, "fs")?;
    let member = resolve_member(&device, &cli.agent).map_err(|e| refusal(&e))?;
    // Ctrl+C ends a stream cleanly; a one-shot request is simply interrupted.
    if matches!(cli.op, Op::Grep { .. } | Op::Tree { .. } | Op::Bench { .. }) {
        run::catch_signals();
    }
    let t = Instant::now();
    let (session, client) = connect(&device, &member, &cli.sip)?;
    if matches!(cli.op, Op::Bench { .. }) {
        eprintln!(
            "{}",
            json!({"session_open_ms": t.elapsed().as_secs_f64() * 1e3})
        );
    }
    let result = match cli.op {
        Op::Patch {
            label,
            file,
            base,
            checks,
            pr,
        } => submit_patch(&device, &client, &label, &file, base, &checks, pr),
        Op::Ask {
            label,
            question,
            base,
        } => ask(
            &device,
            &client,
            &member,
            &label,
            &question,
            base.as_deref(),
        ),
        Op::Mount { dir, root } => crate::fsmount::mount(Arc::new(client), &root, &dir)
            .map_err(|e| format!("zone fs mount: {e}")),
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
                // An owner that hands back no cursor, or the same one, would page forever.
                if end.cursor.is_none() || end.cursor == cursor {
                    return Err(refusal(&FsError::new(
                        Code::Transport,
                        "the owner's listing does not advance",
                    )));
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
                    format!(
                        "{}:{}:{}:{}",
                        clean(&m.path),
                        m.line,
                        m.column,
                        clean(&m.text)
                    )
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
                println!("{}", clean(&e.path));
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
        Op::Symbols {
            name,
            path,
            kind,
            exact,
            ignore_case,
        } => {
            let a = bp_zone_fs::symbols::SymbolArgs {
                name,
                kind,
                exact,
                case_insensitive: ignore_case,
            };
            let (symbols, end) = client.symbols(&path, &a, None).map_err(|e| refusal(&e))?;
            for s in symbols {
                println!(
                    "{}:{}:{}: {} {}",
                    clean(&s.path),
                    s.line,
                    s.column,
                    clean(&s.kind),
                    clean(&s.name)
                );
            }
            if end.truncated {
                eprintln!(
                    "zone fs: stopped at the result bound after {} symbols",
                    end.items
                );
            }
            Ok(())
        }
        Op::Semantic {
            query,
            path,
            top,
            json: as_json,
        } => {
            let a = bp_zone_fs::semantic::SemanticArgs { query, top_k: top };
            let (hits, _) = client.semantic(&path, &a).map_err(|e| refusal(&e))?;
            for h in hits {
                if as_json {
                    println!("{}", h.to_json());
                } else {
                    println!(
                        "{}:{}-{}: {:.3} {}",
                        clean(&h.path),
                        h.start_line,
                        h.end_line,
                        h.score,
                        clean(h.snippet.lines().next().unwrap_or_default())
                    );
                }
            }
            Ok(())
        }
        Op::Mount { .. } | Op::Patch { .. } | Op::Ask { .. } => {
            Err("zone fs: handled by the caller".into())
        }
    }
}

// ── Coding sessions ──────────────────────────────────────────────────────────────────────────

/// stdin for `-`, else the file; at most `max` bytes.
fn read_input(path: &Path, max: usize, what: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = vec![];
    let r: Box<dyn Read> = if path == Path::new("-") {
        Box::new(std::io::stdin())
    } else {
        Box::new(
            fs::File::open(path)
                .map_err(|e| format!("zone fs: {what}: {}: {e}", path.display()))?,
        )
    };
    r.take(max as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("zone fs: {what}: {e}"))?;
    if buf.len() > max {
        return Err(format!("zone fs: fs-size: the {what} is over {max} bytes"));
    }
    Ok(buf)
}

fn submit_patch(
    device: &Arc<ZoneDevice>,
    client: &FsClient,
    label: &str,
    file: &Path,
    base: String,
    checks: &[String],
    pr: Option<String>,
) -> Result<(), String> {
    let bytes = read_input(file, patch::MAX_PATCH_BYTES, "patch")?;
    let checks_claimed = checks
        .iter()
        .map(|c| {
            let (name, result) = c
                .rsplit_once('=')
                .ok_or_else(|| format!("zone fs patch: --check {c:?}: want NAME=pass|fail|skip"))?;
            Ok(ClaimedCheck {
                name: name.to_owned(),
                result: result.to_owned(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let session = client.coding_session(label).map_err(|e| refusal(&e))?;
    let signer = DeviceSigner::new(device.clone())?;
    let (envelope, env) = patch::sign_envelope(
        &signer,
        &PatchSpec {
            zone: device.status().zone_id.unwrap_or_default(),
            export_id: session.export_id.clone(),
            base_commit: base,
            checks_claimed,
            pr_url: pr,
            session: Some(SessionRef {
                grant_id: session.grant_id.clone(),
                requester: signer.member(),
            }),
            ttl_ms: 0,
        },
        &bytes,
        now_ms(),
    )
    .map_err(|e| format!("zone fs patch: {e}"))?;
    let (v, _) = client
        .submit_patch(label, &envelope, &bytes)
        .map_err(|e| refusal(&e))?;
    println!(
        "{}",
        json!({"patch_id": v["patch_id"], "patch_digest": env.patch_digest, "files": env.files.len(), "status": v["status"]})
    );
    Ok(())
}

fn ask(
    device: &Arc<ZoneDevice>,
    client: &FsClient,
    member: &str,
    label: &str,
    question: &str,
    base: Option<&str>,
) -> Result<(), String> {
    let text = if question == "-" {
        String::from_utf8(read_input(Path::new("-"), 32 * 1024, "question")?)
            .map_err(|_| "zone fs ask: the question is not UTF-8")?
    } else {
        question.to_owned()
    };
    let session = client.coding_session(label).map_err(|e| refusal(&e))?;
    let note = coding::question_note(&session.export_id, &session.grant_id, base, &text)
        .map_err(|e| refusal(&e))?;
    let id = device
        .add_note_to(vec![member.to_owned()], note)
        .map_err(|e| items::sdk_error("fs ask", &e))?;
    println!(
        "{}",
        json!({"note": id.id, "to": member, "state": coding::BLOCKED_STATE, "focus": coding::QUESTION_FOCUS})
    );
    Ok(())
}

/// The repository behind the export a filed patch names.
fn patch_repo(state: &Path, export_id: &str) -> Result<PathBuf, String> {
    let export = load_exports(state)?
        .into_iter()
        .find(|e| e.export_id == export_id)
        .ok_or("zone fs inbox: that patch's export is no longer exported")?;
    coding::repo_of(&export.root)
        .map(|(top, _)| top)
        .ok_or_else(|| "zone fs inbox: that export is not in a git work tree".into())
}

/// Tell a patch's author how it ended: the signed receipt, sealed to that member alone.
fn tell_author(device: &ZoneDevice, author: &str, body: Value) {
    if let Err(e) = device.add_note_to(vec![author.to_owned()], body.to_string()) {
        eprintln!("{}", items::sdk_error("fs inbox: telling the author", &e));
    }
}

fn inbox(state: &Path, command: InboxCommand) -> Result<(), String> {
    let device = items::joined(state, "fs inbox")?;
    let srv = server(state, &device)?;
    let inbox = srv.patches().ok_or("zone fs inbox: no inbox")?;
    let signer = srv.signer();
    let perr = |e: patch::PatchError| format!("zone fs inbox: {e}");
    match command {
        InboxCommand::List => {
            for f in inbox.list(signer) {
                println!("{}", f.summary());
            }
            Ok(())
        }
        InboxCommand::Show { patch_id, diff } => {
            let (f, bytes) = inbox.get(signer, &patch_id).map_err(perr)?;
            if diff {
                let mut out = std::io::stdout().lock();
                out.write_all(&bytes)
                    .and_then(|()| out.flush())
                    .map_err(|e| format!("zone fs: stdout: {}", e.kind()))?;
            } else {
                println!("{}", f.summary());
            }
            Ok(())
        }
        InboxCommand::Review {
            patch_id,
            checks,
            timeout,
        } => {
            let checks = checks
                .iter()
                .map(|c| {
                    let (name, cmd) = c.split_once('=').ok_or_else(|| {
                        format!("zone fs inbox: --check {c:?}: want NAME=COMMAND")
                    })?;
                    let argv: Vec<String> = cmd.split_whitespace().map(str::to_owned).collect();
                    if name.trim().is_empty() || argv.is_empty() {
                        return Err(format!("zone fs inbox: --check {c:?}: want NAME=COMMAND"));
                    }
                    Ok((name.trim().to_owned(), argv))
                })
                .collect::<Result<Vec<_>, String>>()?;
            let (f, _) = inbox.get(signer, &patch_id).map_err(perr)?;
            let repo = patch_repo(state, &f.envelope.export_id)?;
            let (applied, ran) = inbox
                .review(
                    signer,
                    &patch_id,
                    &repo,
                    &checks,
                    std::time::Duration::from_secs(timeout),
                    now_ms(),
                )
                .map_err(perr)?;
            let passed = ran.iter().all(|c| c.result == "pass");
            println!(
                "{}",
                json!({
                    "patch_id": patch_id,
                    "commit": applied.commit,
                    "worktree": applied.workdir,
                    "checks": ran.iter().map(|c| json!({"name": c.name, "result": c.result, "duration_ms": c.duration_ms})).collect::<Vec<_>>(),
                    "passed": passed,
                })
            );
            Ok(())
        }
        InboxCommand::Accept { patch_id } => {
            let (f, _) = inbox.get(signer, &patch_id).map_err(perr)?;
            let a = inbox.accept(signer, &patch_id, now_ms()).map_err(perr)?;
            let body = json!({"type": "bp-zone-fs-patch-outcome-v1", "patch_id": patch_id, "outcome": "accepted", "payment_ref": a.payment_ref, "receipt": a.receipt});
            if a.first {
                tell_author(&device, &f.envelope.author, body.clone());
            }
            println!(
                "{}",
                json!({"patch_id": patch_id, "payment_ref": a.payment_ref, "first": a.first, "receipt": a.receipt})
            );
            Ok(())
        }
        InboxCommand::Reject { patch_id, reason } => {
            let reason = patch::Reason::parse(&reason)
                .ok_or_else(|| format!("zone fs inbox: unknown reason {reason:?}"))?;
            let (f, _) = inbox.get(signer, &patch_id).map_err(perr)?;
            let repo = patch_repo(state, &f.envelope.export_id).ok();
            let receipt = inbox
                .reject(signer, &patch_id, reason, repo.as_deref(), now_ms())
                .map_err(perr)?;
            tell_author(
                &device,
                &f.envelope.author,
                json!({"type": "bp-zone-fs-patch-outcome-v1", "patch_id": patch_id, "outcome": "rejected", "reason": reason.as_str(), "receipt": receipt}),
            );
            println!(
                "{}",
                json!({"patch_id": patch_id, "reason": reason.as_str(), "receipt": receipt})
            );
            Ok(())
        }
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
/// An open session: the agent as the tool call named it, the stream and the client over it.
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
    {
        // A hit costs no sync and no roster read: the key is what the agent was called.
        let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
        sessions.retain(|(_, s, _)| s.is_open());
        if let Some((_, _, c)) = sessions.iter().find(|(a, _, _)| a == agent) {
            return Ok(c.clone());
        }
    }
    let device = items::joined(state, "fs")?;
    let member = resolve_member(&device, agent).map_err(|e| refusal(&e))?;
    let (session, client) = connect(&device, &member, &SipArgs::from_env())?;
    let client = Arc::new(client);
    SESSIONS.lock().unwrap_or_else(|p| p.into_inner()).push((
        agent.to_owned(),
        session,
        client.clone(),
    ));
    Ok(client)
}

/// Drop `agent`'s session after a transport error: its frame reader may be unusable.
fn mcp_forget(agent: &str) {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    sessions.retain(|(a, s, _)| {
        if a == agent {
            close(s);
        }
        a != agent
    });
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
        let fail = |e: FsError| {
            if e.code == Code::Transport {
                mcp_forget(agent);
            }
            refusal(&e)
        };
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
                        Err(e) => return Err(fail(e)),
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
    fn coding_session_commands_parse() {
        let base = "a".repeat(40);
        let c = TargetCli::try_parse_from([
            "aly",
            "patch",
            "core",
            "fix.diff",
            "--base",
            &base,
            "--check",
            "cargo test=pass",
            "--pr",
            "https://github.com/acme/app/pull/7",
        ])
        .unwrap();
        match c.op {
            Op::Patch {
                label,
                file,
                base: b,
                checks,
                pr,
            } => {
                assert_eq!(label, "core");
                assert_eq!(file, PathBuf::from("fix.diff"));
                assert_eq!(b, base);
                assert_eq!(checks, ["cargo test=pass"]);
                assert!(pr.is_some());
            }
            _ => panic!("not patch"),
        }
        assert!(
            TargetCli::try_parse_from(["aly", "patch", "core", "fix.diff"]).is_err(),
            "a patch names its base"
        );
        let c = TargetCli::try_parse_from([
            "aly",
            "semantic",
            "retry with backoff",
            "core",
            "--top",
            "3",
        ])
        .unwrap();
        assert!(matches!(c.op, Op::Semantic { ref path, top: 3, .. } if path == "core"));
        let c = TargetCli::try_parse_from(["aly", "ask", "core", "which retry policy?"]).unwrap();
        assert!(matches!(c.op, Op::Ask { ref question, .. } if question == "which retry policy?"));

        #[derive(clap::Parser)]
        struct Top {
            #[command(subcommand)]
            fs: FsCommand,
        }
        let t = Top::try_parse_from([
            "zone",
            "inbox",
            "review",
            "zfp_abc",
            "--check",
            "test=cargo test --locked",
        ])
        .unwrap();
        assert!(matches!(
            t.fs,
            FsCommand::Inbox { command: InboxCommand::Review { ref checks, .. } } if checks == &["test=cargo test --locked"]
        ));
        assert!(
            Top::try_parse_from(["zone", "inbox", "review", "zfp_abc"]).is_err(),
            "review needs an owner check"
        );
        assert!(Top::try_parse_from(["zone", "inbox", "accept", "zfp_abc"]).is_ok());
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
