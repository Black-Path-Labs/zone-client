//! `zone`: the command-line client for Black Path Zones. Every Zone and Core operation goes
//! through the zone-sdk crates; this binary only parses arguments and prints results.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

mod backoff;
mod device;
#[cfg(unix)]
mod fs;
#[cfg(unix)]
mod fsmount;
mod head;
mod items;
mod join;
mod links;
mod live;
mod mcp;
mod receipts;
mod record;
mod room;
mod run;
mod secret;
mod slash;
mod srv;
mod state;
mod stream;
mod transport;
mod vault;
mod view;

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
    /// Join a zone with a magic link, an invite link, a zone code or a join address, and wait
    /// until admitted. Pass the link, zone code or join address on stdin (`zone join -`) or in a file
    /// (`--invite-file`), never as an argument: an argument shows in shell history and process
    /// listings. A join address is a secret: anyone who has it and a kit key can join.
    Join {
        /// The magic link or invite link (`…#join=bpzi2.…`, `…#join=bpzi1.…` or the bare
        /// code), zone code (`bpzc1.…`)
        /// or join address (`za_…`), or `-` to read it from stdin. Given here it works but draws
        /// a warning.
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
    /// Print the zone's members as JSON: pseudonym, name, kind (human, agent, ...), admin, devices
    /// and the capabilities each offers, with whether each is online. Reads the roster this
    /// device last synced, syncing once first when it is over a minute old; `--sync` always syncs.
    Members {
        #[arg(long)]
        sync: bool,
    },
    /// Print the zone's admission grants as JSON: places, used and left, expiry, the kinds and
    /// scope each admits, whether revoked, and the per-agent caps (AORs, rooms, lines, admissions
    /// per day; null when the owner set none). Reads the last synced roster,
    /// syncing once first when it is over a minute old; `--sync` always syncs.
    Grants {
        #[arg(long)]
        sync: bool,
    },
    /// Keep the joined zone synced until SIGINT or SIGTERM, and hold the live session: notes
    /// other members push arrive at once, and `zone note` goes through it.
    Run {
        /// Stop after N syncs.
        #[arg(long, value_name = "N")]
        rounds: Option<u32>,
        #[command(flatten)]
        live: live::LiveArgs,
    },
    /// Work with zone receipts.
    Receipts {
        #[command(subcommand)]
        command: ReceiptsCommand,
    },
    /// Show this device's identity: fingerprints and pseudonym, never key material.
    Whoami,
    /// Post a note, sealed to its recipients: TEXT, or `-` to read stdin (at most 32 KiB). To the
    /// whole zone, or with `--to` to one member or a group of up to 32.
    Note {
        /// A recipient's pseudonym (from `zone members`); repeat for a group, up to 32. Without
        /// it the note goes to the whole zone.
        #[arg(long = "to", value_name = "PSEUDONYM")]
        to: Vec<String>,
        /// The note's text, or `-` to read it from stdin.
        text: String,
    },
    /// Sync once and print the inbox's notes as JSON lines, oldest first: those addressed to
    /// this member or sent by it, and zone-wide ones only when subscribed (`zone subscribe`).
    Notes {
        /// Only notes newer than TS (ms since the epoch, a `ts_ms` printed before).
        #[arg(long, value_name = "TS")]
        since: Option<i64>,
        /// Wait up to SECS for a note that passes the inbox filter and print what arrived.
        #[arg(long, value_name = "SECS")]
        wait: Option<u32>,
    },
    /// Turn zone-wide notes and files on (`zone`) or off (`--off`) for this device's inbox, or
    /// with neither show the subscriptions. Off by default: only what is addressed to you.
    Subscribe {
        /// `zone`: every zone-wide note and file reaches the inbox.
        #[arg(value_parser = ["zone"], conflicts_with = "off")]
        what: Option<String>,
        /// Only addressed notes and files reach the inbox.
        #[arg(long)]
        off: bool,
    },
    /// Send a file, sealed to its recipients: up to 32 MiB through the mailbox, up to 1 GiB as a
    /// resumable large file, or any size live over a stream to one online member (`--live`).
    SendFile {
        path: PathBuf,
        /// The name members see (default: the path's file name).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// The file's media type (default `application/octet-stream`).
        #[arg(long, value_name = "TYPE")]
        mime: Option<String>,
        /// A recipient's pseudonym; repeat for a group, up to 32. Without it the file goes to
        /// the whole zone.
        #[arg(long = "to", value_name = "PSEUDONYM")]
        to: Vec<String>,
        /// Send it live over a stream to this member, who runs `zone stream offer`. Any size;
        /// both must be online.
        #[arg(long, value_name = "PSEUDONYM", conflicts_with = "to")]
        live: Option<String>,
        #[command(flatten)]
        sip: stream::SipArgs,
    },
    /// Sync once and print the inbox's files as JSON lines, oldest first, with the receipts
    /// members signed for the files this device sent.
    Files {
        /// Only files newer than TS (ms since the epoch, a `ts_ms` printed before).
        #[arg(long, value_name = "TS")]
        since: Option<i64>,
        /// Wait up to SECS for a file that passes the inbox filter and print what arrived.
        #[arg(long, value_name = "SECS")]
        wait: Option<u32>,
    },
    /// Save a complete file, checked against its sender's signed SHA-256, then send the sender
    /// a signed receipt. A large file resumes where an earlier run stopped.
    Get {
        /// The file's id, from `zone files`.
        id: String,
        /// Where to write it, or `-` for stdout (files up to 32 MiB).
        #[arg(short, long, value_name = "PATH")]
        output: PathBuf,
        /// Replace an existing file at PATH.
        #[arg(long)]
        force: bool,
        /// Do not send the sender a file receipt.
        #[arg(long)]
        no_receipt: bool,
    },
    /// Browse and search the source trees other members export to you (`zone fs <agent> ls|cat|
    /// tree|stat|grep|find`), and export, grant and serve your own. Read-only; runs on the
    /// exporter's host.
    #[cfg(unix)]
    Fs {
        #[command(subcommand)]
        command: fs::FsCommand,
    },
    /// Publish a file or folder of one of your exports under a stable name (`zone share NAME
    /// PATH`), list your shares, or remove one. Readers use the name as a path.
    #[cfg(unix)]
    Share {
        #[command(subcommand)]
        command: fs::ShareCommand,
    },
    /// Stream bytes or media between two members, end to end, with a signed receipt each side.
    Stream {
        #[command(subcommand)]
        command: StreamCommand,
    },
    /// Meeting rooms, end to end: create, list, close, set who may join, invite, and join with
    /// audio, video and data. Recording needs every member's consent.
    Room {
        #[command(subcommand)]
        command: RoomCommand,
    },
    /// Invite one person with a magic link, or revoke one (`zone link new`, `zone link revoke`).
    Link {
        #[command(subcommand)]
        command: LinkCommand,
    },
    /// Print the zone's magic links as JSON lines: id, label, state (open, used, revoked or
    /// expired), times, and the member a used link admitted.
    Links,
    /// Who in the zone is online now (`/who`), from the live session `zone run` holds; without
    /// one, presence is unknown. Reads local state only.
    Who,
    /// One member (`/whois`): kind, devices and whether each is online, capabilities. MEMBER is
    /// a pseudonym, a pseudonym prefix of 6 or more characters, or a name or its start.
    Whois {
        #[arg(required = true, num_args = 1..)]
        member: Vec<String>,
    },
    /// What a member is working on now (`/head`): with no option, this member's head, or
    /// MEMBER's as this device last heard it. With `--set STATE`, `--focus`, `--next`, `--task`,
    /// `--blocked-on`, `--private` or `--public`, set this member's head through the running
    /// `zone run`, which pushes it sealed to every member when it changed (never to Core).
    /// `--watch` prints every head change the run takes as JSON lines.
    Head {
        /// A pseudonym, a pseudonym prefix of 6 or more characters, or a name or its start.
        member: Vec<String>,
        #[command(flatten)]
        set: head::SetArgs,
        /// Print each head change as it arrives (JSON lines), until interrupted.
        #[arg(long, conflicts_with_all = ["set", "task", "focus", "next", "blocked_on", "private", "public", "reset"])]
        watch: bool,
        /// With `--watch`: only these states (repeatable).
        #[arg(long = "only-state", requires = "watch", value_name = "STATE")]
        only_state: Vec<String>,
    },
    /// What everyone in the zone is working on (`/heads`), from the heads the running `zone run`
    /// holds.
    Heads,
    /// The zone's capabilities as last synced (`/caps`).
    Caps,
    /// What a capability costs (`/price`): one Core read, by kind, id or tag.
    Price {
        #[arg(required = true, num_args = 1..)]
        capability: Vec<String>,
    },
    /// This device and member (`/me`): pseudonym, name, fingerprint, admitted, online.
    Me,
    /// The zone rep's three questions after joining (`/onboard`): with no ANSWER, start them
    /// again; with one, answer the current question (`skip`, `skip all`) or the draft (`yes`
    /// publishes it, anything else publishes nothing). Answers stay on this device until a yes.
    Onboard {
        #[arg(num_args = 0..)]
        answer: Vec<String>,
    },
    /// Your zone profile (`/profile`); `zone profile off` withdraws it.
    Profile {
        #[arg(value_parser = ["off"])]
        off: Option<String>,
    },
    /// Run any slash command line (`zone slash "/whois Aly"`), as the composer would. `/help`
    /// lists them.
    Slash { line: String },
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
enum StreamCommand {
    /// Publish this device's stream and answer one caller: a bytes stream goes to stdout, an rtp
    /// stream to `--record DIR`. Prints the receipt as one JSON line at the end (on stderr for
    /// bytes, whose stdout is the stream).
    Offer {
        /// Admit only this pseudonym; repeat for more. Without it any member may call.
        #[arg(long, value_name = "PSEUDONYM")]
        allow: Vec<String>,
        #[arg(long, value_enum, default_value_t = stream::Mode::Bytes)]
        mode: stream::Mode,
        /// rtp: write the received audio (`audio.opus`) and video (`video.ivf`) into DIR.
        #[arg(long, value_name = "DIR")]
        record: Option<PathBuf>,
        #[command(flatten)]
        sip: stream::SipArgs,
    },
    /// Call PSEUDONYM's stream and send stdin (bytes) or `--audio`/`--video` (rtp). Prints the
    /// receipt as one JSON line at the end.
    Open {
        /// The member whose stream to open.
        member: String,
        #[arg(long, value_enum, default_value_t = stream::Mode::Bytes)]
        mode: stream::Mode,
        /// A capability-call quote id, for a paid stream.
        #[arg(long, value_name = "ID")]
        quote: Option<String>,
        /// rtp: Ogg Opus audio from a file, or `exec:PROGRAM ARGS` whose stdout is Ogg Opus.
        #[arg(long, value_name = "SRC")]
        audio: Option<String>,
        /// rtp: IVF VP8 video from a file, or `exec:PROGRAM ARGS` whose stdout is IVF.
        #[arg(long, value_name = "SRC")]
        video: Option<String>,
        #[command(flatten)]
        sip: stream::SipArgs,
    },
}

#[derive(Subcommand)]
enum RoomCommand {
    /// Create a room in the zone and print it as JSON.
    Create {
        name: String,
        /// A room-create quote id, needed beyond the free rooms.
        #[arg(long, value_name = "ID")]
        quote: Option<String>,
    },
    /// Print the zone's live rooms as JSON lines.
    List,
    /// Close a room for good: one this device created, or any from an owner device.
    Close { id: String },
    /// Set who may join a room: `members` (any member), `invite` (signed invites only) or
    /// `listed` (the `--aor` and `--member` given).
    Acl {
        id: String,
        #[arg(value_parser = ["members", "invite", "listed"])]
        mode: String,
        /// `listed`: an agent SIP address that may join; repeat for more.
        #[arg(long = "aor", value_name = "AOR")]
        aors: Vec<String>,
        /// `listed`: a member's pseudonym that may join; repeat for more.
        #[arg(long = "member", value_name = "PSEUDONYM")]
        members: Vec<String>,
    },
    /// Invite a member to a room. The invite goes to that member alone as a direct note; this
    /// prints its id and expiry, never its token.
    Invite {
        id: String,
        pseudonym: String,
        /// How long the invite is good for.
        #[arg(long, value_name = "SECS", default_value_t = 600)]
        ttl: u64,
    },
    /// Join a room and stay until `--for` runs out or SIGINT. Prints the join, every event
    /// (participants, key epochs, `recording: on|off`) and the leave as JSON lines.
    Join {
        id: String,
        /// Ogg Opus audio from a file (played in a loop) or `exec:PROGRAM ARGS`.
        #[arg(long, value_name = "SRC")]
        audio: Option<String>,
        /// IVF VP8 video from a file (played in a loop) or `exec:PROGRAM ARGS`.
        #[arg(long, value_name = "SRC")]
        video: Option<String>,
        /// Send each stdin line as a room data message and print room data as JSON lines.
        #[arg(long)]
        data: bool,
        /// Leave after SECS.
        #[arg(long = "for", value_name = "SECS")]
        for_secs: Option<u64>,
        /// Ask every member present to consent, and record into DIR on this device only once
        /// all have.
        #[arg(long, value_name = "DIR")]
        record: Option<PathBuf>,
        /// Answer another member's recording request; without it a terminal is asked, and
        /// otherwise no consent is given.
        #[arg(long, value_name = "ANSWER", value_parser = ["yes", "no"])]
        consent_record: Option<String>,
    },
}

#[derive(Subcommand)]
enum LinkCommand {
    /// Make a one-time join link for one person (the zone's owner, or a device granted
    /// invite-people). Prints `{link_id, expires_at}`; the link itself goes only to --out. Whoever
    /// opens the link first joins, so send it to that person alone.
    New {
        /// Who the link is for (1 to 120 bytes), shown in `zone links`.
        #[arg(long = "for", value_name = "LABEL")]
        label: String,
        /// How long the link lasts: Ns, Nm or Nh, 1m to 24h.
        #[arg(long, value_name = "TTL", default_value = "15m", value_parser = links::parse_ttl)]
        ttl: u64,
        /// Write the link to a new file PATH (mode 0600), or `-` for stdout (the JSON line then
        /// goes to stderr).
        #[arg(long, value_name = "PATH", default_value = "-")]
        out: PathBuf,
    },
    /// Revoke an unused link. A used link stays used: remove the member it admitted instead.
    Revoke {
        /// The link's id (`ml_…`), from `zone links`.
        link_id: String,
    },
}

#[derive(Subcommand)]
enum ReceiptsCommand {
    /// The latest receipts this device opened, with their count (`/receipts`). Local only.
    List,
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
        Command::Members { sync } => {
            println!("{}", view::members(&state, sync)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Grants { sync } => {
            println!("{}", view::grants(&state, sync)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Run { rounds, live } => {
            run::run(&state, rounds, live)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Receipts {
            command: ReceiptsCommand::List,
        } => {
            slash::run(&state, "/receipts")?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Who => {
            slash::run(&state, "/who")?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Whois { member } => {
            slash::run(&state, &format!("/whois {}", member.join(" ")))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Head {
            member,
            set,
            watch,
            only_state,
        } => {
            if watch {
                let members = head_members(&state, &member)?;
                head::watch(&state, &members, &only_state)?;
            } else if set.any() {
                if !member.is_empty() {
                    return Err("head: set only your own head (drop MEMBER)".into());
                }
                head::set(&state, &set)?;
            } else {
                slash::run(&state, format!("/head {}", member.join(" ")).trim_end())?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Heads => {
            slash::run(&state, "/heads")?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Caps => {
            slash::run(&state, "/caps")?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Price { capability } => {
            slash::run(&state, &format!("/price {}", capability.join(" ")))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Me => {
            slash::run(&state, "/me")?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Onboard { answer } => {
            slash::run(&state, &format!("/onboard {}", answer.join(" ")))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Profile { off } => {
            slash::run(&state, &format!("/profile {}", off.unwrap_or_default()))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Slash { line } => {
            slash::run(&state, &line)?;
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
        Command::Note { to, text } => {
            items::note(&state, (text != "-").then_some(text), to)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Notes { since, wait } => {
            items::list(&state, bp_zone_core::ItemKind::Note, since, wait)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Subscribe { what, off } => {
            let on = if off { Some(false) } else { what.map(|_| true) };
            items::subscribe(&state, on)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::SendFile {
            path,
            live: Some(member),
            sip,
            ..
        } => {
            stream::send_live(&state, member, &path, &sip)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::SendFile {
            path,
            name,
            mime,
            to,
            live: None,
            sip: _,
        } => {
            items::send_file(&state, &path, name, mime, to)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Files { since, wait } => {
            items::list(&state, bp_zone_core::ItemKind::File, since, wait)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Get {
            id,
            output,
            force,
            no_receipt,
        } => {
            items::get(&state, &id, &output, force, !no_receipt)?;
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(unix)]
        Command::Share { command } => {
            fs::share_command(&state, command)?;
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(unix)]
        Command::Fs { command } => {
            fs::command(&state, command)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Stream {
            command:
                StreamCommand::Offer {
                    allow,
                    mode,
                    record,
                    sip,
                },
        } => {
            stream::offer(&state, allow, mode, record, &sip)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Stream {
            command:
                StreamCommand::Open {
                    member,
                    mode,
                    quote,
                    audio,
                    video,
                    sip,
                },
        } => {
            stream::open(&state, member, mode, quote, audio, video, &sip)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Room { command } => {
            match command {
                RoomCommand::Create { name, quote } => room::create(&state, name, quote)?,
                RoomCommand::List => room::list(&state)?,
                RoomCommand::Close { id } => room::close(&state, id)?,
                RoomCommand::Acl {
                    id,
                    mode,
                    aors,
                    members,
                } => room::acl(&state, id, mode, aors, members)?,
                RoomCommand::Invite { id, pseudonym, ttl } => {
                    room::invite(&state, id, pseudonym, ttl)?
                }
                RoomCommand::Join {
                    id,
                    audio,
                    video,
                    data,
                    for_secs,
                    record,
                    consent_record,
                } => room::join_room(
                    &state,
                    id,
                    room::JoinOptions {
                        audio,
                        video,
                        data,
                        for_secs,
                        record,
                        consent: consent_record.map(|a| a == "yes"),
                    },
                )?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Link {
            command: LinkCommand::New { label, ttl, out },
        } => {
            links::new(&state, &label, ttl, &out)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Link {
            command: LinkCommand::Revoke { link_id },
        } => {
            links::revoke(&state, &link_id)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Links => {
            links::list(&state)?;
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

/// `zone head --watch MEMBER`: the one member MEMBER names, as `/whois` matches it.
fn head_members(state: &std::path::Path, member: &[String]) -> Result<Vec<String>, String> {
    if member.is_empty() {
        return Ok(vec![]);
    }
    let r = slash::answer(state, &format!("/whois {}", member.join(" ")))?;
    match r.json["member"]["memberId"].as_str() {
        Some(id) if r.ok => Ok(vec![id.to_owned()]),
        _ => Err(format!("head: {}", r.text)),
    }
}
