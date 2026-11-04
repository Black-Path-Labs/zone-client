//! The SDK's `CoreTransport` over HTTP(S): one blocking exchange per call. The SDK builds every
//! request (method, URL, headers, body) and reads every reply; this only moves bytes. rustls with
//! the webpki roots, so the binary needs no system TLS library. One agent keeps its connections
//! alive, so the calls of a command after the first reuse one TLS session. Nothing here logs a
//! header or a body; with `-v` each exchange is one stderr line of the method, the route with
//! every id cut out, the status and the time it took.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bp_zone_core::{CoreTransport, HttpHeader, HttpReply, ZoneError};

pub struct Http {
    agent: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        Http {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                // A long-polling sync waits up to 25 s before Core answers.
                .timeout_read(Duration::from_secs(40))
                .timeout_write(Duration::from_secs(40))
                .build(),
        }
    }
}

static VERBOSE: AtomicBool = AtomicBool::new(false);
static STARTED: OnceLock<Instant> = OnceLock::new();

/// `-v`: time every exchange on stderr, from the start of the command.
pub fn set_verbose(on: bool) {
    STARTED.get_or_init(Instant::now);
    VERBOSE.store(on, Ordering::SeqCst);
}

/// Milliseconds since the command started (since `set_verbose`).
pub fn elapsed_ms() -> u128 {
    STARTED.get_or_init(Instant::now).elapsed().as_millis()
}

/// `-v`: a stderr line, stamped with the time since the command started.
pub fn trace(what: &str) {
    if VERBOSE.load(Ordering::SeqCst) {
        eprintln!("zone: +{}ms {what}", elapsed_ms());
    }
}

/// The route of `url` with no host, query or id: a segment that is not lowercase letters and
/// dashes (a zone id, a join address, a hash) shows as `…`, so `-v` output is safe to share.
fn route(url: &str) -> String {
    let path = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = path.split_once('/').map_or("", |(_, p)| p);
    let path = path.split(['?', '#']).next().unwrap_or("");
    if path.is_empty() {
        return "/".into();
    }
    let segments: Vec<&str> = path
        .split('/')
        .map(|s| {
            if !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
                s
            } else {
                "…"
            }
        })
        .collect();
    format!("/{}", segments.join("/"))
}

/// The largest reply read: a page of 1 MiB file chunks.
const MAX_REPLY_BYTES: u64 = 64 * 1024 * 1024;

impl CoreTransport for Http {
    fn send(
        &self,
        method: String,
        url: String,
        headers: Vec<HttpHeader>,
        body: Vec<u8>,
    ) -> Result<HttpReply, ZoneError> {
        let start = Instant::now();
        let traced = VERBOSE.load(Ordering::SeqCst).then(|| route(&url));
        let mut req = self.agent.request(&method, &url);
        for h in &headers {
            req = req.set(&h.name, &h.value);
        }
        let res = if body.is_empty() {
            req.call()
        } else {
            req.send_bytes(&body)
        };
        let res = match res {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => {
                if let Some(r) = &traced {
                    trace(&format!(
                        "{method} {r} -> unreachable ({}) {}ms",
                        t.kind(),
                        start.elapsed().as_millis()
                    ));
                }
                return Err(ZoneError::Core {
                    status: 0,
                    reason: format!("Core unreachable: {}", t.kind()),
                });
            }
        };
        let status = res.status();
        let mut out = Vec::new();
        res.into_reader()
            .take(MAX_REPLY_BYTES)
            .read_to_end(&mut out)
            .map_err(|e| ZoneError::Core {
                status: 0,
                reason: format!("reply cut short: {}", e.kind()),
            })?;
        if let Some(r) = &traced {
            trace(&format!(
                "{method} {r} -> {status} {}ms",
                start.elapsed().as_millis()
            ));
        }
        Ok(HttpReply { status, body: out })
    }
}

#[cfg(test)]
mod tests {
    use super::route;

    #[test]
    fn a_traced_route_keeps_no_host_query_or_id() {
        assert_eq!(
            route("https://core.example/api/workspaces/wks_01ABC/messages?after=7&wait=20"),
            "/api/workspaces/…/messages"
        );
        assert_eq!(
            route("https://core.example/api2/join-addresses/za_abcdefghijklmnop"),
            "/…/join-addresses/…"
        );
        assert_eq!(route("http://127.0.0.1:9"), "/");
    }
}
