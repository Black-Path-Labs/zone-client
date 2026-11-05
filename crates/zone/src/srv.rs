//! The zone's SIP edge from DNS: the `_sip._udp.<SIP domain>` SRV record (RFC 3263), asked of
//! the first nameserver in /etc/resolv.conf over UDP. The zone service's host often sits behind
//! an HTTP-only proxy that carries no SIP, so its own name is no edge; the SRV record names one.
//! Only the lowest-priority answer is used, and nothing is cached.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use rand_core::{OsRng, RngCore};

/// How long the nameserver may take.
const WAIT: Duration = Duration::from_secs(2);
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;

/// `HOST:PORT` of the SIP edge `_sip._udp.<domain>` names, or `None` (no record, no resolver,
/// no answer in time).
pub fn sip_edge(domain: &str) -> Option<String> {
    let resolver = nameserver(&std::fs::read_to_string("/etc/resolv.conf").ok()?)?;
    let name = format!("_sip._udp.{}", domain.trim_end_matches('.'));
    let mut id = [0u8; 2];
    OsRng.fill_bytes(&mut id);
    let id = u16::from_be_bytes(id);
    let query = encode_query(id, &name)?;
    let bind: SocketAddr = match resolver {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    }
    .parse()
    .ok()?;
    let socket = UdpSocket::bind(bind).ok()?;
    socket.send_to(&query, SocketAddr::new(resolver, 53)).ok()?;
    // One deadline for the whole wait, so stray packets cannot keep the read going.
    let deadline = Instant::now() + WAIT;
    let mut buf = [0u8; 1500];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        socket
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .ok()?;
        let (n, from) = socket.recv_from(&mut buf).ok()?;
        if from.ip() == resolver {
            if let Some(answer) = parse_srv_answer(&buf[..n], id) {
                return Some(answer);
            }
            if n >= 2 && u16::from_be_bytes([buf[0], buf[1]]) == id {
                return None;
            }
        }
    }
}

/// The first `nameserver` line's address.
fn nameserver(resolv_conf: &str) -> Option<IpAddr> {
    resolv_conf.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        (words.next() == Some("nameserver"))
            .then(|| words.next()?.split('%').next()?.parse().ok())
            .flatten()
    })
}

fn encode_query(id: u16, name: &str) -> Option<Vec<u8>> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    // Recursion desired; one question.
    q.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&TYPE_SRV.to_be_bytes());
    q.extend_from_slice(&CLASS_IN.to_be_bytes());
    Some(q)
}

/// The SRV target of lowest priority (then highest weight) in a response to query `id`, as
/// `host:port`. `None` for another id, an error, or no SRV answer.
fn parse_srv_answer(msg: &[u8], id: u16) -> Option<String> {
    if msg.len() < 12 || u16::from_be_bytes([msg[0], msg[1]]) != id {
        return None;
    }
    // A response, with no error.
    if msg[2] & 0x80 == 0 || msg[3] & 0x0f != 0 {
        return None;
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    let an = u16::from_be_bytes([msg[6], msg[7]]);
    let mut at = 12;
    for _ in 0..qd {
        at = skip_name(msg, at)? + 4;
    }
    let mut best: Option<(u16, u16, String, u16)> = None;
    for _ in 0..an {
        at = skip_name(msg, at)?;
        let head = msg.get(at..at + 10)?;
        let kind = u16::from_be_bytes([head[0], head[1]]);
        let len = u16::from_be_bytes([head[8], head[9]]) as usize;
        let data = at + 10;
        at = data + len;
        if kind != TYPE_SRV || len < 7 {
            continue;
        }
        let rd = msg.get(data..data + 6)?;
        let priority = u16::from_be_bytes([rd[0], rd[1]]);
        let weight = u16::from_be_bytes([rd[2], rd[3]]);
        let port = u16::from_be_bytes([rd[4], rd[5]]);
        let target = read_name(msg, data + 6)?;
        // "." means the service is not offered there.
        if target.is_empty() {
            continue;
        }
        let better = best
            .as_ref()
            .is_none_or(|(p, w, _, _)| priority < *p || (priority == *p && weight > *w));
        if better {
            best = Some((priority, weight, target, port));
        }
    }
    best.map(|(_, _, host, port)| format!("{host}:{port}"))
}

/// The offset just past the name at `at` (labels, or a compression pointer).
fn skip_name(msg: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let len = *msg.get(at)?;
        match len {
            0 => return Some(at + 1),
            l if l & 0xc0 == 0xc0 => return Some(at + 2),
            l => at += 1 + l as usize,
        }
    }
}

