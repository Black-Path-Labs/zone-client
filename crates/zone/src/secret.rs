//! The kit key: the operator's standing key for an agent install (SDK `agent_join`). It is read
//! from `--key-file FILE` (mode 0600 enforced) or, with `--key-file -`, from stdin; never from
//! argv or the environment, which other local users and process listings can see. The key is
//! 32 bytes, raw or as 64 hex digits, and is held zeroized. Errors name the source, never bytes
//! of it.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use bp_workspace_core::agent_join::kit_key_from_seed;
use bp_workspace_core::jws::PublicJwk;
use bp_workspace_core::zone::jwk_thumbprint;
use zeroize::Zeroizing;

/// The largest input taken as a key, so stdin cannot be made to buffer without end.
const MAX_KEY_INPUT: u64 = 4096;

/// A checked kit key: the 32-byte secret, zeroed on drop.
pub struct KitKey {
    seed: Zeroizing<Vec<u8>>,
}

impl KitKey {
    /// Read `path`, or stdin for `-`.
    pub fn read(path: &Path) -> Result<KitKey, String> {
        if path == Path::new("-") {
            return KitKey::parse(read_capped(std::io::stdin().lock())?, "stdin");
        }
        let shown = path.display();
        let f = File::open(path).map_err(|e| format!("key file {shown}: {}", e.kind()))?;
        let mode = f
            .metadata()
            .map_err(|e| format!("key file {shown}: {}", e.kind()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "key file {shown} is open to others (mode {:03o}); chmod 600 it",
                mode & 0o777
            ));
        }
        KitKey::parse(
            read_capped(f).map_err(|e| format!("key file {shown}: {e}"))?,
            &format!("key file {shown}"),
        )
    }

    fn parse(raw: Zeroizing<Vec<u8>>, source: &str) -> Result<KitKey, String> {
        let seed = if raw.len() == 32 {
            raw
        } else {
            let text = raw.trim_ascii();
            if text.len() != 64 || !text.iter().all(u8::is_ascii_hexdigit) {
                return Err(format!("{source}: not 32 bytes or 64 hex digits"));
            }
            let digit = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            Zeroizing::new(
                text.chunks(2)
                    .map(|p| (digit(p[0]) << 4) | digit(p[1]))
                    .collect(),
            )
        };
        kit_key_from_seed(&seed).map_err(|_| format!("{source}: not a P-256 key"))?;
        Ok(KitKey { seed })
    }

    /// The secret, for the SDK call that signs with it (`request_admission`).
    pub fn seed(&self) -> &[u8] {
        &self.seed
    }

    /// RFC 7638 thumbprint of the public key: the kit fingerprint an admitter sees.
    pub fn fingerprint(&self) -> String {
        let key = kit_key_from_seed(&self.seed).expect("checked in parse");
        jwk_thumbprint(&PublicJwk::from_verifying_key(key.verifying_key()))
    }
}

fn read_capped(r: impl Read) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut raw = Zeroizing::new(Vec::new());
    r.take(MAX_KEY_INPUT + 1)
        .read_to_end(&mut raw)
        .map_err(|e| e.kind().to_string())?;
    if raw.len() as u64 > MAX_KEY_INPUT {
        return Err("key input too long".into());
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;
    use std::io::Write;

    /// A test key: bytes 1..=32. Not a key anyone uses.
    fn test_seed() -> Vec<u8> {
        (1..=32).collect()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn key_file(dir: &Path, bytes: &[u8], mode: u32) -> std::path::PathBuf {
        let path = dir.join("kit.key");
        let mut f = File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn the_fingerprint_is_stable_across_encodings_and_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = KitKey::read(&key_file(tmp.path(), &test_seed(), 0o600)).unwrap();
        let text = format!("{}\n", hex(&test_seed()));
        let hexed = KitKey::read(&key_file(tmp.path(), text.as_bytes(), 0o600)).unwrap();
        let upper = KitKey::parse(Zeroizing::new(text.to_uppercase().into_bytes()), "t").unwrap();
        assert_eq!(raw.seed(), test_seed());
        assert_eq!(raw.fingerprint(), hexed.fingerprint());
        assert_eq!(raw.fingerprint(), upper.fingerprint());
        // Pinned, computed outside the SDK (Python `cryptography`, RFC 7638): a zone-sdk bump
        // that changes the kit fingerprint changes what admitters see.
        assert_eq!(raw.fingerprint(), PINNED_FINGERPRINT);
    }

    const PINNED_FINGERPRINT: &str = "6UoWwDCkLjV0J-pQG8c0THxbVhBcpR0AZDift1Yl5DM";

    #[test]
    fn a_key_file_others_can_read_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        for mode in [0o644, 0o640, 0o604, 0o660] {
            let path = key_file(tmp.path(), &test_seed(), mode);
            let err = KitKey::read(&path).err().unwrap();
            assert!(err.contains("chmod 600"), "{mode:o}: {err}");
        }
    }

    #[test]
    fn errors_carry_no_key_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut junk = hex(&test_seed());
        junk.push_str("zz");
        let path = key_file(tmp.path(), junk.as_bytes(), 0o600);
        let err = KitKey::read(&path).err().unwrap();
        assert!(!err.contains(&hex(&test_seed())[..16]), "{err}");
        assert!(KitKey::parse(Zeroizing::new(vec![0; 32]), "t").is_err());
    }

    #[test]
    fn oversized_input_is_refused() {
        let big = vec![b'a'; MAX_KEY_INPUT as usize + 1];
        assert!(read_capped(big.as_slice()).is_err());
    }
}
