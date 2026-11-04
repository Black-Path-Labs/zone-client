//! Notes and files between zone members: `zone note`, `zone notes`, `zone send-file`,
//! `zone files`, `zone get` and `zone subscribe`. The SDK seals each note and file to the
//! devices of the members it is addressed to (the whole zone, or 1 to 32 pseudonyms, plus the
//! sender's own devices) before the zone service sees it, and checks a received file's size and
//! SHA-256 against the sender's signed header before it marks it complete. Above 32 MiB a file
//! goes as a resumable large-file transfer. `zone notes` and `zone files` list only the quiet
//! inbox: what is addressed to this member or sent by it, and zone-wide items only when
//! subscribed. This module reads input, checks limits before any network call, and prints one
//! JSON object per line. Nothing here logs note text, file names or bytes.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bp_zone_core::{Item, ItemKind, ZoneDevice, ZoneError, MAX_FILE_BYTES, MAX_FILE_V2_BYTES};
use serde_json::{json, Value};

use crate::device;
use crate::join;
use crate::record::SyncRecord;
use crate::transport::Http;

/// The largest note, in bytes of UTF-8 text. Core refuses a sealed message over 64 KiB; this
/// leaves room for the note's framing and sealing.
pub const MAX_NOTE_BYTES: usize = 32 * 1024;
/// The most members one direct or group note or file names.
pub const MAX_RECIPIENTS: usize = 32;
/// The mime type of a file sent without `--mime`.
const DEFAULT_MIME: &str = "application/octet-stream";

/// The joined device, over HTTP. Its roster is refreshed by one sync first, so what it posts is
/// sealed to the zone's current devices; a failed sync is reported and the command goes on.
pub fn joined(state: &Path, cmd: &str) -> Result<Arc<ZoneDevice>, String> {
    let device = device::open_existing(state, Arc::new(Http::default()))?
        .filter(|d| d.status().zone_id.is_some())
        .ok_or("no zone joined: run `zone join` first")?;
    sync_once(state, &device, cmd);
    Ok(device)
}

/// One sync that does not wait, recorded for `zone status` and the roster reads. A failure
/// prints its kind, never Core's text.
fn sync_once(state: &Path, device: &ZoneDevice, cmd: &str) {
    let synced = device.sync(0);
    let mut record = SyncRecord::load(state);
    record.synced(synced.as_ref().map(|_| ()));
    if let Err(e) = record.save(state) {
        eprintln!("zone {cmd}: {e}");
    }
    if let Err(e) = synced {
        eprintln!(
            "zone {cmd}: sync failed ({}); showing local state",
            join::kind(&e)
        );
    }
}

pub fn sdk_error(cmd: &str, e: &ZoneError) -> String {
    match e {
        ZoneError::NotAdmitted => format!("{cmd}: not admitted yet; `zone status` shows the state"),
        ZoneError::TooLarge { size, limit } => {
            format!("{cmd}: {size} bytes is over the {limit}-byte limit")
        }
        ZoneError::NotAMember { member } => format!(
            "{cmd}: refused (not-a-member): {member} is not a member of the zone; nothing was sent"
        ),
        ZoneError::RelayQuota { need, have } => format!(
            "{cmd}: refused (relay-quota): the relay needs {need} bytes (the file once per \
             recipient device) and has {have} free; nothing was sent"
        ),
        e => format!("{cmd}: refused ({})", join::kind(e)),
    }
}

/// The pseudonyms of `--to`, checked before any network call: none means the whole zone; at
/// most [`MAX_RECIPIENTS`], each named once.
pub fn recipients(cmd: &str, to: Vec<String>) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for p in to {
        let p = p.trim();
        if p.is_empty() {
            return Err(format!("{cmd}: a pseudonym is empty"));
        }
        if !out.iter().any(|q| q == p) {
            out.push(p.to_owned());
        }
    }
    if out.len() > MAX_RECIPIENTS {
        return Err(format!(
            "{cmd}: {} recipients is over the {MAX_RECIPIENTS}-recipient limit",
            out.len()
        ));
    }
    Ok(out)
}

