//! `zone`: the command-line client for Black Path Zones. Every Zone and Core operation goes
//! through the zone-sdk crates; this binary only parses arguments and prints results.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    /// Show this device's identity.
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
    let cli = Cli::parse();
    eprintln!("zone {}: not implemented yet", cli.command.name());
    ExitCode::from(EXIT_NOT_IMPLEMENTED)
}
