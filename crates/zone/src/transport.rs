//! The SDK's `CoreTransport` over HTTP(S): one blocking exchange per call. The SDK builds every
//! request (method, URL, headers, body) and reads every reply; this only moves bytes. rustls with
//! the webpki roots, so the binary needs no system TLS library. Nothing here logs a header or a
//! body.

use std::io::Read;
use std::time::Duration;

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
                return Err(ZoneError::Core {
                    status: 0,
                    reason: format!("Core unreachable: {}", t.kind()),
                })
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
        Ok(HttpReply { status, body: out })
    }
}