/// A note's text, checked before any network call.
pub fn note_text(raw: Vec<u8>) -> Result<String, String> {
    let text = String::from_utf8(raw).map_err(|_| "note: the note is not UTF-8")?;
    if text.trim().is_empty() {
        return Err("note: the note is empty".into());
    }
    if text.len() > MAX_NOTE_BYTES {
        return Err(format!(
            "note: {} bytes is over the {MAX_NOTE_BYTES}-byte note limit; send it as a file",
            text.len()
        ));
    }
    Ok(text)
}

/// One item as the JSON line the list commands print.
pub fn item_line(i: &Item) -> Value {
    let mut v = json!({
        "id": i.id,
        "author": i.author_id,
        "name": i.author,
        "ts_ms": i.ts_ms,
        "mine": i.mine,
        "to": if i.to.is_empty() { json!("zone") } else { json!(i.to) },
    });
    match i.kind {
        ItemKind::Note => v["text"] = json!(i.text),
        ItemKind::File => {
            v["file"] = json!(i.text);
            v["mime"] = json!(i.mime);
            v["size"] = json!(i.size);
            v["complete"] = json!(i.complete);
        }
    }
    v
}

/// The items of one kind newer than `since`, oldest first.
pub fn select(items: Vec<Item>, kind: ItemKind, since: Option<i64>) -> Vec<Item> {
    items
        .into_iter()
        .filter(|i| i.kind == kind && since.is_none_or(|t| i.ts_ms > t))
        .collect()
}

/// `zone note`: `text` is the argument, or None to read stdin; `to` the recipients, or none for
/// the whole zone.
pub fn note(state: &Path, text: Option<String>, to: Vec<String>) -> Result<(), String> {
    let to = recipients("note", to)?;
    let raw = match text {
        Some(t) => t.into_bytes(),
        None => {
            let mut buf = Vec::new();
            std::io::stdin()
                .take(MAX_NOTE_BYTES as u64 + 1)
                .read_to_end(&mut buf)
                .map_err(|e| format!("note: reading stdin: {e}"))?;
            buf
        }
    };
    let text = note_text(raw)?;
    let device = joined(state, "note")?;
    let item = if to.is_empty() {
        device.add_note(text)
    } else {
        device.add_note_to(to, text)
    }
    .map_err(|e| sdk_error("note", &e))?;
    println!("{}", item_line(&item));
    Ok(())
}

