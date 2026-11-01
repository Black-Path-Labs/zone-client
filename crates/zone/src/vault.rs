//! The client's [`SecretVault`]: one random 32-byte key in `<state>/vault.key` (mode 0600) wraps
//! the SDK store key with AES-256-GCM. A phone holds this key in its keystore; a CLI has the file
//! system and its permissions, so a key file anyone else can read is refused rather than used.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use bp_zone_core::{SecretVault, ZoneError};
use rand_core::{OsRng, RngCore};
use zeroize::Zeroizing;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

pub struct FileVault {
    key: Zeroizing<[u8; 32]>,
}

fn vault_error(reason: impl Into<String>) -> ZoneError {
    ZoneError::Vault {
        reason: reason.into(),
    }
}

impl FileVault {
    pub fn path(state: &Path) -> PathBuf {
        state.join("vault.key")
    }

    pub fn exists(state: &Path) -> bool {
        FileVault::path(state).exists()
    }

    /// Open the vault key in `state`, making it on first use.
    pub fn open(state: &Path) -> Result<FileVault, ZoneError> {
        let path = FileVault::path(state);
        let mut key = Zeroizing::new([0u8; 32]);
        match File::open(&path) {
            Ok(mut f) => {
                let mode = f
                    .metadata()
                    .map_err(|e| vault_error(e.kind().to_string()))?
                    .permissions()
                    .mode();
                if mode & 0o077 != 0 {
                    return Err(vault_error("vault.key is open to others; chmod 600 it"));
                }
                let mut raw = Zeroizing::new(Vec::new());
                f.read_to_end(&mut raw)
                    .map_err(|e| vault_error(e.kind().to_string()))?;
                if raw.len() != key.len() {
                    return Err(vault_error("vault.key is not 32 bytes"));
                }
                key.copy_from_slice(&raw);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                OsRng.fill_bytes(key.as_mut());
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .map_err(|e| vault_error(e.kind().to_string()))?;
                f.write_all(key.as_ref())
                    .and_then(|()| f.sync_all())
                    .map_err(|e| vault_error(e.kind().to_string()))?;
            }
            Err(e) => return Err(vault_error(e.kind().to_string())),
        }
        Ok(FileVault { key })
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(self.key.as_ref()).expect("32-byte key")
    }
}

impl SecretVault for FileVault {
    fn wrap(&self, plaintext: Vec<u8>) -> Result<Vec<u8>, ZoneError> {
        let plaintext = Zeroizing::new(plaintext);
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let ct = self
            .cipher()
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
            .map_err(|_| vault_error("wrap failed"))?;
        Ok([nonce.as_slice(), &ct].concat())
    }

    fn unwrap(&self, wrapped: Vec<u8>) -> Result<Vec<u8>, ZoneError> {
        if wrapped.len() < NONCE_LEN + TAG_LEN {
            return Err(vault_error("wrapped value too short"));
        }
        self.cipher()
            .decrypt(
                Nonce::from_slice(&wrapped[..NONCE_LEN]),
                &wrapped[NONCE_LEN..],
            )
            .map_err(|_| vault_error("unwrap failed: another vault key"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;

    #[test]
    fn the_key_is_made_once_owner_only_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = FileVault::open(tmp.path()).unwrap();
        let mode = std::fs::metadata(FileVault::path(tmp.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let wrapped = vault.wrap(b"store key".to_vec()).unwrap();
        let again = FileVault::open(tmp.path()).unwrap();
        assert_eq!(again.unwrap(wrapped).unwrap(), b"store key");
    }

    #[test]
    fn a_world_readable_key_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        FileVault::open(tmp.path()).unwrap();
        std::fs::set_permissions(FileVault::path(tmp.path()), Permissions::from_mode(0o644))
            .unwrap();
        let err = FileVault::open(tmp.path()).err().unwrap();
        assert!(err.to_string().contains("chmod 600"), "{err}");
    }

    #[test]
    fn another_vault_cannot_unwrap() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let wrapped = FileVault::open(a.path())
            .unwrap()
            .wrap(vec![7; 32])
            .unwrap();
        assert!(FileVault::open(b.path()).unwrap().unwrap(wrapped).is_err());
    }
}
