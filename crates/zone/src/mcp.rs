//! `zone mcp`: an MCP server on stdio for a CLI agent, bridged message by message to a hosted zone
//! MCP server (MCP Streamable HTTP). The hosted server holds the zone tools and acts as the
//! agent's custodial membership of one zone; this only moves JSON-RPC messages and keeps the OAuth
//! tokens. It speaks no zone protocol itself.
//!
//! `zone mcp login URL` gets the tokens the way the hosted server offers them to a native client:
//! protected-resource and authorization-server metadata (RFC 9728, RFC 8414), dynamic client
//! registration (RFC 7591), then the authorization code flow with PKCE S256 and a loopback
//! redirect (RFC 8252). The zone owner opens the printed link, adds the agent identity the consent
//! screen shows to a zone, and allows it. Tokens live in `<state>/mcp.json` (0600); the refresh
//! token rotates on every use, so each new one is saved before it is used. Nothing here prints a
//! token.
//!
//! The slash commands (`zone who` and the rest, slash.rs) are local tools here, `zone_who`,
//! `zone_whois`, … `zone_help`, answered from this machine's joined zone device (and the presence
//! a running `zone run` holds), never by the hosted server: it is custodial and cannot open the
//! sealed roster. They join the hosted server's `tools/list`; with a joined device and no
//! `zone mcp login`, they are served alone. `zone_head_set` is local too: it sets this member's
//! head through the running `zone run` (head.rs), so an agent says what it is on when its task or
//! state changes, with no turn spent on it otherwise.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bp_workspace_core::b64;
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use url::Url;

use bp_zone_core::commands;

use crate::record::now_ms;
use crate::{device, slash, state};

/// Refresh an access token this close to its end rather than send it.
const REFRESH_MARGIN_MS: u64 = 30_000;
/// The largest reply read from the hosted server.
const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;
/// The JSON-RPC error a request gets when the bridge, not the server, failed it.
const BRIDGE_ERROR: i64 = -32000;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(120))
        .timeout_write(Duration::from_secs(30))
        .redirects(0)
        .build()
}

fn path(state: &Path) -> PathBuf {
    state.join("mcp.json")
}

/// One connection to a hosted zone MCP server: what `zone mcp` needs to call it and refresh.
#[derive(Debug, PartialEq)]
struct Connection {
    /// The MCP endpoint, as its protected-resource metadata names it.
    resource: String,
    token_endpoint: String,
    client_id: String,
    access_token: String,
    expires_at_ms: u64,
    refresh_token: Option<String>,
}

impl Connection {
    fn load(state: &Path) -> Result<Connection, String> {
        let file = path(state);
        let bytes = match std::fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err("not connected: run `zone mcp login URL` first".into())
            }
            Err(e) => return Err(format!("{}: {}", file.display(), e.kind())),
        };
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|_| format!("{} is not JSON", file.display()))?;
        let s = |k: &str| {
            v[k].as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{} has no {k}", file.display()))
        };
        Ok(Connection {
            resource: s("resource")?,
            token_endpoint: s("tokenEndpoint")?,
            client_id: s("clientId")?,
            access_token: s("accessToken")?,
            expires_at_ms: v["expiresAtMs"].as_u64().unwrap_or(0),
            refresh_token: v["refreshToken"].as_str().map(str::to_owned),
        })
    }

    /// Write it whole: a temp file renamed over the old one, both 0600 under the client's umask.
    fn save(&self, state: &Path) -> Result<(), String> {
        let body = json!({
            "resource": self.resource,
            "tokenEndpoint": self.token_endpoint,
            "clientId": self.client_id,
            "accessToken": self.access_token,
            "expiresAtMs": self.expires_at_ms,
            "refreshToken": self.refresh_token,
        });
        let file = path(state);
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, body.to_string())
            .and_then(|()| std::fs::rename(&tmp, &file))
            .map_err(|e| format!("{}: {}", file.display(), e.kind()))
    }

    /// Take a token endpoint answer: a new access token and, if given, the rotated refresh token.
    fn take_tokens(&mut self, v: &Value) -> Result<(), String> {
        self.access_token = v["access_token"]
            .as_str()
            .ok_or("the token endpoint gave no access_token")?
            .to_owned();
        let ttl = v["expires_in"].as_u64().unwrap_or(0);
        self.expires_at_ms = now_ms() + ttl * 1000;
        if let Some(r) = v["refresh_token"].as_str() {
            self.refresh_token = Some(r.to_owned());
        }
        Ok(())
    }
}

fn random_b64(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    OsRng.fill_bytes(&mut bytes);
    b64::encode(&bytes)
}

