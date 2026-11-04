//! `zone`: the command-line client for Black Path Zones. Every Zone and Core operation goes
//! through the zone-sdk crates; this binary only parses arguments and prints results.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

mod backoff;
mod device;
mod items;
mod join;
mod mcp;
mod receipts;
mod record;
mod run;
mod secret;
mod state;
mod transport;
mod vault;

/// `zone join`: the SDK or Core refused; stderr names the reason kind.
const EXIT_REFUSED: u8 = 3;
/// `zone join --timeout`: not admitted in time. `zone join` again resumes the wait.
const EXIT_TIMED_OUT: u8 = 4;
/// `zone receipts verify`: a receipt or a chain did not verify; stderr names each.
const EXIT_UNVERIFIED: u8 = 5;
/// The Core `zone join` asks for a join address's zone code; the zone code names the Core after.
const DEFAULT_CORE: &str = "https://zone.blackpathlabs.com";

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (zone-sdk ",
    env!("ZONE_SDK_REV"),
    ")"
);

#[derive(Parser)]
#[command(name = "zone", version = VERSION, about = "Command-line client for Black Path Zones")]
struct Cli {
    /// State dir (default `$XDG_DATA_HOME/zone`). Mode 0700; files in it 0600.
    #[arg(long, global = true, value_name = "DIR")]
    state: Option<PathBuf>,
    /// Read the kit key from FILE (mode 0600), or from stdin with `-`. Never from argv or env.
    #[arg(long, global = true, value_name = "FILE")]
    key_file: Option<PathBuf>,
    /// Time each step and each Core exchange on stderr (the route only: no id, header or body).
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Join a zone with an invite link, a zone code or a join address, and wait until admitted.
    /// Pass an invite on stdin (`zone join -`) or in a file (`--invite-file`): an argument shows
    /// in shell history and process listings. A join address is not secret.
    Join {
        /// The invite link (`bpzi1.…`, or the web app's `…#join=bpzi1.…`), zone code (`bpzc1.…`)
        /// or join address (`za_…`), or `-` to read it from stdin.
        #[arg(
            required_unless_present = "invite_file",
            conflicts_with = "invite_file"
        )]
        invite: Option<String>,
        /// Read the invite from FILE (mode 0600).
        #[arg(long, value_name = "FILE")]
        invite_file: Option<PathBuf>,
        /// The Core that resolves a join address (`za_…`).
        #[arg(long, value_name = "URL", default_value = DEFAULT_CORE)]
        core: String,
        /// Ask the zone's admitter to admit this device, signed with the kit key (`--key-file`).
        #[arg(long)]
        kit: bool,
        /// Return once joined (and, with `--kit`, asked) instead of waiting for admission.
        #[arg(long)]
        no_wait: bool,
        /// Stop waiting after SECS (exit 4).
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
    },
    /// Print this device's zone state as JSON: zone, pseudonym, admitted or pending, members,
    /// last and next sync, last failure kind. Reads local state only.
    Status,
    /// Keep the joined zone synced until SIGINT or SIGTERM.
    Run {
        /// Stop after N syncs.
        #[arg(long, value_name = "N")]
        rounds: Option<u32>,
    },
    /// Work with zone receipts.
    Receipts {
        #[command(subcommand)]
        command: ReceiptsCommand,
    },
    /// Show this device's identity: fingerprints and pseudonym, never key material.
    Whoami,
    /// Post a note to the zone, sealed to its members: TEXT, or `-` to read stdin (at most 32 KiB).
    Note {
        /// The note's text, or `-` to read it from stdin.
        text: String,
    },
    /// Sync once and print the zone's notes as JSON lines, oldest first.
    Notes {
        /// Only notes newer than TS (ms since the epoch, a `ts_ms` printed before).
        #[arg(long, value_name = "TS")]
        since: Option<i64>,
    },
    /// Send a file to the zone, sealed to its members (at most 32 MiB).
    SendFile {
        path: PathBuf,
        /// The name members see (default: the path's file name).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// The file's media type (default `application/octet-stream`).
        #[arg(long, value_name = "TYPE")]
        mime: Option<String>,
    },
    /// Sync once and print the zone's files as JSON lines, oldest first.
    Files {
        /// Only files newer than TS (ms since the epoch, a `ts_ms` printed before).
        #[arg(long, value_name = "TS")]
        since: Option<i64>,
    },
    /// Save a complete file from the zone, checked against its sender's signed SHA-256.
    Get {
        /// The file's id, from `zone files`.
        id: String,
        /// Where to write it, or `-` for stdout.
        #[arg(short, long, value_name = "PATH")]
        output: PathBuf,
        /// Replace an existing file at PATH.
        #[arg(long)]
        force: bool,
    },
    /// Serve MCP on stdio, bridged to a hosted zone MCP server (`zone mcp login URL` first).
    Mcp {
        #[command(subcommand)]
        command: Option<McpCommand>,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Connect to the hosted zone MCP endpoint at URL (`https://…/mcp`): the zone owner opens the
    /// printed link in a browser on this machine and allows a zone.
    Login {
        url: String,
        /// Take the browser's redirect on this loopback port (default: any free one).
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
        /// Stop waiting for the consent after SECS.
        #[arg(long, value_name = "SECS", default_value_t = 900)]
        timeout: u64,
    },
}