/// `zone notes` and `zone files`: the inbox's items of one kind. With `wait`, print what arrives
/// within that many seconds instead (with `since`, what is newer, and wait only when nothing
/// is); the wait ends only for an item that passes the quiet filter.
pub fn list(
    state: &Path,
    kind: ItemKind,
    since: Option<i64>,
    wait: Option<u32>,
) -> Result<(), String> {
    let cmd = match kind {
        ItemKind::Note => "notes",
        ItemKind::File => "files",
    };
    let device = joined(state, cmd)?;
    let mut items = select(device.inbox(), kind.clone(), since);
    if let Some(secs) = wait.filter(|_| since.is_none() || items.is_empty()) {
        // Only items that arrive from now on end the wait; ids, since clocks differ.
        let seen: HashSet<String> = device.inbox().into_iter().map(|i| i.id).collect();
        let deadline = Instant::now() + Duration::from_secs(secs.into());
        items = Vec::new();
        while items.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let left = u32::try_from(left.as_secs().max(1)).unwrap_or(u32::MAX);
            if device
                .wait_for_inbox(left)
                .map_err(|e| sdk_error(cmd, &e))?
            {
                items = select(device.inbox(), kind.clone(), since)
                    .into_iter()
                    .filter(|i| !seen.contains(&i.id))
                    .collect();
            }
        }
    }
    let mut out = std::io::stdout().lock();
    for i in items {
        let mut line = item_line(&i);
        if i.kind == ItemKind::File && i.mine {
            line["receipts"] = receipts_line(&device, &i.id);
        }
        writeln!(out, "{line}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The signed receipts this device got for a file it sent: one per receiving member.
fn receipts_line(device: &ZoneDevice, id: &str) -> Value {
    device
        .file_receipts(id.to_owned())
        .iter()
        .map(|r| json!({"member": r.member, "received_at": r.received_at}))
        .collect()
}

/// `zone subscribe`: `Some(true)` subscribes to zone-wide notes and files, `Some(false)` turns
/// that off, None only shows. A device setting, kept locally; prints the subscriptions.
pub fn subscribe(state: &Path, on: Option<bool>) -> Result<(), String> {
    let device = device::open_existing(state, Arc::new(device::Offline))?
        .ok_or("no zone joined: run `zone join` first")?;
    if let Some(on) = on {
        let subs = if on { vec!["zone".to_owned()] } else { vec![] };
        device
            .set_subscriptions(subs)
            .map_err(|e| sdk_error("subscribe", &e))?;
    }
    println!("{}", json!({"subscriptions": device.subscriptions()}));
    Ok(())
}

/// A file to send: refused over the SDK's limit from its metadata, before it is read.
pub fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let meta = fs::metadata(path).map_err(|e| format!("send-file: {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("send-file: {} is not a file", path.display()));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "send-file: {} bytes is over the {MAX_FILE_BYTES}-byte file limit",
            meta.len()
        ));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    fs::File::open(path)
        .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("send-file: {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "send-file: {} grew past the {MAX_FILE_BYTES}-byte file limit",
            path.display()
        ));
    }
    Ok(bytes)
}

/// The name a file is sent under: `--name`, else the path's file name.
pub fn file_name(path: &Path, name: Option<String>) -> Result<String, String> {
    match name {
        Some(n) if !n.trim().is_empty() => Ok(n),
        Some(_) => Err("send-file: --name is empty".into()),
        None => path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                format!(
                    "send-file: {} has no file name; pass --name",
                    path.display()
                )
            }),
    }
}

/// The size of a file to send, from its metadata, before it is read.
fn file_size(path: &Path) -> Result<u64, String> {
    let meta = fs::metadata(path).map_err(|e| format!("send-file: {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("send-file: {} is not a file", path.display()));
    }
    Ok(meta.len())
}

/// `zone send-file`: through the mailbox up to 32 MiB, as a resumable large file up to 1 GiB.
/// `to` the recipients, or none for the whole zone.
pub fn send_file(
    state: &Path,
    path: &Path,
    name: Option<String>,
    mime: Option<String>,
    to: Vec<String>,
) -> Result<(), String> {
    let name = file_name(path, name)?;
    let to = recipients("send-file", to)?;
    let mime = mime.unwrap_or_else(|| DEFAULT_MIME.into());
    let size = file_size(path)?;
    if size > MAX_FILE_V2_BYTES {
        return Err(format!(
            "send-file: {size} bytes is over the {MAX_FILE_V2_BYTES}-byte file limit; send it \
             to one member with --live PSEUDONYM"
        ));
    }
    if size > MAX_FILE_BYTES {
        let path = path
            .to_str()
            .ok_or_else(|| format!("send-file: {} is not a UTF-8 path", path.display()))?;
        let device = joined(state, "send-file")?;
        let item = device
            .send_file_v2(to, name, mime, path.to_owned())
            .map_err(|e| sdk_error("send-file", &e))?;
        println!("{}", item_line(&item));
        return Ok(());
    }
    let bytes = read_file(path)?;
    let sha256 = bp_workspace_core::sha256_tagged(&bytes);
    let device = joined(state, "send-file")?;
    let item = if to.is_empty() {
        device.add_file(name, mime, bytes)
    } else {
        device.add_file_to(to, name, mime, bytes)
    }
    .map_err(|e| sdk_error("send-file", &e))?;
    let mut line = item_line(&item);
    line["sha256"] = json!(sha256);
    println!("{line}");
    Ok(())
}

/// Write `bytes` to `target` whole or not at all: a 0600 temporary file in the same directory,
/// synced, then renamed over the target. An existing target is refused unless `force`.
pub fn write_atomic(target: &Path, bytes: &[u8], force: bool) -> Result<(), String> {
    if !force && fs::symlink_metadata(target).is_ok() {
        return Err(format!(
            "get: {} exists; pass --force to replace it",
            target.display()
        ));
    }
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let base = target
        .file_name()
        .ok_or_else(|| format!("get: {} names no file", target.display()))?;
    let mut nonce = [0u8; 8];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
    let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = dir.join(format!(".{}.{hex}.part", base.to_string_lossy()));
    let write = || -> std::io::Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, target)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("get: {}: {e}", target.display())
    })
}