/// RFC 7636 S256: base64url(sha256(verifier)), both through the SDK.
fn pkce_challenge(verifier: &str) -> String {
    let tagged = bp_workspace_core::sha256_tagged(verifier.as_bytes());
    let hex = tagged.trim_start_matches("sha256:");
    let digest: Vec<u8> = (0..hex.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();
    b64::encode(&digest)
}

/// A short, single-line excerpt of a server's text for an error message.
fn excerpt(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect::<String>()
}

/// Read a reply whole (bounded): status, content type, body.
fn read(res: ureq::Response) -> Result<(u16, String, String), String> {
    let status = res.status();
    let content_type = res.content_type().to_ascii_lowercase();
    let mut body = String::new();
    res.into_reader()
        .take(MAX_REPLY_BYTES)
        .read_to_string(&mut body)
        .map_err(|e| format!("reply cut short: {}", e.kind()))?;
    Ok((status, content_type, body))
}

/// Any status is an answer; only a transport failure is an error.
fn answer(res: Result<ureq::Response, ureq::Error>, what: &str) -> Result<ureq::Response, String> {
    match res {
        Ok(r) | Err(ureq::Error::Status(_, r)) => Ok(r),
        Err(ureq::Error::Transport(t)) => Err(format!("{what} unreachable: {}", t.kind())),
    }
}

fn get_json(agent: &ureq::Agent, url: &str) -> Result<Value, String> {
    let (status, _, body) = read(answer(agent.get(url).call(), url)?)?;
    if status != 200 {
        return Err(format!("{url} answered HTTP {status}"));
    }
    serde_json::from_str(&body).map_err(|_| format!("{url} did not answer JSON"))
}

/// The `resource_metadata` parameter of a `WWW-Authenticate: Bearer …` challenge (RFC 9728 §5.1).
fn resource_metadata_param(challenge: &str) -> Option<String> {
    let at = challenge.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &challenge[at..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"')?],
        None => rest.split([',', ' ']).next()?,
    };
    Some(value.to_owned())
}

/// The RFC 8414 / RFC 9728 well-known URL for `base`: the suffix goes between host and path.
fn well_known(base: &Url, suffix: &str) -> String {
    let path = base.path().trim_end_matches('/');
    format!(
        "{}/.well-known/{suffix}{path}",
        base.origin().ascii_serialization()
    )
}

/// What `login` needs from the server's metadata.
#[derive(Debug)]
struct Discovered {
    resource: String,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
    scope: Option<String>,
}

fn discover(agent: &ureq::Agent, mcp: &Url) -> Result<Discovered, String> {
    // An unauthenticated request: the 401 names the protected-resource metadata.
    let probe = answer(
        agent
            .post(mcp.as_str())
            .set("content-type", "application/json")
            .set("accept", "application/json, text/event-stream")
            .send_string("{}"),
        mcp.as_str(),
    )?;
    let prm_url = (probe.status() == 401)
        .then(|| {
            probe
                .header("www-authenticate")
                .and_then(resource_metadata_param)
        })
        .flatten()
        .unwrap_or_else(|| well_known(mcp, "oauth-protected-resource"));
    let prm = get_json(agent, &prm_url)?;
    let resource = prm["resource"]
        .as_str()
        .ok_or("the protected-resource metadata names no resource")?
        .to_owned();
    // RFC 9728 §3.3: the metadata must be for the endpoint asked about.
    if resource.trim_end_matches('/') != mcp.as_str().trim_end_matches('/') {
        return Err(format!(
            "{prm_url} is for {resource}, not {mcp}; give the MCP endpoint URL"
        ));
    }
    let issuer = prm["authorization_servers"][0]
        .as_str()
        .ok_or("the protected-resource metadata names no authorization server")?
        .to_owned();
    let issuer_url = Url::parse(&issuer).map_err(|_| format!("bad issuer {issuer}"))?;
    let asm = get_json(
        agent,
        &well_known(&issuer_url, "oauth-authorization-server"),
    )?;
    if asm["issuer"].as_str().map(|s| s.trim_end_matches('/')) != Some(issuer.trim_end_matches('/'))
    {
        return Err("the authorization server metadata is for another issuer".into());
    }
    let s256 = asm["code_challenge_methods_supported"]
        .as_array()
        .is_some_and(|m| m.iter().any(|x| x == "S256"));
    if !s256 {
        return Err("the authorization server does not offer PKCE S256".into());
    }
    let endpoint = |k: &str, what: &str| {
        asm[k]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("the authorization server offers no {what}"))
    };
    let scope = asm["scopes_supported"]
        .as_array()
        .or_else(|| prm["scopes_supported"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty());
    Ok(Discovered {
        resource,
        issuer,
        authorization_endpoint: endpoint("authorization_endpoint", "authorization endpoint")?,
        token_endpoint: endpoint("token_endpoint", "token endpoint")?,
        registration_endpoint: endpoint(
            "registration_endpoint",
            "dynamic client registration for a native client",
        )?,
        scope,
    })
}

/// The query of the redirect the browser brought back: the code, or the server's refusal.
fn callback_code(target: &str, state: &str, issuer: &str) -> Result<Option<String>, String> {
    let url = Url::parse(&format!("http://127.0.0.1{target}")).map_err(|_| "bad callback")?;
    if url.path() != "/callback" {
        return Ok(None);
    }
    let q = |k: &str| {
        url.query_pairs()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.into_owned())
    };
    if q("state").as_deref() != Some(state) {
        return Err(
            "the redirect carries another login's state; start `zone mcp login` again".into(),
        );
    }
    // RFC 9207: when the server names itself, it must be the one asked.
    if let Some(iss) = q("iss") {
        if iss.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(format!("the redirect comes from {iss}, not {issuer}"));
        }
    }
    if let Some(error) = q("error") {
        let why = q("error_description").unwrap_or_default();
        return Err(format!("consent refused ({error}): {}", excerpt(&why)));
    }
    q("code")
        .map(Some)
        .ok_or_else(|| "the redirect carries no code".into())
}

