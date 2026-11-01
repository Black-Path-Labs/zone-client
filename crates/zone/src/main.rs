//! `zone`: the command-line client for Black Path Zones. Every Zone and Core operation goes
//! through the zone-sdk crates; this binary only parses arguments and prints results.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

mod device;
mod secret;
mod state;
mod vault;

/// Exit code of a subcommand that is not implemented yet.
const EXIT_NOT_IMPLEMENTED: u8 = 2;

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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Join a zone with an invite link or a join token.
    Join {
        /// The invite link or join token.
        invite: String,
    },
    /// Show this device's zones and their state.
    Status,
    /// Run the client: stay connected to joined zones.
    Run,
    /// Work with zone receipts.
    Receipts {
        #[command(subcommand)]
        command: ReceiptsCommand,
    },
    /// Show this device's identity: fingerprints and pseudonym, never key material.
    Whoami,
}

#[derive(Subcommand)]
enum ReceiptsCommand {
    /// Verify receipts against the zone's keys.
    Verify,
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Join { .. } => "join",
            Command::Status => "status",
            Command::Run => "run",
            Command::Receipts {
                command: ReceiptsCommand::Verify,
            } => "receipts verify",
            Command::Whoami => "whoami",
        }
    }
}

fn main() -> ExitCode {
    state::restrict_umask();
    let cli = Cli::parse();
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
        Command::Whoami => {
            whoami(&state, kit.as_ref())?;
            Ok(ExitCode::SUCCESS)
        }
        other => {
            eprintln!("zone {}: not implemented yet", other.name());
            Ok(ExitCode::from(EXIT_NOT_IMPLEMENTED))
        }
    }
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
