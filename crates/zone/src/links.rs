//! `zone link new`, `zone links` and `zone link revoke`: one-time magic links that invite one
//! named person into the joined zone. The link carries its secret, so it goes only to a new
//! 0600 file or to stdout, never into a JSON line, a log or an error message.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use bp_zone_core::{ZoneDevice, ZoneError};
use serde_json::json;
use zeroize::Zeroizing;

use crate::device;
use crate::join;
use crate::transport::Http;

/// The shortest and longest link lifetimes, in seconds.
const MIN_TTL_S: u64 = 60;
const MAX_TTL_S: u64 = 24 * 3600;

/// `15m`, `90s`, `2h`: a lifetime between 1 minute and 24 hours, in seconds.
pub fn parse_ttl(text: &str) -> Result<u64, String> {
    let bad = || format!("--ttl {text}: give Ns, Nm or Nh between 1m and 24h");
    let t = text.trim();
    let (n, unit) = t.split_at(t.len().saturating_sub(1));
    let n: u64 = n.parse().map_err(|_| bad())?;
    let secs = match unit {
        "s" => Some(n),
        "m" => n.checked_mul(60),
        "h" => n.checked_mul(3600),
        _ => None,
    }
    .ok_or_else(bad)?;
    if !(MIN_TTL_S..=MAX_TTL_S).contains(&secs) {
        return Err(bad());
    }
    Ok(secs)
}

fn joined(state: &Path) -> Result<Arc<ZoneDevice>, String> {
    device::open_existing(state, Arc::new(Http::default()))?
        .filter(|d| d.status().zone_id.is_some())
        .ok_or_else(|| "no zone joined: run `zone join` first".to_owned())
}

fn refused(cmd: &str, e: &ZoneError) -> String {
    match e {
        ZoneError::NotAdmin => {
            format!("{cmd}: only the zone's owner, or a device granted invite-people, can do that")
        }
        ZoneError::NotAdmitted => format!("{cmd}: not admitted yet; `zone status` shows the state"),
        ZoneError::BadCode { reason } => format!("{cmd}: {reason}"),
        e => format!("{cmd}: refused ({})", join::kind(e)),
    }
}

/// Write `link` to a new file at `out` (mode 0600; an existing file is refused).
fn write_new(out: &Path, link: &str) -> Result<(), String> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts
        .open(out)
        .map_err(|e| format!("link new: {}: {e}", out.display()))?;
    f.write_all(link.as_bytes())
        .and_then(|()| f.write_all(b"\n"))
        .and_then(|()| f.sync_all())
        .map_err(|e| {
            let _ = fs::remove_file(out);
            format!("link new: {}: {e}", out.display())
        })
}

/// `zone link new`: mint a link for `label`, valid `ttl_s`. The link goes to `out` (a new file,
/// or `-` for stdout); `{link_id, expires_at}` to stdout, or to stderr when the link took stdout.
pub fn new(state: &Path, label: &str, ttl_s: u64, out: &Path) -> Result<(), String> {
    let device = joined(state)?;
    let to_stdout = out == Path::new("-");
    if !to_stdout && fs::symlink_metadata(out).is_ok() {
        return Err(format!("link new: {} exists", out.display()));
    }
    let created = device
        .create_magic_link(label.to_owned(), ttl_s)
        .map_err(|e| refused("link new", &e))?;
    let link = Zeroizing::new(created.link);
    if to_stdout {
        let mut o = std::io::stdout().lock();
        writeln!(o, "{}", link.as_str())
            .and_then(|()| o.flush())
            .map_err(|e| format!("link new: stdout: {e}"))?;
    } else {
        write_new(out, &link)?;
    }
    let line = json!({"link_id": created.link_id, "expires_at": created.expires_at});
    if to_stdout {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
    Ok(())
}

/// `zone links`: one JSON line per link, as Core lists them for this device.
pub fn list(state: &Path) -> Result<(), String> {
    let device = joined(state)?;
    let links = device.magic_links().map_err(|e| refused("links", &e))?;
    for l in links {
        println!(
            "{}",
            json!({
                "link_id": l.link_id,
                "label": l.label,
                "state": l.state,
                "created_at": l.created_at,
                "expires_at": l.expires_at,
                "used_at": l.used_at,
                "joined": l.joined_as,
            })
        );
    }
    Ok(())
}

/// `zone link revoke`: revoke an unused link.
pub fn revoke(state: &Path, link_id: &str) -> Result<(), String> {
    let device = joined(state)?;
    device
        .revoke_magic_link(link_id.to_owned())
        .map_err(|e| refused("link revoke", &e))?;
    println!("{}", json!({"link_id": link_id, "state": "revoked"}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttls_parse_within_bounds() {
        assert_eq!(parse_ttl("15m").unwrap(), 900);
        assert_eq!(parse_ttl("60s").unwrap(), 60);
        assert_eq!(parse_ttl("24h").unwrap(), 86400);
        assert_eq!(parse_ttl("1m").unwrap(), 60);
        for bad in [
            "59s",
            "25h",
            "1441m",
            "15",
            "m",
            "",
            "-1m",
            "1.5h",
            "15d",
            "99999999999999999h",
        ] {
            assert!(parse_ttl(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_link_file_is_new_and_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("link");
        write_new(&path, "<link>").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "<link>\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(write_new(&path, "<link>").is_err());
    }
}