/// Wait on the loopback listener for the browser's redirect; answer it with a short page.
fn await_code(
    listener: &TcpListener,
    state: &str,
    issuer: &str,
    deadline: Instant,
) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listen: {}", e.kind()))?;
    loop {
        if Instant::now() >= deadline {
            return Err("no consent in time; run `zone mcp login` again".into());
        }
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(format!("listen: {}", e.kind())),
        };
        if let Some(result) = take_redirect(stream, state, issuer) {
            return result;
        }
    }
}

/// One browser request on the loopback port: None for anything but the redirect.
fn take_redirect(
    mut stream: TcpStream,
    state: &str,
    issuer: &str,
) -> Option<Result<String, String>> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    BufReader::new((&stream).take(8192))
        .read_line(&mut line)
        .ok()?;
    let mut words = line.split_whitespace();
    let target = match (words.next(), words.next()) {
        (Some("GET"), Some(t)) => t.to_owned(),
        _ => return None,
    };
    let result = callback_code(&target, state, issuer).transpose();
    let (status, text) = match &result {
        None => ("404 Not Found", "Not found."),
        Some(Ok(_)) => ("200 OK", "zone mcp is connected. You can close this tab."),
        Some(Err(_)) => (
            "400 Bad Request",
            "zone mcp did not connect; see the terminal.",
        ),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nconnection: close\r\n\r\n{text}",
        text.len()
    );
    result
}

fn post_form(agent: &ureq::Agent, url: &str, form: &[(&str, &str)]) -> Result<Value, String> {
    let (status, _, body) = read(answer(agent.post(url).send_form(form), url)?)?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if status != 200 {
        let error = v["error"].as_str().unwrap_or("unknown");
        let why = v["error_description"].as_str().unwrap_or("");
        return Err(format!("{error}: {}", excerpt(why)));
    }
    Ok(v)
}