#[derive(Subcommand)]
enum ReceiptsCommand {
    /// Verify the zone's signed action receipts and each actor's chain, offline. Exit 5 names the
    /// actor and seq of each receipt that is bad, dropped, repeated or out of order.
    Verify {
        /// Check the receipts in FILE (`{workspace_id, devices, receipts}`) instead of the
        /// joined zone's. Its devices are trusted as given.
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    state::restrict_umask();
    let cli = Cli::parse();
    transport::set_verbose(cli.verbose);
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("zone: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    let state = state::resolve(cli.state)?;
    // A key is checked before any subcommand runs, so a bad one fails the same way everywhere.
    let kit = cli
        .key_file
        .as_deref()
        .map(secret::KitKey::read)
        .transpose()?;
    match cli.command {
        Command::Join {
            invite,
            invite_file,
            core,
            kit: with_kit,
            no_wait,
            timeout,
        } => {
            let kit = match (with_kit, kit.as_ref()) {
                (true, None) => return Err("join --kit needs the kit key: --key-file FILE".into()),
                (true, Some(k)) => Some(k),
                (false, _) => None,
            };
            let invite = match (invite.as_deref(), invite_file) {
                (_, Some(path)) => secret::read_invite(&path)?,
                (Some("-"), None) => {
                    if cli.key_file.as_deref() == Some(std::path::Path::new("-")) {
                        return Err(
                            "the kit key and the invite cannot both come from stdin; use \
                             --invite-file"
                                .into(),
                        );
                    }
                    secret::read_invite(std::path::Path::new("-"))?
                }
                (Some(code), None) if bp_workspace_core::space::is_join_address(code.trim()) => {
                    zeroize::Zeroizing::new(code.to_owned())
                }
                (Some(code), None) => {
                    eprintln!(
                        "zone join: an invite on the command line shows in shell history and \
                         process listings; use `zone join -` or --invite-file"
                    );
                    zeroize::Zeroizing::new(code.to_owned())
                }
                (None, None) => unreachable!("clap requires the invite or --invite-file"),
            };
            let outcome = join::run(
                &state,
                join::Options {
                    code: &invite,
                    core: &core,
                    kit,
                    wait: !no_wait,
                    timeout: timeout.map(std::time::Duration::from_secs),
                },
            )?;
            Ok(match outcome {
                join::Outcome::Done => ExitCode::SUCCESS,
                join::Outcome::Refused { kind, message } => {
                    eprintln!("zone join: refused ({kind}): {message}");
                    ExitCode::from(EXIT_REFUSED)
                }
                join::Outcome::TimedOut => {
                    eprintln!("zone join: not admitted yet; `zone join` again resumes the wait");
                    ExitCode::from(EXIT_TIMED_OUT)
                }
            })
        }
        Command::Status => {
            status(&state)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Run { rounds } => {
            run::run(&state, rounds)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Receipts {
            command: ReceiptsCommand::Verify { file },
        } => {
            let input = match file {
                Some(f) => receipts::from_file(&f)?,
                None => receipts::from_state(&state)?,
            };
            Ok(verify_receipts(&input))
        }
        Command::Whoami => {
            whoami(&state, kit.as_ref())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Note { text } => {
            items::note(&state, (text != "-").then_some(text))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Notes { since } => {
            items::list(&state, bp_zone_core::ItemKind::Note, since)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::SendFile { path, name, mime } => {
            items::send_file(&state, &path, name, mime)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Files { since } => {
            items::list(&state, bp_zone_core::ItemKind::File, since)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Get { id, output, force } => {
            items::get(&state, &id, &output, force)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Mcp { command: None } => {
            mcp::serve(&state)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Mcp {
            command: Some(McpCommand::Login { url, port, timeout }),
        } => {
            mcp::login(&state, &url, port, std::time::Duration::from_secs(timeout))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `zone receipts verify`: one line per verified chain on stdout, one per failure on stderr.
fn verify_receipts(input: &receipts::Input) -> ExitCode {
    let (chains, failures) = receipts::verify(input);
    for c in &chains {
        println!(
            "ok {} seq {}..={} ({} receipts)",
            c.actor, c.first_seq, c.last_seq, c.count
        );
    }
    for f in &failures {
        eprintln!("zone receipts verify: {f}");
    }
    if failures.is_empty() {
        let n: usize = chains.iter().map(|c| c.count).sum();
        println!("verified {n} receipts in {} chains", chains.len());
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_UNVERIFIED)
    }
}

/// `zone status`: one JSON object. `state` is `admitted`, `pending` or `not-joined`; times are
/// ms since the epoch, null when unknown; `lastFailure` is a kind, never Core's text.
fn status(state: &std::path::Path) -> Result<(), String> {
    let (zone_id, pseudonym, membership, members) =
        match device::open_existing(state, Arc::new(device::Offline))? {
            None => (None, None, "not-joined", 0),
            Some(d) => match device::identity(&d)? {
                None => (None, None, "not-joined", 0),
                Some(id) => {
                    let members = d.members();
                    let pseudonym = id.pseudonym.or_else(|| {
                        members
                            .iter()
                            .find(|m| m.is_me)
                            .map(|m| m.member_id.clone())
                    });
                    let membership = if d.status().admitted {
                        "admitted"
                    } else {
                        "pending"
                    };
                    (Some(id.zone_id), pseudonym, membership, members.len())
                }
            },
        };
    let record = record::SyncRecord::load(state);
    println!(
        "{}",
        serde_json::json!({
            "zoneId": zone_id,
            "pseudonym": pseudonym,
            "state": membership,
            "members": members,
            "lastSyncMs": record.last_sync_ms,
            "nextSyncMs": record.next_sync_ms,
            "lastFailure": record.last_failure,
        })
    );
    Ok(())
}

fn whoami(state: &std::path::Path, kit: Option<&secret::KitKey>) -> Result<(), String> {
    println!("state: {}", state.display());
    match device::open_existing(state, Arc::new(device::Offline))? {
        None => println!("device: none yet; `zone join` makes one"),
        Some(d) => match device::identity(&d)? {
            None => println!("zone: none joined"),
            Some(id) => {
                println!("zone: {}", id.zone_id);
                println!("device: {}", id.device_fingerprint);
                match id.pseudonym {
                    Some(p) => println!("pseudonym: {p}"),
                    None => println!("pseudonym: not yet attested in the roster"),
                }
            }
        },
    }
    if let Some(k) = kit {
        println!("kit: {}", k.fingerprint());
    }
    Ok(())
}
