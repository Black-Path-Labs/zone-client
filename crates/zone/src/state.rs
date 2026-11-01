//! The state dir: `$XDG_DATA_HOME/zone` (else `~/.local/share/zone`), or `--state DIR`. It holds
//! the vault key and the SDK's encrypted device store. The dir must be mode 0700 and every file
//! in it 0600; a wider one is refused, never silently narrowed, because someone else may already
//! have read it.

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Owner-only from process start: every file and dir the client or the SDK writes is 0600 or
/// 0700 without a window in which it is wider.
pub fn restrict_umask() {
    // SAFETY: umask only sets the process file mode mask and cannot fail.
    unsafe {
        libc::umask(0o077);
    }
}

/// `--state DIR` if given, else `$XDG_DATA_HOME/zone`, else `$HOME/.local/share/zone`.
pub fn resolve(flag: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(dir) = flag {
        return Ok(dir);
    }
    let non_empty = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(data) = non_empty("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data).join("zone"));
    }
    if let Some(home) = non_empty("HOME") {
        return Ok(PathBuf::from(home).join(".local/share/zone"));
    }
    Err("no state dir: set XDG_DATA_HOME or HOME, or pass --state DIR".into())
}

/// Make the state dir (0700) if it is missing, then check it.
pub fn create(dir: &Path) -> Result<(), String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("state dir {}: {}", dir.display(), e.kind()))?;
    check(dir)
}

/// Refuse a state dir, or anything in it, that others can read, write or enter.
pub fn check(dir: &Path) -> Result<(), String> {
    check_entry(dir, true)?;
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {}", dir.display(), e.kind()))? {
        let path = entry
            .map_err(|e| format!("{}: {}", dir.display(), e.kind()))?
            .path();
        if path.is_dir() {
            check(&path)?;
        } else {
            check_entry(&path, false)?;
        }
    }
    Ok(())
}

fn check_entry(path: &Path, is_dir: bool) -> Result<(), String> {
    let meta =
        std::fs::symlink_metadata(path).map_err(|e| format!("{}: {}", path.display(), e.kind()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is a symlink; the state dir holds only real files",
            path.display()
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        let want = if is_dir { "700" } else { "600" };
        return Err(format!(
            "{} is open to others (mode {mode:03o}); chmod {want} it",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;

    #[test]
    fn create_makes_an_owner_only_dir() {
        restrict_umask();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/zone");
        create(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn a_dir_open_to_others_is_refused_not_narrowed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zone");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
        let err = create(&dir).unwrap_err();
        assert!(err.contains("mode 755"), "{err}");
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "check must not chmod");
    }

    #[test]
    fn a_world_readable_file_anywhere_inside_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zone");
        create(&dir).unwrap();
        let items = dir.join("items");
        DirBuilder::new().mode(0o700).create(&items).unwrap();
        let blob = items.join("x.bin");
        std::fs::write(&blob, b"x").unwrap();
        std::fs::set_permissions(&blob, Permissions::from_mode(0o600)).unwrap();
        check(&dir).unwrap();
        std::fs::set_permissions(&blob, Permissions::from_mode(0o644)).unwrap();
        let err = check(&dir).unwrap_err();
        assert!(err.contains("x.bin") && err.contains("chmod 600"), "{err}");
    }

    #[test]
    fn resolve_prefers_the_flag() {
        let flag = PathBuf::from("/tmp/elsewhere");
        assert_eq!(resolve(Some(flag.clone())).unwrap(), flag);
    }
}
