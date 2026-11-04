//! Slash commands: `zone who`, `zone whois`, `zone caps`, `zone price`, `zone receipts list`,
//! `zone me`, `zone slash LINE`, a `zone note` that starts with `/`, and the MCP bridge's local
//! tools. The SDK owns the registry, the parser
//! and every answer (`ZoneDevice::run_command`), so all of them agree.
//!
//! A running `zone run` answers over `live.sock`: it holds the device and the presence view the
//! live session keeps, so `/who` is live there. Without one the command opens the device here and
//! presence is unknown. Either way nothing syncs: `/price` and `/links` make Core's one read, the
//! rest read local state. A command never reaches the zone.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use bp_zone_core::commands::{self, Parsed};
use bp_zone_core::CoreTransport;
use serde_json::{json, Value};

use crate::device;
use crate::transport::{trace, Http};

/// A command's answer: whether it worked, the structured result and its short text.
pub struct Reply {
    pub ok: bool,
    pub json: Value,
    pub text: String,
}

impl Reply {
    /// From the SDK's result, or `zone run`'s JSON form of it.
    fn from_wire(v: &Value) -> Reply {
        Reply {
            ok: v["ok"].as_bool().unwrap_or(false),
            json: v["json"].clone(),
            text: v["text"].as_str().unwrap_or_default().to_owned(),
        }
    }
}

/// The JSON `zone run` answers a `{"cmd": LINE}` with.
pub fn wire(r: Option<bp_zone_core::CommandResult>) -> Value {
    match r {
        Some(r) => json!({
            "name": r.name,
            "ok": r.ok,
            "json": serde_json::from_str::<Value>(&r.json).unwrap_or(Value::Null),
            "text": r.text,
        }),
        None => json!({"ok": false, "text": "not a command: it would be sent as a note"}),
    }
}

/// Answer `line`: through a running `zone run` when there is one, else from the device here.
pub fn answer(state: &Path, line: &str) -> Result<Reply, String> {
    let start = Instant::now();
    let name = match commands::parse(line) {
        Parsed::Text(_) => return Err("not a command: it would be sent as a note".into()),
        Parsed::Error(e) => {
            return Ok(Reply {
                ok: false,
                json: json!({"error": e}),
                text: e,
            })
        }
        Parsed::Command { name, .. } => name,
    };
    if let Some(v) = crate::live::ask_command(state, line)? {
        trace(&format!(
            "/{name} via zone run: {} ms",
            start.elapsed().as_millis()
        ));
        return Ok(Reply::from_wire(&v));
    }
    let core = commands::spec(name).is_some_and(|s| s.core);
    let transport: Arc<dyn CoreTransport> = if core {
        Arc::new(Http::default())
    } else {
        Arc::new(device::Offline)
    };
    let dev = device::open_existing(state, transport)?
        .filter(|d| d.status().zone_id.is_some())
        .ok_or("no zone joined: run `zone join` first")?;
    let r = dev
        .run_command(line.to_owned())
        .map_err(|e| format!("/{name}: {} ({e})", crate::join::kind(&e)))?;
    trace(&format!(
        "/{name} local: {} ms",
        start.elapsed().as_millis()
    ));
    Ok(Reply::from_wire(&wire(r)))
}

/// `zone <command>`: print the answer's JSON; a failed one is an error.
pub fn run(state: &Path, line: &str) -> Result<(), String> {
    let r = answer(state, line)?;
    if !r.ok {
        return Err(r.text);
    }
    println!("{}", r.json);
    Ok(())
}