/// After a complete write: sign the file receipt and send it to the sender alone, unless
/// `want` is false or the file is this member's own. `sent`, `skipped` or `failed`.
fn send_receipt(device: &ZoneDevice, id: &str, want: bool) -> &'static str {
    if !want || device.items().iter().any(|i| i.id == id && i.mine) {
        return "skipped";
    }
    match device.sign_file_receipt(id.to_owned()) {
        Ok(_) => "sent",
        Err(e) => {
            eprintln!(
                "zone get: the file receipt was not sent ({})",
                join::kind(&e)
            );
            "failed"
        }
    }
}

/// `zone get`: `out` is a path, or `-` for stdout. A large file is received into a spool next
/// to `out` and a second run resumes it; `receipt` sends the file receipt once it is written.
pub fn get(state: &Path, id: &str, out: &Path, force: bool, receipt: bool) -> Result<(), String> {
    // Errors name the id, as the hint below does.
    let cmd = format!("get {id}");
    let device = joined(state, &cmd)?;
    let to_stdout = out == Path::new("-");
    if !to_stdout {
        if !force && fs::symlink_metadata(out).is_ok() {
            return Err(format!(
                "get: {} exists; pass --force to replace it",
                out.display()
            ));
        }
        let path = out
            .to_str()
            .ok_or_else(|| format!("get: {} is not a UTF-8 path", out.display()))?;
        match device.receive_file_v2(id.to_owned(), path.to_owned()) {
            Ok(st) => {
                let mut line = json!({
                    "id": id,
                    "path": out.display().to_string(),
                    "size": st.size,
                    "sha256": st.sha256,
                    "complete": st.complete,
                    "chunks": st.chunk_count,
                    "missing_chunks": st.missing_chunks,
                });
                if !st.complete {
                    println!("{line}");
                    return Err(format!(
                        "get: {id}: {} of {} chunks have not arrived yet; run `zone get` again \
                         to resume",
                        st.missing_chunks, st.chunk_count
                    ));
                }
                line["receipt"] = json!(send_receipt(&device, id, receipt));
                println!("{line}");
                return Ok(());
            }
            // Not a large file: it comes whole from the device store, below.
            Err(ZoneError::NotFound) => {}
            Err(e) => return Err(sdk_error(&cmd, &e)),
        }
    }
    let bytes = device.file_bytes(id.to_owned()).map_err(|e| match e {
        ZoneError::NotFound => format!(
            "get: {id}: not found or not complete; run `zone files` (a file over 32 MiB needs \
             -o PATH)"
        ),
        e => sdk_error(&cmd, &e),
    })?;
    if to_stdout {
        let mut o = std::io::stdout().lock();
        o.write_all(&bytes)
            .and_then(|()| o.flush())
            .map_err(|e| format!("get: stdout: {e}"))?;
    } else {
        write_atomic(out, &bytes, force)?;
    }
    let line = json!({
        "id": id,
        "path": if to_stdout { "-".into() } else { out.display().to_string() },
        "size": bytes.len(),
        "sha256": bp_workspace_core::sha256_tagged(&bytes),
        "receipt": send_receipt(&device, id, receipt),
    });
    if to_stdout {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: ItemKind, ts_ms: i64) -> Item {
        Item {
            id: format!("id-{ts_ms}"),
            kind,
            author: "Grok".into(),
            author_id: "bp:self:abc".into(),
            text: "hello".into(),
            mime: None,
            size: 5,
            ts_ms,
            mine: false,
            complete: true,
            to: vec![],
        }
    }

    #[test]
    fn a_note_is_non_empty_utf8_within_the_limit() {
        assert_eq!(note_text(b"hi\n".to_vec()).unwrap(), "hi\n");
        assert!(note_text(b" \n\t".to_vec()).unwrap_err().contains("empty"));
        assert!(note_text(vec![0xff, 0xfe]).unwrap_err().contains("UTF-8"));
        assert!(note_text(vec![b'a'; MAX_NOTE_BYTES]).is_ok());
        let err = note_text(vec![b'a'; MAX_NOTE_BYTES + 1]).unwrap_err();
        assert!(err.contains("32768") && err.contains("as a file"), "{err}");
    }

    #[test]
    fn note_and_file_lines_carry_their_own_fields() {
        let n = item_line(&item(ItemKind::Note, 7));
        assert_eq!(
            n,
            json!({"id": "id-7", "author": "bp:self:abc", "name": "Grok", "ts_ms": 7,
                   "mine": false, "to": "zone", "text": "hello"})
        );
        let mut f = item(ItemKind::File, 8);
        f.mime = Some("text/plain".into());
        f.complete = false;
        assert_eq!(
            item_line(&f),
            json!({"id": "id-8", "author": "bp:self:abc", "name": "Grok", "ts_ms": 8,
                   "mine": false, "to": "zone", "file": "hello", "mime": "text/plain", "size": 5,
                   "complete": false})
        );
    }

    #[test]
    fn select_keeps_one_kind_newer_than_since_in_order() {
        let all = vec![
            item(ItemKind::Note, 1),
            item(ItemKind::File, 2),
            item(ItemKind::Note, 3),
            item(ItemKind::Note, 4),
        ];
        let ts = |v: Vec<Item>| v.iter().map(|i| i.ts_ms).collect::<Vec<_>>();
        assert_eq!(ts(select(all.clone(), ItemKind::Note, None)), [1, 3, 4]);
        assert_eq!(ts(select(all.clone(), ItemKind::Note, Some(3))), [4]);
        assert_eq!(ts(select(all, ItemKind::File, Some(0))), [2]);
    }

    #[test]
    fn a_file_over_the_limit_is_refused_before_it_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let big = tmp.path().join("big.bin");
        fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let err = read_file(&big).unwrap_err();
        assert!(err.contains("33554433") && err.contains("limit"), "{err}");
        assert!(read_file(tmp.path()).unwrap_err().contains("not a file"));
        let ok = tmp.path().join("ok.bin");
        fs::write(&ok, b"abc").unwrap();
        assert_eq!(read_file(&ok).unwrap(), b"abc");
    }

    #[test]
    fn a_file_is_named_after_its_path_unless_given() {
        let p = Path::new("/tmp/report.pdf");
        assert_eq!(file_name(p, None).unwrap(), "report.pdf");
        assert_eq!(file_name(p, Some("q3.pdf".into())).unwrap(), "q3.pdf");
        assert!(file_name(p, Some(" ".into())).is_err());
        assert!(file_name(Path::new("/"), None)
            .unwrap_err()
            .contains("--name"));
    }

    #[test]
    fn get_writes_whole_owner_only_files_and_keeps_existing_ones() {
        crate::state::restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("out.bin");
        write_atomic(&target, b"first", false).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let err = write_atomic(&target, b"second", false).unwrap_err();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(fs::read(&target).unwrap(), b"first");
        write_atomic(&target, b"second", true).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");
        let left: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file left behind");
    }
}
