//! This install's zone device, opened through the SDK (`bp_zone_core::ZoneDevice`). The SDK makes
//! the person secret and the store key on first open and keeps them in its encrypted store; the
//! per-zone pseudonym and device keys derive from them when a zone is joined. The client only
//! supplies the vault and the state dir.

use std::path::Path;
use std::sync::Arc;

use bp_zone_core::{CoreTransport, HttpHeader, HttpReply, ZoneDevice, ZoneError};

use crate::state;
use crate::vault::FileVault;

/// The device name Core and other members see for a CLI install.
const DEVICE_NAME: &str = "zone-cli";

/// The transport for commands that work from local state alone (`whoami`). Any Core call
/// through it fails as "Core not reached" rather than reaching the network.
pub struct Offline;

impl CoreTransport for Offline {
    fn send(
        &self,
        _method: String,
        _url: String,
        _headers: Vec<HttpHeader>,
        _body: Vec<u8>,
    ) -> Result<HttpReply, ZoneError> {
        Err(ZoneError::Core {
            status: 0,
            reason: "this command works offline".into(),
        })
    }
}

/// The device in `dir`, if this install has one. Never creates state.
pub fn open_existing(
    dir: &Path,
    transport: Arc<dyn CoreTransport>,
) -> Result<Option<Arc<ZoneDevice>>, String> {
    if !dir.exists() {
        return Ok(None);
    }
    state::check(dir)?;
    if !FileVault::exists(dir) {
        return Ok(None);
    }
    // The store's settings win over these arguments; they only seed a new device.
    let device = open(dir, String::new(), String::new(), transport)?;
    // A saved `agent.json` is what this seat advertises; the next sync publishes a change.
    crate::profile::apply(dir, &device, None);
    Ok(Some(device))
}

/// The device in `dir`, made on first use with `core_url` and `core_zone` as where new zones go.
pub fn open_or_create(
    dir: &Path,
    core_url: &str,
    core_zone: &str,
    transport: Arc<dyn CoreTransport>,
) -> Result<Arc<ZoneDevice>, String> {
    state::create(dir)?;
    let device = open(dir, core_url.into(), core_zone.into(), transport)?;
    // The SDK wrote its store under the client's umask; check that held.
    state::check(dir)?;
    Ok(device)
}

fn open(
    dir: &Path,
    core_url: String,
    core_zone: String,
    transport: Arc<dyn CoreTransport>,
) -> Result<Arc<ZoneDevice>, String> {
    let vault = FileVault::open(dir).map_err(|e| e.to_string())?;
    let data_dir = dir
        .to_str()
        .ok_or_else(|| format!("state dir {} is not UTF-8", dir.display()))?
        .to_owned();
    ZoneDevice::open(
        data_dir,
        DEVICE_NAME.into(),
        core_url,
        core_zone,
        Arc::new(vault),
        transport,
    )
    .map_err(|e| e.to_string())
}

/// Who this device is in its current zone: never key material, only public identifiers.
pub struct Identity {
    pub zone_id: String,
    /// `sha256:` of the device bundle.
    pub device_fingerprint: String,
    /// The pseudonym's BPID (`bp:self:...`), once the zone's roster attests this device.
    pub pseudonym: Option<String>,
}

/// The current zone's identity, or None before the device has joined a zone.
pub fn identity(device: &ZoneDevice) -> Result<Option<Identity>, String> {
    let Some(zone_id) = device.status().zone_id else {
        return Ok(None);
    };
    let device_fingerprint = device.device_fingerprint().map_err(|e| e.to_string())?;
    // A device still waiting for admission has no verified roster yet, so no pseudonym.
    let pseudonym = match device.zone_devices() {
        Ok(devices) => devices
            .into_iter()
            .find(|(fp, _)| *fp == device_fingerprint)
            .map(|(_, bpid)| bpid),
        Err(ZoneError::NotAdmitted) => None,
        Err(e) => return Err(e.to_string()),
    };
    Ok(Some(Identity {
        zone_id,
        device_fingerprint,
        pseudonym,
    }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Only `check_url` sees this address: opening a device makes local state and calls no Core.
    const LOCAL_CORE: &str = "http://127.0.0.1:9";

    fn modes(dir: &Path, out: &mut Vec<(String, u32)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            out.push((path.display().to_string(), mode));
            if path.is_dir() {
                modes(&path, out);
            }
        }
    }

    #[test]
    fn first_use_makes_an_owner_only_device_that_reopens() {
        state::restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zone");
        assert!(open_existing(&dir, Arc::new(Offline)).unwrap().is_none());

        let made = open_or_create(&dir, LOCAL_CORE, "ca-mb-1", Arc::new(Offline)).unwrap();
        assert_eq!(made.status().device_name, DEVICE_NAME);
        assert!(identity(&made).unwrap().is_none(), "no zone joined yet");
        drop(made);

        let mut all = vec![];
        modes(&dir, &mut all);
        assert!(all.iter().any(|(p, _)| p.ends_with("vault.key")));
        assert!(all.iter().any(|(p, _)| p.ends_with("vault.bin")));
        for (path, mode) in &all {
            let want = if Path::new(path).is_dir() {
                0o700
            } else {
                0o600
            };
            assert_eq!(*mode, want, "{path}");
        }

        let again = open_existing(&dir, Arc::new(Offline)).unwrap().unwrap();
        let status = again.status();
        assert_eq!(status.core_url, LOCAL_CORE, "the store's settings win");
        assert_eq!(status.zone_id, None);
    }

    #[test]
    fn a_widened_store_file_is_refused_before_the_sdk_reads_it() {
        state::restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zone");
        drop(open_or_create(&dir, LOCAL_CORE, "ca-mb-1", Arc::new(Offline)).unwrap());
        std::fs::set_permissions(
            dir.join("vault.bin"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let err = open_existing(&dir, Arc::new(Offline)).err().unwrap();
        assert!(
            err.contains("vault.bin") && err.contains("chmod 600"),
            "{err}"
        );
    }

    #[test]
    fn another_vault_key_cannot_open_the_store() {
        state::restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zone");
        drop(open_or_create(&dir, LOCAL_CORE, "ca-mb-1", Arc::new(Offline)).unwrap());
        std::fs::remove_file(FileVault::path(&dir)).unwrap();
        FileVault::open(&dir).unwrap();
        let err = open_existing(&dir, Arc::new(Offline)).err().unwrap();
        assert!(err.contains("vault"), "{err}");
    }
}
