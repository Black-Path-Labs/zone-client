//! The name and skills this seat advertises to its zone (`zone join --manifest FILE` or
//! `--name NAME`, `zone rename NAME`). They are kept in `<state>/agent.json` and handed to the
//! SDK (`set_agent_profile`) on every later command, so editing that file and running any command
//! changes what the next sync publishes.
//!
//! A manifest is exactly `{"name":"Aly","capabilities":["Ticket triage"]}`: a name and its
//! skills, each nonempty printable text of at most 64 bytes, at most 32 distinct skills. An empty
//! list is deliberate and withdraws the seat's skills. After admission the SDK publishes each
//! skill as an unpriced, zone-wide discovery entry and seals the name and labels to the zone's
//! members. A skill is a description: it grants nobody access to anything.
//!
//! A seat given neither (a plain `zone join`, or one joined before the CLI published) advertises
//! a default the first time a command opens it: its kit agent id or roster name, else `Agent`
//! and its fingerprint, with the CLI's own skill. `zone rename NAME` gives it a real name.

use std::path::{Path, PathBuf};

use bp_workspace_core::agent_join::{AgentProfile, DEFAULT_AGENT_ID};
use bp_zone_core::ZoneDevice;

/// The skill the CLI itself supports, advertised when a name is given without a manifest.
pub const DEFAULT_CAPABILITIES: [&str; 1] = ["Zone notes and files"];
/// A manifest is a name and at most 32 short labels: far below this.
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
const FILE: &str = "agent.json";

pub fn path(state: &Path) -> PathBuf {
    state.join(FILE)
}

/// Parse and check a manifest's text.
pub fn parse(text: &[u8], source: &str) -> Result<AgentProfile, String> {
    let value: serde_json::Value =
        serde_json::from_slice(text).map_err(|_| format!("{source}: not JSON"))?;
    AgentProfile::from_value(&value).map_err(|_| {
        format!(
            "{source}: want exactly {{\"name\":…,\"capabilities\":[…]}}, a name and at most 32 \
             distinct skills, each 1 to 64 bytes of printable text"
        )
    })
}

/// The profile `--name NAME` makes: that name and the CLI's own skill.
pub fn named(name: &str) -> Result<AgentProfile, String> {
    with_name(
        AgentProfile {
            name: String::new(),
            capabilities: DEFAULT_CAPABILITIES.map(str::to_owned).to_vec(),
        },
        name,
    )
}

/// `profile` under a new name, its skills unchanged.
pub fn with_name(profile: AgentProfile, name: &str) -> Result<AgentProfile, String> {
    let renamed = AgentProfile {
        name: name.to_owned(),
        ..profile
    };
    renamed
        .check()
        .map_err(|_| "a name is 1 to 64 bytes of printable text".to_owned())?;
    Ok(renamed)
}

/// Read a manifest file. Not a secret: no mode check, but a size cap.
pub fn read_manifest(file: &Path) -> Result<AgentProfile, String> {
    use std::io::Read;
    let source = format!("manifest {}", file.display());
    let f = std::fs::File::open(file).map_err(|e| format!("{source}: {}", e.kind()))?;
    let mut raw = Vec::new();
    f.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("{source}: {}", e.kind()))?;
    if raw.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!("{source}: too long"));
    }
    parse(&raw, &source)
}

/// The saved profile, if this state dir has one.
pub fn load(state: &Path) -> Result<Option<AgentProfile>, String> {
    let file = path(state);
    match std::fs::read(&file) {
        Ok(raw) => parse(&raw, &file.display().to_string()).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {}", file.display(), e.kind())),
    }
}

/// Write the profile whole (a temp file renamed over the old one), owner-only like the rest of
/// the state dir.
pub fn save(state: &Path, profile: &AgentProfile) -> Result<(), String> {
    let file = path(state);
    let tmp = state.join(format!("{FILE}.tmp"));
    let body = serde_json::to_vec_pretty(&profile.to_value()).expect("json");
    std::fs::write(&tmp, body)
        .and_then(|()| std::fs::rename(&tmp, &file))
        .map_err(|e| format!("{}: {}", file.display(), e.kind()))
}

/// The name a seat advertises when nobody gave it one: `agent_id` (a kit agent's own id, not the
/// shared `default`), else the name the zone's roster already lists for this seat, else
/// `Agent` and the device fingerprint's first eight hex digits, as the SDK names an admitted
/// agent. Never the host name: a CLI install is only `zone-cli` to the zone. Always a valid name.
pub fn default_name(
    agent_id: Option<&str>,
    roster_name: Option<&str>,
    fingerprint: &str,
) -> String {
    let valid = |n: &&str| with_name(named("x").expect("valid"), n).is_ok();
    if let Some(id) = agent_id.filter(|id| *id != DEFAULT_AGENT_ID).filter(valid) {
        return id.to_owned();
    }
    if let Some(name) = roster_name.filter(valid) {
        return name.to_owned();
    }
    let hex: String = fingerprint
        .strip_prefix("sha256:")
        .unwrap_or(fingerprint)
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(8)
        .collect();
    if hex.is_empty() {
        "Agent".to_owned()
    } else {
        format!("Agent {hex}")
    }
}

/// The profile a joined seat advertises when it has neither `agent.json` nor a profile in the
/// SDK: [`default_name`] and the CLI's own skill. None for a seat the roster lists as a human,
/// who names themselves.
fn default_profile(device: &ZoneDevice, agent_id: Option<&str>) -> Option<AgentProfile> {
    let me = device.members().into_iter().find(|m| m.is_me);
    if me.as_ref().is_some_and(|m| m.kind == "human") {
        return None;
    }
    let fingerprint = device.device_fingerprint().unwrap_or_default();
    let name = default_name(agent_id, me.as_ref().map(|m| m.name.as_str()), &fingerprint);
    named(&name).ok()
}