/// The name at `at`, following compression pointers (at most 16), without the root dot.
fn read_name(msg: &[u8], mut at: usize) -> Option<String> {
    let mut labels: Vec<String> = vec![];
    let mut jumps = 0;
    loop {
        let len = *msg.get(at)?;
        if len == 0 {
            return Some(labels.join("."));
        }
        if len & 0xc0 == 0xc0 {
            jumps += 1;
            if jumps > 16 {
                return None;
            }
            at = (usize::from(len & 0x3f) << 8) | usize::from(*msg.get(at + 1)?);
            continue;
        }
        let label = msg.get(at + 1..at + 1 + len as usize)?;
        labels.push(std::str::from_utf8(label).ok()?.to_ascii_lowercase());
        at += 1 + len as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response to `_sip._udp.example.com` with two SRV answers, the second compressed.
    fn response(id: u16) -> Vec<u8> {
        let mut m = encode_query(id, "_sip._udp.example.com").unwrap();
        m[2] = 0x81;
        m[3] = 0x80;
        m[7] = 2;
        // priority 20 -> backup.example.com:5070
        m.extend_from_slice(&[0xc0, 12, 0, 33, 0, 1, 0, 0, 1, 44]);
        let target = b"\x06backup\x07example\x03com\x00";
        m.extend_from_slice(&((6 + target.len()) as u16).to_be_bytes());
        m.extend_from_slice(&[0, 20, 0, 5, 0x13, 0xce]);
        m.extend_from_slice(target);
        // priority 10 -> edge.<example.com via pointer to offset 22>:5060
        m.extend_from_slice(&[0xc0, 12, 0, 33, 0, 1, 0, 0, 1, 44]);
        let target = [&b"\x04edge"[..], &[0xc0, 22]].concat();
        m.extend_from_slice(&((6 + target.len()) as u16).to_be_bytes());
        m.extend_from_slice(&[0, 10, 0, 5, 0x13, 0xc4]);
        m.extend_from_slice(&target);
        m
    }

    #[test]
    fn takes_the_lowest_priority_target() {
        assert_eq!(
            parse_srv_answer(&response(7), 7).as_deref(),
            Some("edge.example.com:5060")
        );
    }

    #[test]
    fn ignores_another_id_an_error_and_a_query() {
        assert_eq!(parse_srv_answer(&response(7), 8), None);
        let mut nx = response(7);
        nx[3] = 0x83;
        assert_eq!(parse_srv_answer(&nx, 7), None);
        let q = encode_query(7, "_sip._udp.example.com").unwrap();
        assert_eq!(parse_srv_answer(&q, 7), None);
        assert_eq!(parse_srv_answer(&response(7)[..40], 7), None);
    }

    #[test]
    fn reads_the_first_nameserver() {
        let conf = "# generated\nsearch lan\nnameserver 127.0.0.53\nnameserver 1.1.1.1\n";
        assert_eq!(nameserver(conf), Some("127.0.0.53".parse().unwrap()));
        assert_eq!(
            nameserver("nameserver fe80::1%eth0"),
            Some("fe80::1".parse().unwrap())
        );
        assert_eq!(nameserver("search lan"), None);
    }
}