/// `zone mcp login URL`: connect this state dir to the hosted zone MCP endpoint at URL.
pub fn login(
    state_dir: &Path,
    url: &str,
    port: Option<u16>,
    timeout: Duration,
) -> Result<(), String> {
    let mcp = Url::parse(url).map_err(|_| format!("not a URL: {url}"))?;
    if mcp.scheme() != "https"
        && !matches!(mcp.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
    {
        return Err("the MCP endpoint is https (http only on loopback)".into());
    }
    state::create(state_dir)?;
    let agent = agent();
    let d = discover(&agent, &mcp)?;

    let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
        .map_err(|e| format!("loopback port: {}", e.kind()))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("loopback port: {}", e.kind()))?
        .port();
    let redirect = format!("http://127.0.0.1:{bound}/callback");

    let (status, _, body) = read(answer(
        agent
            .post(&d.registration_endpoint)
            .set("content-type", "application/json")
            .send_string(
                &json!({
                "client_name": "zone CLI",
                "redirect_uris": [redirect],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
                })
                .to_string(),
            ),
        &d.registration_endpoint,
    )?)?;
    let registered: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let client_id = match (status, registered["client_id"].as_str()) {
        (200 | 201, Some(id)) => id.to_owned(),
        _ => {
            return Err(format!(
                "client registration refused (HTTP {status}): {}",
                excerpt(registered["error_description"].as_str().unwrap_or(""))
            ))
        }
    };

    let verifier = random_b64(32);
    let login_state = random_b64(16);
    let mut authorize = Url::parse(&d.authorization_endpoint)
        .map_err(|_| "bad authorization endpoint".to_owned())?;
    {
        let mut q = authorize.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("code_challenge", &pkce_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &login_state)
            .append_pair("resource", &d.resource);
        if let Some(scope) = &d.scope {
            q.append_pair("scope", scope);
        }
    }
    eprintln!(
        "zone mcp login: open this link in a browser on this machine. It shows the agent identity \
         to add to a zone; once the zone owner has added it, pick the zone and allow.\n\n{authorize}\n"
    );
    eprintln!(
        "zone mcp login: waiting up to {} min on 127.0.0.1:{bound}",
        timeout.as_secs().div_ceil(60)
    );
    let code = await_code(&listener, &login_state, &d.issuer, Instant::now() + timeout)?;

    let tokens = post_form(
        &agent,
        &d.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
            ("resource", &d.resource),
        ],
    )
    .map_err(|e| format!("token exchange refused ({e})"))?;
    let mut conn = Connection {
        resource: d.resource,
        token_endpoint: d.token_endpoint,
        client_id,
        access_token: String::new(),
        expires_at_ms: 0,
        refresh_token: None,
    };
    conn.take_tokens(&tokens)?;
    conn.save(state_dir)?;
    eprintln!("zone mcp login: connected to {}", conn.resource);
    Ok(())
}

/// The ids of the requests in a JSON-RPC message or batch (notifications and responses have none
/// to answer).
fn request_ids(msg: &Value) -> Vec<Value> {
    let one =
        |m: &Value| (m.get("method").is_some() && m.get("id").is_some()).then(|| m["id"].clone());
    match msg {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        m => one(m).into_iter().collect(),
    }
}

/// The JSON-RPC messages in a Server-Sent Events body: each event's `data` lines, joined.
fn sse_messages(body: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut data = String::new();
    for line in body.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !data.is_empty() {
                if let Ok(v) = serde_json::from_str(&data) {
                    out.push(v);
                }
                data.clear();
            }
        } else if let Some(d) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(d.strip_prefix(' ').unwrap_or(d));
        }
    }
    out
}

/// A body of JSON-RPC messages: one object or a batch.
fn json_messages(body: &str) -> Option<Vec<Value>> {
    match serde_json::from_str::<Value>(body).ok()? {
        Value::Array(items) => Some(items),
        v @ Value::Object(_) => Some(vec![v]),
        _ => None,
    }
}

struct Bridge {
    state: PathBuf,
    agent: ureq::Agent,
    conn: Connection,
    session: Option<String>,
    protocol: Option<String>,
    initialize_id: Option<Value>,
}

impl Bridge {
    fn refresh(&mut self) -> Result<(), String> {
        let refresh = self.conn.refresh_token.clone().ok_or(
            "the access token has expired and there is no refresh token; run `zone mcp login URL` again",
        )?;
        let tokens = post_form(
            &self.agent,
            &self.conn.token_endpoint.clone(),
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", &self.conn.client_id.clone()),
                ("resource", &self.conn.resource.clone()),
            ],
        )
        .map_err(|e| format!("the connection has ended ({e}); run `zone mcp login URL` again"))?;
        self.conn.take_tokens(&tokens)?;
        self.conn.save(&self.state)
    }

    fn send(&self, body: &str) -> Result<ureq::Response, String> {
        let mut req = self
            .agent
            .post(&self.conn.resource)
            .set("content-type", "application/json")
            .set("accept", "application/json, text/event-stream")
            .set(
                "authorization",
                &format!("Bearer {}", self.conn.access_token),
            );
        if let Some(p) = &self.protocol {
            req = req.set("mcp-protocol-version", p);
        }
        if let Some(s) = &self.session {
            req = req.set("mcp-session-id", s);
        }
        answer(req.send_string(body), "the zone MCP server")
    }

    /// POST one message; refresh and retry once on 401. Returns the server's messages.
    fn forward(&mut self, body: &str) -> Result<Vec<Value>, String> {
        if self.conn.expires_at_ms <= now_ms() + REFRESH_MARGIN_MS {
            self.refresh()?;
        }
        let mut res = self.send(body)?;
        if res.status() == 401 {
            self.refresh()?;
            res = self.send(body)?;
        }
        if let Some(s) = res.header("mcp-session-id") {
            self.session = Some(s.to_owned());
        }
        let (status, content_type, text) = read(res)?;
        if status == 202 || status == 204 {
            return Ok(vec![]);
        }
        let messages = if content_type == "text/event-stream" {
            Some(sse_messages(&text))
        } else {
            json_messages(&text)
        };
        match messages {
            Some(m) if (200..300).contains(&status) => Ok(m),
            // A JSON-RPC error the server sent with an HTTP error status is still its answer.
            Some(m) if m.iter().all(|v| v.get("jsonrpc").is_some()) && !m.is_empty() => Ok(m),
            _ if status == 401 => Err(format!(
                "the zone MCP server refused the token ({}); run `zone mcp login URL` again",
                excerpt(&text)
            )),
            _ => Err(format!("the zone MCP server answered HTTP {status}")),
        }
    }

    /// Note what later requests must carry: the protocol version `initialize` agreed on.
    fn observe(&mut self, msg: &Value) {
        if msg.get("method").and_then(Value::as_str) == Some("initialize") {
            self.initialize_id = msg.get("id").cloned();
        }
    }

    fn observe_reply(&mut self, reply: &Value) {
        if self.initialize_id.is_some() && reply.get("id") == self.initialize_id.as_ref() {
            if let Some(p) = reply["result"]["protocolVersion"].as_str() {
                self.protocol = Some(p.to_owned());
            }
            self.initialize_id = None;
        }
    }
}