/// Hand the SDK the saved profile, when there is one and a zone is joined. The SDK ignores an
/// unchanged one. A bad file is a warning, never a reason to refuse an unrelated command.
///
/// A joined seat with no saved profile and none in the SDK (a plain `zone join`, or a seat
/// joined before the CLI published anything) gets [`default_profile`], saved to `agent.json`
/// so `zone rename` and an edit keep working from it; the next sync publishes it. `agent_id`
/// is the kit agent id a join was given, if any. True when that default was set just now.
pub fn apply(state: &Path, device: &ZoneDevice, agent_id: Option<&str>) -> bool {
    if device.status().zone_id.is_none() {
        return false;
    }
    match load(state) {
        Ok(None) => {
            if !matches!(device.agent_profile(), Ok(None)) {
                return false;
            }
            let Some(p) = default_profile(device, agent_id) else {
                return false;
            };
            if let Err(e) = device.set_agent_profile(p.clone()) {
                eprintln!(
                    "zone: default name and skills not applied ({})",
                    crate::join::kind(&e)
                );
                return false;
            }
            if let Err(e) = save(state, &p) {
                eprintln!("zone: {e}; the default name and skills apply but are not saved");
            }
            eprintln!(
                "zone: this seat advertises the name {:?} and the skills {:?} from its next sync; \
                 `zone rename NAME` or editing {} changes them",
                p.name,
                p.capabilities,
                path(state).display()
            );
            true
        }
        Ok(Some(p)) => {
            if let Err(e) = device.set_agent_profile(p) {
                eprintln!(
                    "zone: {} not applied ({})",
                    path(state).display(),
                    crate::join::kind(&e)
                );
            }
            false
        }
        Err(e) => {
            eprintln!("zone: {e}; the name and skills last applied stay");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_is_exactly_a_name_and_its_skills() {
        let p = parse(
            br#"{"name":"Aly","capabilities":["Ticket triage"]}"#,
            "test",
        )
        .unwrap();
        assert_eq!(p.name, "Aly");
        assert_eq!(p.capabilities, ["Ticket triage"]);
        // Deliberately empty: withdraws the seat's skills.
        let none = parse(br#"{"name":"Aly","capabilities":[]}"#, "test").unwrap();
        assert!(none.capabilities.is_empty());
        for bad in [
            &br#"{"name":"Aly"}"#[..],
            br#"{"name":"Aly","capabilities":["a"],"price":1}"#,
            br#"{"name":"","capabilities":[]}"#,
            br#"{"name":"Aly","capabilities":["a","a"]}"#,
            br#"{"name":"Aly","capabilities":["line\nbreak"]}"#,
            br#"{"name":"Aly","capabilities":[1]}"#,
            br#"["Aly"]"#,
            b"not json",
        ] {
            assert!(
                parse(bad, "test").is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let long = format!(r#"{{"name":"{}","capabilities":[]}}"#, "x".repeat(65));
        assert!(parse(long.as_bytes(), "test").is_err());
        let many: Vec<String> = (0..33).map(|i| format!("skill {i}")).collect();
        let many = serde_json::json!({"name": "Aly", "capabilities": many}).to_string();
        assert!(parse(many.as_bytes(), "test").is_err());
    }

    #[test]
    fn a_name_alone_advertises_the_cli_skill_and_a_rename_keeps_skills() {
        let p = named("Zone App").unwrap();
        assert_eq!(p.capabilities, DEFAULT_CAPABILITIES);
        let aly = parse(
            br#"{"name":"Aly","capabilities":["Ticket triage"]}"#,
            "test",
        )
        .unwrap();
        let renamed = with_name(aly, "Aly Two").unwrap();
        assert_eq!(renamed.name, "Aly Two");
        assert_eq!(renamed.capabilities, ["Ticket triage"]);
        assert!(named("").is_err());
        assert!(named(&"y".repeat(65)).is_err());
    }

    #[test]
    fn the_saved_profile_round_trips_and_a_missing_one_is_none() {
        crate::state::restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(tmp.path()).unwrap(), None);
        let p = named("Aly").unwrap();
        save(tmp.path(), &p).unwrap();
        assert_eq!(load(tmp.path()).unwrap(), Some(p));
        assert!(!tmp.path().join("agent.json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path(tmp.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_default_name_is_the_agent_id_then_the_roster_name_then_the_fingerprint() {
        let fp = "sha256:0123abcd4567ef";
        assert_eq!(default_name(Some("aly"), Some("Old"), fp), "aly");
        // The shared kit id names nobody.
        assert_eq!(
            default_name(Some(DEFAULT_AGENT_ID), Some("Zone App"), fp),
            "Zone App"
        );
        assert_eq!(default_name(None, Some("Zone App"), fp), "Zone App");
        assert_eq!(default_name(None, None, fp), "Agent 0123abcd");
        // Anything that is not a valid name falls through, so the result always is one.
        let long = "x".repeat(65);
        assert_eq!(
            default_name(Some(&long), Some("bad\nname"), fp),
            "Agent 0123abcd"
        );
        assert_eq!(default_name(None, Some(""), ""), "Agent");
        for n in [
            default_name(Some(&long), None, "sha256:zz"),
            default_name(None, None, fp),
        ] {
            assert!(named(&n).is_ok(), "{n}");
        }
        assert_eq!(
            named(&default_name(None, None, fp)).unwrap().capabilities,
            DEFAULT_CAPABILITIES
        );
    }
}