/// The local tools: one per slash command in the SDK's registry, and the zone fs tools.
fn local_tools() -> Vec<Value> {
    let mut tools = slash_tools();
    tools.push(head_set_tool());
    #[cfg(unix)]
    tools.extend(crate::fs::mcp_tools());
    tools
}

fn slash_tools() -> Vec<Value> {
    commands::COMMANDS
        .iter()
        .map(|c| {
            let (props, required) = match c.name {
                "whois" => (
                    json!({"member": {"type": "string", "description": "A pseudonym, a pseudonym prefix of 6 or more characters, or a name or its start"}}),
                    json!(["member"]),
                ),
                "price" => (
                    json!({"capability": {"type": "string", "description": "A capability kind, id or tag"}}),
                    json!(["capability"]),
                ),
                "note" => (
                    json!({"text": {"type": "string", "description": "The note to post"}}),
                    json!(["text"]),
                ),
                "onboard" => (
                    json!({"answer": {"type": "string", "description": "The member's own answer to the current step, word for word: text, skip or skip all for a question; yes or no for the draft. Omit to start the questions again. Never answer for the member."}}),
                    json!([]),
                ),
                "profile" => (
                    json!({"off": {"type": "boolean", "description": "Withdraw the member's profile"}}),
                    json!([]),
                ),
                "rep" => (
                    json!({"off": {"type": "boolean", "description": "Revoke this device's zone rep session now"}}),
                    json!([]),
                ),
                "head" => (
                    json!({"member": {"type": "string", "description": "A pseudonym, a pseudonym prefix of 6 or more characters, or a name or its start; leave out for your own head"}}),
                    json!([]),
                ),
                _ => (json!({}), json!([])),
            };
            // These write in the member's name: a note, a profile on yes, a withdrawal.
            let writes = matches!(c.name, "note" | "onboard" | "profile" | "rep");
            json!({
                "name": format!("zone_{}", c.name),
                "description": format!(
                    "{} ({}), from this machine's joined zone device{}.",
                    c.summary,
                    c.usage,
                    if c.core { "" } else { "; reads local state only" }
                ),
                "inputSchema": {"type": "object", "properties": props, "required": required},
                "annotations": {"readOnlyHint": !writes, "openWorldHint": c.core},
            })
        })
        .collect()
}

/// `zone_head_set`: set this member's head through the running `zone run`, as `zone head` does.
fn head_set_tool() -> Value {
    let text = |d: &str| json!({"type": "string", "description": d});
    json!({
        "name": "zone_head_set",
        "description": "Set what you are working on now (your head), sent sealed to every zone member over the running `zone run` when it changes; never to the zone service. Fields left out keep their value. Call it when your task or state changes, not on every turn.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "state": {"type": "string", "enum": ["working", "blocked", "waiting", "idle"]},
                "task": text("The task board id you are on"),
                "focus": text("One line: what you are doing now"),
                "next": text("One line: the next step"),
                "blocked_on": text("One line: what blocks you (with state blocked)"),
                "private": {"type": "boolean", "description": "Members see only busy or idle"},
                "reset": {"type": "boolean", "description": "Start from an empty head"},
            },
            "required": [],
        },
        "annotations": {"readOnlyHint": false, "openWorldHint": false},
    })
}

/// A `zone_head_set` call's result.
fn head_set_call(state: &Path, args: &Value) -> Value {
    let mut req = json!({});
    for k in ["state", "task", "focus", "next", "blocked_on"] {
        if let Some(v) = args[k].as_str() {
            req[k] = json!(v);
        }
    }
    match args["private"].as_bool() {
        Some(true) => req["private"] = json!(true),
        Some(false) => req["public"] = json!(true),
        None => {}
    }
    req["reset"] = json!(args["reset"].as_bool() == Some(true));
    match crate::head::set_value(state, &req) {
        Ok(r) => json!({
            "content": [{"type": "text", "text": bp_zone_core::head::line(&head_of(&r["head"]))}],
            "structuredContent": r,
            "isError": false,
        }),
        Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
    }
}

/// The head in `zone run`'s answer, back as the SDK's record (for its one-line form).
fn head_of(v: &Value) -> bp_zone_core::head::Head {
    let text = |k: &str| v[k].as_str().map(str::to_owned);
    bp_zone_core::head::Head {
        member_id: text("memberId").unwrap_or_default(),
        name: text("name").unwrap_or_default(),
        me: true,
        fingerprint: String::new(),
        state: text("state").unwrap_or_default(),
        private: v["private"].as_bool().unwrap_or(false),
        task_id: text("taskId"),
        focus: text("focus"),
        next: text("next"),
        blocked_on: text("blockedOn"),
        updated_ms: v["updatedMs"].as_u64().unwrap_or(0),
        seen_ms: v["seenMs"].as_u64().unwrap_or(0),
    }
}

/// The command line a local tool call stands for, or None when it is not a local tool.
fn local_line(params: &Value) -> Option<String> {
    let name = params["name"].as_str()?.strip_prefix("zone_")?;
    let spec = commands::spec(name)?;
    let arg = match spec.name {
        "whois" | "head" => params["arguments"]["member"].as_str().unwrap_or_default(),
        "price" => params["arguments"]["capability"]
            .as_str()
            .unwrap_or_default(),
        "note" => params["arguments"]["text"].as_str().unwrap_or_default(),
        "onboard" => params["arguments"]["answer"].as_str().unwrap_or_default(),
        "profile" | "rep" if params["arguments"]["off"] == json!(true) => "off",
        _ => "",
    };
    Some(format!("/{} {arg}", spec.name).trim_end().to_owned())
}

/// A local tool's result: the text, and the structured answer.
fn local_call(state: &Path, line: &str) -> Value {
    match slash::answer(state, line) {
        Ok(r) => json!({
            "content": [{"type": "text", "text": r.text}],
            "structuredContent": r.json,
            "isError": !r.ok,
        }),
        Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
    }
}

/// Whether `state` holds a device that joined a zone.
fn has_joined_device(state: &Path) -> bool {
    device::open_existing(state, std::sync::Arc::new(device::Offline))
        .ok()
        .flatten()
        .is_some_and(|d| d.status().zone_id.is_some())
}

/// What the bridge answers itself: a local tool call, and, with no hosted server, the rest of
/// the protocol. None: forward it.
fn answer_locally(state: &Path, msg: &Value, hosted: bool) -> Option<Option<Value>> {
    let method = msg.get("method")?.as_str()?;
    let id = msg.get("id").cloned();
    if method == "tools/call" {
        #[cfg(unix)]
        if let Some(result) = crate::fs::mcp_call(state, &msg["params"]) {
            return Some(id.map(|id| json!({"jsonrpc": "2.0", "id": id, "result": result})));
        }
        if msg["params"]["name"] == json!("zone_head_set") {
            let result = head_set_call(state, &msg["params"]["arguments"]);
            return Some(id.map(|id| json!({"jsonrpc": "2.0", "id": id, "result": result})));
        }
        if let Some(line) = local_line(&msg["params"]) {
            let result = local_call(state, &line);
            return Some(id.map(|id| json!({"jsonrpc": "2.0", "id": id, "result": result})));
        }
    }
    if hosted {
        return None;
    }
    let Some(id) = id else {
        return Some(None); // a notification
    };
    let result = match method {
        "initialize" => json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "zone", "version": env!("CARGO_PKG_VERSION")},
        }),
        "tools/list" => json!({"tools": local_tools()}),
        "ping" => json!({}),
        "tools/call" => {
            return Some(Some(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "unknown tool"}}),
            ))
        }
        _ => {
            return Some(Some(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}),
            ))
        }
    };
    Some(Some(json!({"jsonrpc": "2.0", "id": id, "result": result})))
}

fn write_line(out: &mut impl Write, v: &Value) -> Result<(), String> {
    writeln!(out, "{v}")
        .and_then(|()| out.flush())
        .map_err(|e| format!("stdout: {}", e.kind()))
}

fn bridge_error(id: Value, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": BRIDGE_ERROR, "message": message}})
}

/// `zone mcp`: newline-delimited JSON-RPC on stdin and stdout, until stdin ends. A request the
/// bridge cannot deliver gets a JSON-RPC error naming why; stderr stays quiet for the agent.
pub fn serve(state_dir: &Path) -> Result<(), String> {
    let joined = has_joined_device(state_dir);
    let conn = match Connection::load(state_dir) {
        Ok(c) => Some(c),
        Err(_) if joined => None,
        Err(e) => return Err(e),
    };
    state::check(state_dir)?;
    let mut bridge = conn.map(|conn| Bridge {
        state: state_dir.to_owned(),
        agent: agent(),
        conn,
        session: None,
        protocol: None,
        initialize_id: None,
    });
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("stdin: {}", e.kind()))?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            write_line(
                &mut out,
                &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}),
            )?;
            continue;
        };
        if joined {
            if let Some(reply) = answer_locally(state_dir, &msg, bridge.is_some()) {
                if let Some(r) = reply {
                    write_line(&mut out, &r)?;
                }
                continue;
            }
        }
        let Some(bridge) = bridge.as_mut() else {
            continue;
        };
        bridge.observe(&msg);
        // The local tools join the hosted list's first page.
        let list_id =
            (joined && msg["method"] == "tools/list" && msg["params"]["cursor"].is_null())
                .then(|| msg["id"].clone());
        match bridge.forward(&line) {
            Ok(replies) => {
                for r in &replies {
                    bridge.observe_reply(r);
                    let mut r = r.clone();
                    if list_id.as_ref().is_some_and(|id| r["id"] == *id) {
                        if let Some(tools) = r["result"]["tools"].as_array_mut() {
                            tools.extend(local_tools());
                        }
                    }
                    write_line(&mut out, &r)?;
                }
            }
            Err(e) => {
                for id in request_ids(&msg) {
                    write_line(&mut out, &bridge_error(id, &format!("zone mcp: {e}")))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_is_unpadded_base64url_of_sha256() {
        // `printf %s VERIFIER | openssl dgst -sha256 -binary | base64 | tr '+/' '-_' | tr -d =`
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mJ92ZzQ0Ve6lSO8BXH-uLmI8Ip-y5o"),
            "YSHEINy5u1N7fwPcG8BLQyVffBp84kxUhrKmOEOFIzI"
        );
    }

    #[test]
    fn the_challenge_names_the_resource_metadata() {
        let c = r#"Bearer error="invalid_token", resource_metadata="https://z.example/.well-known/oauth-protected-resource/mcp""#;
        assert_eq!(
            resource_metadata_param(c).as_deref(),
            Some("https://z.example/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(resource_metadata_param("Bearer realm=x"), None);
    }

    #[test]
    fn well_known_goes_between_host_and_path() {
        let mcp = Url::parse("https://z.example/mcp").unwrap();
        assert_eq!(
            well_known(&mcp, "oauth-protected-resource"),
            "https://z.example/.well-known/oauth-protected-resource/mcp"
        );
        let issuer = Url::parse("https://z.example").unwrap();
        assert_eq!(
            well_known(&issuer, "oauth-authorization-server"),
            "https://z.example/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn the_callback_gives_the_code_only_for_this_login() {
        let iss = "http://127.0.0.1:9097";
        assert_eq!(
            callback_code(
                "/callback?code=c1&state=s1&iss=http%3A%2F%2F127.0.0.1%3A9097",
                "s1",
                iss
            ),
            Ok(Some("c1".into()))
        );
        assert_eq!(callback_code("/favicon.ico", "s1", iss), Ok(None));
        assert!(callback_code("/callback?code=c1&state=other", "s1", iss).is_err());
        assert!(callback_code(
            "/callback?code=c1&state=s1&iss=https%3A%2F%2Fevil",
            "s1",
            iss
        )
        .is_err());
        let denied = callback_code("/callback?error=access_denied&state=s1", "s1", iss);
        assert!(denied.unwrap_err().contains("access_denied"));
    }

    #[test]
    fn only_requests_are_answered_on_failure() {
        let batch = json!([
            {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 7, "result": {}},
        ]);
        assert_eq!(request_ids(&batch), vec![json!(1)]);
        assert!(request_ids(&json!({"jsonrpc": "2.0", "method": "x"})).is_empty());
    }

    #[test]
    fn sse_events_become_messages() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\ndata: \"result\":{}}\n\n: comment\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"n\"}\n";
        // The second data line of the first event continues the JSON.
        let msgs = sse_messages(body);
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[0]["id"], 1);
        assert_eq!(msgs[1]["method"], "n");
    }

    #[cfg(unix)]
    #[test]
    fn the_connection_round_trips_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        state::restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zone");
        state::create(&dir).unwrap();
        let mut c = Connection {
            resource: "https://z.example/mcp".into(),
            token_endpoint: "https://z.example/token".into(),
            client_id: "bpzc_x".into(),
            access_token: String::new(),
            expires_at_ms: 0,
            refresh_token: Some("r0".into()),
        };
        c.take_tokens(&json!({"access_token": "a1", "expires_in": 600, "refresh_token": "r1"}))
            .unwrap();
        assert!(c.expires_at_ms > now_ms() + 590_000);
        c.save(&dir).unwrap();
        assert_eq!(Connection::load(&dir).unwrap(), c);
        let mode = std::fs::metadata(path(&dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        state::check(&dir).unwrap();
    }

    #[test]
    fn every_command_is_a_local_tool_and_back() {
        let tools = slash_tools();
        assert_eq!(tools.len(), commands::COMMANDS.len());
        assert_eq!(
            local_tools().len(),
            commands::COMMANDS.len() + 1 + 4,
            "plus zone_head_set and the zone fs tools"
        );
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["member"]));
        let line = |name: &str, args: Value| local_line(&json!({"name": name, "arguments": args}));
        assert_eq!(line("zone_who", json!({})).as_deref(), Some("/who"));
        assert_eq!(
            line("zone_whois", json!({"member": "Aly Bot"})).as_deref(),
            Some("/whois Aly Bot")
        );
        assert_eq!(
            line("zone_price", json!({"capability": "inference"})).as_deref(),
            Some("/price inference")
        );
        assert_eq!(
            line("zone_onboard", json!({"answer": "skip all"})).as_deref(),
            Some("/onboard skip all")
        );
        assert_eq!(line("zone_onboard", json!({})).as_deref(), Some("/onboard"));
        assert_eq!(
            line("zone_profile", json!({"off": true})).as_deref(),
            Some("/profile off")
        );
        assert_eq!(line("zone_profile", json!({})).as_deref(), Some("/profile"));
        assert_eq!(
            line("zone_note", json!({"text": "hi"})).as_deref(),
            Some("/note hi")
        );
        // A tool that can publish in the member's name is not marked read-only.
        for t in &tools {
            let writes = ["zone_note", "zone_onboard", "zone_profile", "zone_rep"]
                .contains(&t["name"].as_str().unwrap());
            assert_eq!(
                t["annotations"]["readOnlyHint"],
                json!(!writes),
                "{}",
                t["name"]
            );
        }
        assert_eq!(
            line("zone_head", json!({"member": "Aly"})).as_deref(),
            Some("/head Aly")
        );
        assert_eq!(line("zone_head", json!({})).as_deref(), Some("/head"));
        assert_eq!(line("zone_heads", json!({})).as_deref(), Some("/heads"));
        assert_eq!(
            line("zone_rep", json!({"off": true})).as_deref(),
            Some("/rep off")
        );
        assert_eq!(line("zone_rep", json!({})).as_deref(), Some("/rep"));
        assert_eq!(line("zone_join", json!({})), None, "a hosted tool goes on");
    }

    #[test]
    fn without_a_hosted_server_the_bridge_speaks_for_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}});
        let r = answer_locally(tmp.path(), &init, false).unwrap().unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let r = answer_locally(tmp.path(), &list, false).unwrap().unwrap();
        assert_eq!(
            r["result"]["tools"].as_array().unwrap().len(),
            commands::COMMANDS.len() + 1 + 4
        );
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(answer_locally(tmp.path(), &note, false), Some(None));
        // With a hosted server, only the local tools' calls stay here.
        assert_eq!(answer_locally(tmp.path(), &list, true), None);
        let call = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "zone_help", "arguments": {}}});
        let r = answer_locally(tmp.path(), &call, true).unwrap().unwrap();
        assert_eq!(r["id"], 3);
    }

    #[test]
    fn serve_without_login_says_what_to_do() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Connection::load(tmp.path()).unwrap_err();
        assert!(err.contains("zone mcp login"), "{err}");
    }
}
