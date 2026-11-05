//! `zone fs <agent> mount <dir>`: a remote export as a local directory that ordinary tools can
//! browse, and nothing more. Lookup, getattr, readdir, open and read map to `ls`, `stat` and
//! ranged `cat` on the owner; every write-like operation answers `EROFS` here, before any request
//! leaves this machine, on a mount the kernel also holds read-only.
//!
//! Caching (ZF-D10): metadata is reused for at most one second, counting both this process and
//! the kernel's entry and attribute caches. File contents are never cached: files open with
//! direct I/O, so every read is a fresh `cat` and a grant that expired or was revoked fails the
//! next read instead of serving stale bytes.
//!
//! Linux only for now, through the `fusermount3` helper, so the build needs no libfuse headers.

// Elsewhere only `available` is reached; the logic stays compiled so its tests run everywhere.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bp_zone_fs::policy::{Entry, Kind};
use bp_zone_fs::{Code, FsClient, FsError};

/// The longest any metadata is reused, here and in the kernel together.
const TTL: Duration = Duration::from_secs(1);
/// The mount root's inode, fixed by FUSE.
const ROOT_INO: u64 = 1;

/// Fails, before any network activity, when this machine cannot mount: the platform has no
/// supported FUSE, or FUSE is not installed.
pub fn available() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if !std::path::Path::new("/dev/fuse").exists() {
            return Err(
                "zone fs mount needs FUSE, and /dev/fuse is missing: install fuse3 \
                        (or load the fuse kernel module) and try again"
                    .into(),
            );
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } != 0 && fusermount().is_none() {
            return Err(
                "zone fs mount needs the fusermount3 helper, and it is not installed: \
                        install fuse3 and try again"
                    .into(),
            );
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        Err(
            "zone fs mount is Linux-only in this release (macOS needs macFUSE, not yet \
             supported); use `zone fs <agent> ls` and `cat` instead"
                .into(),
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(
            "zone fs mount needs Linux with FUSE; this platform has none. Use \
             `zone fs <agent> ls` and `cat` instead"
                .into(),
        )
    }
}

/// The fusermount helper fuser will run: `FUSERMOUNT_PATH`, else the first on `PATH` or in the
/// usual system directories.
#[cfg(target_os = "linux")]
fn fusermount() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let runnable = |p: &std::path::Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if let Some(p) = std::env::var_os("FUSERMOUNT_PATH") {
        let p = std::path::PathBuf::from(p);
        return runnable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::env::split_paths(&path)
        .chain(["/usr/bin", "/bin", "/sbin", "/usr/sbin"].map(std::path::PathBuf::from));
    for dir in dirs {
        for name in ["fusermount3", "fusermount"] {
            let p = dir.join(name);
            if runnable(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// Mount `root` (a remote path such as `aly-export` or `aly-export/src`; empty for the list of
/// exports) read-only at `dir`, and serve it until SIGINT, SIGTERM or an outside unmount. Then
/// unmount and return.
pub fn mount(client: Arc<FsClient>, root: &str, dir: &std::path::Path) -> Result<(), String> {
    available()?;
    #[cfg(target_os = "linux")]
    {
        kernel::mount(client, root, dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (client, root, dir);
        unreachable!("available() refuses every platform but Linux")
    }
}

/// The errno a refusal surfaces as.
fn errno(e: &FsError) -> i32 {
    match e.code {
        Code::Denied | Code::ExportNotFound | Code::Path => libc::ENOENT,
        Code::AgentNotFound | Code::AgentAmbiguous => libc::ENOENT,
        Code::Rate => libc::EAGAIN,
        Code::Grant | Code::GrantExpired | Code::GrantRevoked => libc::EACCES,
        Code::Size => libc::EFBIG,
        Code::ReadOnly => libc::EROFS,
        Code::Stale => libc::ESTALE,
        Code::Cancelled => libc::EINTR,
        Code::Transport | Code::IndexUnavailable => libc::EIO,
    }
}

/// The operations that would change the tree. Each is refused here, with no request sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteOp {
    Create,
    Mknod,
    Mkdir,
    Unlink,
    Rmdir,
    Symlink,
    Rename,
    Link,
    Setattr,
    Write,
    Setxattr,
    Removexattr,
    Fallocate,
    CopyFileRange,
}

impl WriteOp {
    #[cfg(test)]
    const ALL: [WriteOp; 14] = [
        WriteOp::Create,
        WriteOp::Mknod,
        WriteOp::Mkdir,
        WriteOp::Unlink,
        WriteOp::Rmdir,
        WriteOp::Symlink,
        WriteOp::Rename,
        WriteOp::Link,
        WriteOp::Setattr,
        WriteOp::Write,
        WriteOp::Setxattr,
        WriteOp::Removexattr,
        WriteOp::Fallocate,
        WriteOp::CopyFileRange,
    ];
}

/// What the kernel is told about one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attr {
    pub ino: u64,
    pub dir: bool,
    pub size: u64,
    pub mtime_ms: u64,
    pub exec: bool,
}

/// One directory entry, at its position in the listing; the next read resumes after `offset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirEnt {
    pub ino: u64,
    pub offset: u64,
    pub dir: bool,
    pub name: String,
}

#[derive(Default)]
struct Table {
    /// Inode to path relative to the mount root ("" is the root). Inodes are never reused.
    paths: HashMap<u64, String>,
    inos: HashMap<String, u64>,
    /// Metadata by relative path, with when it was fetched.
    attrs: HashMap<String, (Entry, Instant)>,
    /// Listings of open directories, by handle: fetched once at opendir, so the kernel's
    /// repeated readdir calls for one listing cost one `ls`.
    dirs: HashMap<u64, Vec<DirEnt>>,
    next_fh: u64,
}

/// The mount's logic, independent of the kernel interface so it can be tested without one.
pub(crate) struct ZoneFs {
    client: Arc<FsClient>,
    root: String,
    t: Mutex<Table>,
}

impl ZoneFs {
    pub fn new(client: Arc<FsClient>, root: &str) -> ZoneFs {
        let mut t = Table::default();
        t.paths.insert(ROOT_INO, String::new());
        t.inos.insert(String::new(), ROOT_INO);
        ZoneFs {
            client,
            root: root.trim_matches('/').to_owned(),
            t: Mutex::new(t),
        }
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.t.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn remote(&self, rel: &str) -> String {
        match (self.root.is_empty(), rel.is_empty()) {
            (true, _) => rel.to_owned(),
            (false, true) => self.root.clone(),
            (false, false) => format!("{}/{rel}", self.root),
        }
    }

    fn path_of(&self, ino: u64) -> Result<String, i32> {
        self.table().paths.get(&ino).cloned().ok_or(libc::ENOENT)
    }

    fn ino_of(&self, rel: &str) -> u64 {
        let mut t = self.table();
        if let Some(i) = t.inos.get(rel) {
            return *i;
        }
        let ino = t.paths.len() as u64 + 1;
        t.paths.insert(ino, rel.to_owned());
        t.inos.insert(rel.to_owned(), ino);
        ino
    }

    fn remember(&self, rel: &str, e: Entry, at: Instant) {
        self.table().attrs.insert(rel.to_owned(), (e, at));
    }

    /// The attributes of `rel` and how much longer the kernel may keep them.
    fn attr(&self, rel: &str) -> Result<(Attr, Duration), i32> {
        let cached = self
            .table()
            .attrs
            .get(rel)
            .filter(|(_, at)| at.elapsed() < TTL)
            .cloned();
        let (e, at) = match cached {
            Some(c) => c,
            None if self.remote(rel).is_empty() => (
                // The list of exports has no metadata of its own.
                Entry {
                    path: String::new(),
                    kind: Kind::Dir,
                    size: 0,
                    mtime_ms: 0,
                    exec: false,
                },
                Instant::now(),
            ),
            None => {
                let at = Instant::now();
                let e = self.client.stat(&self.remote(rel)).map_err(|e| errno(&e))?;
                self.remember(rel, e.clone(), at);
                (e, at)
            }
        };
        let attr = Attr {
            ino: self.ino_of(rel),
            dir: e.kind == Kind::Dir,
            size: e.size,
            mtime_ms: e.mtime_ms,
            exec: e.exec,
        };
        Ok((attr, TTL.saturating_sub(at.elapsed())))
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Result<(Attr, Duration), i32> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(libc::ENOENT);
        }
        let dir = self.path_of(parent)?;
        self.attr(&join(&dir, name))
    }

    pub fn getattr(&self, ino: u64) -> Result<(Attr, Duration), i32> {
        self.attr(&self.path_of(ino)?)
    }

    /// One `ls`, kept for this directory handle; its entries' metadata are cached too.
    pub fn opendir(&self, ino: u64) -> Result<u64, i32> {
        let rel = self.path_of(ino)?;
        let at = Instant::now();
        let entries = self.client.ls(&self.remote(&rel)).map_err(|e| errno(&e))?;
        let up = rel.rsplit_once('/').map_or("", |(p, _)| p);
        let mut list = vec![
            DirEnt {
                ino,
                offset: 1,
                dir: true,
                name: ".".into(),
            },
            DirEnt {
                ino: if rel.is_empty() {
                    ROOT_INO
                } else {
                    self.ino_of(up)
                },
                offset: 2,
                dir: true,
                name: "..".into(),
            },
        ];
        for e in entries {
            // A listing names each child; anything else is not ours to show.
            let name = e.path.rsplit('/').next().unwrap_or_default().to_owned();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            let child = join(&rel, &name);
            let dir = e.kind == Kind::Dir;
            if !self.remote(&rel).is_empty() {
                self.remember(&child, e, at);
            }
            list.push(DirEnt {
                ino: self.ino_of(&child),
                offset: list.len() as u64 + 1,
                dir,
                name,
            });
        }
        let mut t = self.table();
        t.next_fh += 1;
        let fh = t.next_fh;
        t.dirs.insert(fh, list);
        Ok(fh)
    }

    /// The entries of an open directory after `offset`.
    pub fn readdir(&self, fh: u64, offset: u64) -> Result<Vec<DirEnt>, i32> {
        let t = self.table();
        let list = t.dirs.get(&fh).ok_or(libc::EBADF)?;
        Ok(list.iter().skip(offset as usize).cloned().collect())
    }

    pub fn releasedir(&self, fh: u64) {
        self.table().dirs.remove(&fh);
    }

    /// Opening for anything but reading is refused without a request.
    pub fn open(&self, ino: u64, flags: i32) -> Result<(), i32> {
        let writes = libc::O_WRONLY | libc::O_RDWR | libc::O_TRUNC | libc::O_CREAT | libc::O_APPEND;
        if flags & writes != 0 {
            return Err(libc::EROFS);
        }
        if self.getattr(ino)?.0.dir {
            return Err(libc::EISDIR);
        }
        Ok(())
    }

    /// Up to `size` bytes from `offset`, fetched now: a short answer is the end of the file.
    pub fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let remote = self.remote(&self.path_of(ino)?);
        let mut out = Vec::with_capacity(size as usize);
        loop {
            let want = size as u64 - out.len() as u64;
            if want == 0 {
                break;
            }
            let pos = offset + out.len() as u64;
            match self.client.cat(&remote, pos, Some(want), &mut out) {
                // The owner cut the answer at its bound; ask for the rest.
                Ok(end) if end.truncated && end.bytes > 0 => continue,
                Ok(_) => break,
                // Past the end, as when the file shrank since its size was read.
                Err(e) if e.code == Code::Path => break,
                Err(e) => return Err(errno(&e)),
            }
        }
        Ok(out)
    }

    /// Asked to check write access: refused without a request. Anything else is allowed here and
    /// checked by the owner on use.
    pub fn access(&self, mask: i32) -> Result<(), i32> {
        if mask & libc::W_OK != 0 {
            return Err(libc::EROFS);
        }
        Ok(())
    }

    /// Every write-like operation: read-only, and no request is sent.
    pub fn refuse(&self, _op: WriteOp) -> i32 {
        libc::EROFS
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(target_os = "linux")]
mod kernel {
    //! The fuser adapter: each kernel request to [`ZoneFs`], each answer back.

    use std::ffi::OsStr;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use bp_zone_fs::policy::Kind;
    use bp_zone_fs::FsClient;
    use fuser::{
        AccessFlags, BsdFileFlags, Config, CopyFileRangeFlags, Errno, FileAttr, FileHandle,
        FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, MountOption, OpenFlags,
        RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
        ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
    };

    use super::{Attr, WriteOp, ZoneFs};

    struct Kernel {
        fs: ZoneFs,
        uid: u32,
        gid: u32,
    }

    fn err(e: i32) -> Errno {
        Errno::from_i32(e)
    }

    fn utf8(name: &OsStr) -> &str {
        // A name that is not UTF-8 cannot exist on the owner's side of the protocol.
        name.to_str().unwrap_or("")
    }

    impl Kernel {
        fn file_attr(&self, a: &Attr) -> FileAttr {
            let mtime = UNIX_EPOCH + Duration::from_millis(a.mtime_ms);
            FileAttr {
                ino: INodeNo(a.ino),
                size: a.size,
                blocks: a.size.div_ceil(512),
                atime: mtime,
                mtime,
                ctime: mtime,
                crtime: mtime,
                kind: if a.dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                perm: if a.dir || a.exec { 0o555 } else { 0o444 },
                nlink: if a.dir { 2 } else { 1 },
                uid: self.uid,
                gid: self.gid,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            }
        }
    }

    impl Filesystem for Kernel {
        fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
            match self.fs.lookup(parent.0, utf8(name)) {
                Ok((a, ttl)) => reply.entry(&ttl, &self.file_attr(&a), Generation(0)),
                Err(e) => reply.error(err(e)),
            }
        }

        fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
            match self.fs.getattr(ino.0) {
                Ok((a, ttl)) => reply.attr(&ttl, &self.file_attr(&a)),
                Err(e) => reply.error(err(e)),
            }
        }

        fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
            match self.fs.opendir(ino.0) {
                Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
                Err(e) => reply.error(err(e)),
            }
        }

        fn readdir(
            &self,
            _req: &Request,
            _ino: INodeNo,
            fh: FileHandle,
            offset: u64,
            mut reply: ReplyDirectory,
        ) {
            match self.fs.readdir(fh.0, offset) {
                Ok(list) => {
                    for d in list {
                        let kind = if d.dir {
                            FileType::Directory
                        } else {
                            FileType::RegularFile
                        };
                        if reply.add(INodeNo(d.ino), d.offset, kind, &d.name) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(e) => reply.error(err(e)),
            }
        }

        fn releasedir(
            &self,
            _req: &Request,
            _ino: INodeNo,
            fh: FileHandle,
            _flags: OpenFlags,
            reply: ReplyEmpty,
        ) {
            self.fs.releasedir(fh.0);
            reply.ok();
        }

        fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
            match self.fs.open(ino.0, flags.0) {
                // Direct I/O: the kernel keeps no page cache, so every read reaches the owner.
                Ok(()) => reply.opened(FileHandle(0), FopenFlags::FOPEN_DIRECT_IO),
                Err(e) => reply.error(err(e)),
            }
        }

        fn read(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: FileHandle,
            offset: u64,
            size: u32,
            _flags: OpenFlags,
            _lock_owner: Option<LockOwner>,
            reply: ReplyData,
        ) {
            match self.fs.read(ino.0, offset, size) {
                Ok(data) => reply.data(&data),
                Err(e) => reply.error(err(e)),
            }
        }

        fn access(&self, _req: &Request, _ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
            match self.fs.access(mask.bits()) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(err(e)),
            }
        }

        fn flush(
            &self,
            _req: &Request,
            _ino: INodeNo,
            _fh: FileHandle,
            _lock_owner: LockOwner,
            reply: ReplyEmpty,
        ) {
            reply.ok();
        }

        fn create(
            &self,
            _req: &Request,
            _parent: INodeNo,
            _name: &OsStr,
            _mode: u32,
            _umask: u32,
            _flags: i32,
            reply: ReplyCreate,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Create)));
        }

        fn mknod(
            &self,
            _req: &Request,
            _parent: INodeNo,
            _name: &OsStr,
            _mode: u32,
            _umask: u32,
            _rdev: u32,
            reply: ReplyEntry,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Mknod)));
        }

        fn mkdir(
            &self,
            _req: &Request,
            _parent: INodeNo,
            _name: &OsStr,
            _mode: u32,
            _umask: u32,
            reply: ReplyEntry,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Mkdir)));
        }

        fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
            reply.error(err(self.fs.refuse(WriteOp::Unlink)));
        }

        fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
            reply.error(err(self.fs.refuse(WriteOp::Rmdir)));
        }

        fn symlink(
            &self,
            _req: &Request,
            _parent: INodeNo,
            _link_name: &OsStr,
            _target: &Path,
            reply: ReplyEntry,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Symlink)));
        }

        fn rename(
            &self,
            _req: &Request,
            _parent: INodeNo,
            _name: &OsStr,
            _newparent: INodeNo,
            _newname: &OsStr,
            _flags: RenameFlags,
            reply: ReplyEmpty,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Rename)));
        }

        fn link(
            &self,
            _req: &Request,
            _ino: INodeNo,
            _newparent: INodeNo,
            _newname: &OsStr,
            reply: ReplyEntry,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Link)));
        }

        fn setattr(
            &self,
            _req: &Request,
            _ino: INodeNo,
            _mode: Option<u32>,
            _uid: Option<u32>,
            _gid: Option<u32>,
            _size: Option<u64>,
            _atime: Option<TimeOrNow>,
            _mtime: Option<TimeOrNow>,
            _ctime: Option<SystemTime>,
            _fh: Option<FileHandle>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<BsdFileFlags>,
            reply: ReplyAttr,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Setattr)));
        }

        fn write(
            &self,
            _req: &Request,
            _ino: INodeNo,
            _fh: FileHandle,
            _offset: u64,
            _data: &[u8],
            _write_flags: WriteFlags,
            _flags: OpenFlags,
            _lock_owner: Option<LockOwner>,
            reply: ReplyWrite,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Write)));
        }

        fn setxattr(
            &self,
            _req: &Request,
            _ino: INodeNo,
            _name: &OsStr,
            _value: &[u8],
            _flags: i32,
            _position: u32,
            reply: ReplyEmpty,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Setxattr)));
        }

        fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
            reply.error(err(self.fs.refuse(WriteOp::Removexattr)));
        }

        fn fallocate(
            &self,
            _req: &Request,
            _ino: INodeNo,
            _fh: FileHandle,
            _offset: u64,
            _length: u64,
            _mode: i32,
            reply: ReplyEmpty,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::Fallocate)));
        }

        fn copy_file_range(
            &self,
            _req: &Request,
            _ino_in: INodeNo,
            _fh_in: FileHandle,
            _offset_in: u64,
            _ino_out: INodeNo,
            _fh_out: FileHandle,
            _offset_out: u64,
            _len: u64,
            _flags: CopyFileRangeFlags,
            reply: ReplyWrite,
        ) {
            reply.error(err(self.fs.refuse(WriteOp::CopyFileRange)));
        }
    }

    pub(super) fn mount(client: Arc<FsClient>, root: &str, dir: &Path) -> Result<(), String> {
        if !dir.is_dir() {
            return Err(format!("{}: not a directory", dir.display()));
        }
        // Fail here, not on the first `ls` in the mount, when the path is wrong or not shared.
        let root = root.trim_matches('/');
        if !root.is_empty() {
            let e = client.stat(root).map_err(|e| format!("{root}: {e}"))?;
            if e.kind != Kind::Dir {
                return Err(format!("{root}: not a directory"));
            }
        }
        crate::run::catch_signals();
        let fs = Kernel {
            fs: ZoneFs::new(client, root),
            // SAFETY: getuid and getgid have no preconditions and cannot fail.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        };
        let session = spawn(fs, dir)?;
        eprintln!(
            "zone fs: {} mounted read-only at {}; Ctrl+C to unmount",
            if root.is_empty() { "exports" } else { root },
            dir.display()
        );
        while !crate::run::stopped() && !session.guard.is_finished() {
            std::thread::sleep(Duration::from_millis(100));
        }
        unmount(session, dir)
    }

    /// Mount read-only, owner-only (no allow_other). auto_unmount is left off: fusermount3
    /// accepts it only together with allow_other, so a clean stop unmounts instead.
    fn spawn(fs: Kernel, dir: &Path) -> Result<fuser::BackgroundSession, String> {
        let mut cfg = Config::default();
        cfg.mount_options = vec![
            MountOption::RO,
            MountOption::FSName("zone-fs".into()),
            MountOption::Subtype("zone".into()),
        ];
        fuser::spawn_mount(fs, dir, &cfg).map_err(|e| format!("mount {}: {e}", dir.display()))
    }

    /// Unmount (lazily, if something still holds the directory) and wait briefly for the session
    /// to end. A session still held open past that ends when this process exits.
    fn unmount(session: fuser::BackgroundSession, dir: &Path) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(session.umount_and_join());
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) | Err(_) => Ok(()),
            Ok(Err(e)) => Err(format!("unmount {}: {e}", dir.display())),
        }
    }

    #[cfg(test)]
    pub(super) fn spawn_for_test(
        client: Arc<FsClient>,
        root: &str,
        dir: &Path,
    ) -> Result<fuser::BackgroundSession, String> {
        spawn(
            Kernel {
                fs: ZoneFs::new(client, root),
                // SAFETY: as in `mount`.
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
            },
            dir,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bp_zone_fs::grant::{GrantSpec, Limits, Subject};
    use bp_zone_fs::proto::{pipe, Pipe};
    use bp_zone_fs::receipt::ReceiptLog;
    use bp_zone_fs::server::ExportConfig;
    use bp_zone_fs::{grant::GrantStore, Actor, Conn, KeySigner, Server};

    use super::*;

    const OWNER: &str = "bp:self:owner";
    const ALY: &str = "bp:self:aly";
    const SECRET: &str = "SECRET_BODY_9d2e";

    /// Counts the frames the client sends: zero means no request left this machine.
    struct Counted {
        inner: Arc<Pipe>,
        sent: Arc<AtomicUsize>,
    }

    impl Conn for Counted {
        fn send(&self, bytes: Vec<u8>) -> std::io::Result<()> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            self.inner.send(bytes)
        }
        fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
            self.inner.recv()
        }
    }

    fn write(root: &Path, rel: &str, body: &str, mode: u32) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    }

    struct Rig {
        dir: tempfile::TempDir,
        server: Arc<Server>,
        signer: Arc<KeySigner>,
        export_id: String,
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "README.md", "hello zone\n", 0o644);
        write(r, "src/lib.rs", "pub fn alpha() {}\n", 0o644);
        write(r, "src/run.sh", "#!/bin/sh\necho hi\n", 0o755);
        write(r, "src/deep/mod.rs", "// deep\n", 0o644);
        write(r, ".gitignore", "target/\n*.log\n", 0o644);
        write(r, "target/out.rs", SECRET, 0o644);
        write(r, "build.log", SECRET, 0o644);
        write(r, ".env", SECRET, 0o644);
        write(r, "server.key", SECRET, 0o644);
        write(r, "private.txt", SECRET, 0o600);
        // Bigger than one 64 KiB frame and one kernel read, with a recognisable pattern.
        let big: String = (0..40_000).map(|i| format!("{:07}\n", i)).collect();
        write(r, "big.txt", &big, 0o644);
        let signer = Arc::new(KeySigner::random(OWNER));
        let server = Server::new(
            "wks_test",
            signer.clone(),
            GrantStore::memory(OWNER),
            ReceiptLog::memory(),
        );
        let cfg = ExportConfig::new("core", r.to_path_buf(), vec![]).unwrap();
        let export_id = cfg.export_id.clone();
        server.add_export(cfg).unwrap();
        Rig {
            dir,
            server: Arc::new(server),
            signer,
            export_id,
        }
    }

    impl Rig {
        /// A grant for ALY over the whole export, and its JWS.
        fn grant(&self, ttl_ms: Option<u64>) -> (String, bp_zone_fs::grant::Grant) {
            self.server
                .grants
                .issue(
                    &*self.signer,
                    &GrantSpec {
                        zone: "wks_test".into(),
                        export_id: self.export_id.clone(),
                        subject: Subject::Members(vec![ALY.into()]),
                        paths: vec![String::new()],
                        ops: ["metadata", "read"].map(String::from).to_vec(),
                        ttl_ms,
                        limits: Limits {
                            metadata_per_hour: 100_000,
                            reads_per_hour: 100_000,
                            searches_per_hour: 100_000,
                            bytes_per_hour: 1 << 40,
                            burst: 10_000,
                        },
                        parent: None,
                    },
                    now_ms(),
                )
                .unwrap()
        }

        /// A client served on a thread, and the count of frames it sends.
        fn connect(&self) -> (Arc<FsClient>, Arc<AtomicUsize>) {
            let (a, b) = pipe();
            let server = self.server.clone();
            std::thread::spawn(move || server.serve(b, &Actor::member(ALY)));
            let sent = Arc::new(AtomicUsize::new(0));
            let conn = Counted {
                inner: a,
                sent: sent.clone(),
            };
            (Arc::new(FsClient::new(Arc::new(conn))), sent)
        }

        fn fs(&self, root: &str) -> (ZoneFs, Arc<AtomicUsize>) {
            let (c, sent) = self.connect();
            (ZoneFs::new(c, root), sent)
        }
    }

    fn names(fs: &ZoneFs, ino: u64) -> Vec<String> {
        let fh = fs.opendir(ino).unwrap();
        let out = fs
            .readdir(fh, 0)
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        fs.releasedir(fh);
        out
    }

    fn read_all(fs: &ZoneFs, ino: u64) -> Vec<u8> {
        let mut out = vec![];
        loop {
            let chunk = fs.read(ino, out.len() as u64, 128 * 1024).unwrap();
            if chunk.is_empty() {
                return out;
            }
            out.extend(chunk);
        }
    }

    #[test]
    fn every_write_op_is_erofs_without_a_request() {
        let r = rig();
        r.grant(None);
        let (fs, sent) = r.fs("core");
        for op in WriteOp::ALL {
            assert_eq!(fs.refuse(op), libc::EROFS, "{op:?}");
        }
        for flags in [
            libc::O_WRONLY,
            libc::O_RDWR,
            libc::O_RDONLY | libc::O_TRUNC,
            libc::O_RDONLY | libc::O_CREAT,
            libc::O_RDONLY | libc::O_APPEND,
        ] {
            assert_eq!(fs.open(ROOT_INO, flags), Err(libc::EROFS), "{flags:#x}");
        }
        assert_eq!(fs.access(libc::W_OK), Err(libc::EROFS));
        assert_eq!(fs.access(libc::R_OK | libc::X_OK), Ok(()));
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn readdir_getattr_and_read_match_the_export() {
        let r = rig();
        r.grant(None);
        let (fs, _) = r.fs("core");
        // Ignored, denied and owner-only files are not listed.
        assert_eq!(
            names(&fs, ROOT_INO),
            [".", "..", ".gitignore", "README.md", "big.txt", "src"]
        );
        let src = fs.lookup(ROOT_INO, "src").unwrap().0;
        assert!(src.dir);
        assert_eq!(names(&fs, src.ino), [".", "..", "deep", "lib.rs", "run.sh"]);
        let run = fs.lookup(src.ino, "run.sh").unwrap().0;
        assert!(run.exec && !run.dir);
        let (readme, ttl) = fs.lookup(ROOT_INO, "README.md").unwrap();
        assert!(ttl <= TTL);
        assert_eq!(readme.size, 11);
        assert_eq!(fs.getattr(readme.ino).unwrap().0, readme);
        assert_eq!(fs.open(readme.ino, libc::O_RDONLY), Ok(()));
        assert_eq!(fs.open(src.ino, libc::O_RDONLY), Err(libc::EISDIR));
        assert_eq!(read_all(&fs, readme.ino), b"hello zone\n");
        assert_eq!(fs.read(readme.ino, 6, 100).unwrap(), b"zone\n");
        assert_eq!(fs.read(readme.ino, 11, 100).unwrap(), b"");
        assert_eq!(fs.read(readme.ino, 50, 100).unwrap(), b"");
        let big = fs.lookup(ROOT_INO, "big.txt").unwrap().0;
        let want = fs::read(r.dir.path().join("big.txt")).unwrap();
        assert_eq!(big.size, want.len() as u64);
        assert_eq!(read_all(&fs, big.ino), want);
        assert_eq!(
            fs.read(big.ino, 70_000, 200_000).unwrap(),
            want[70_000..270_000]
        );
        // The same inode for the same path, every time.
        assert_eq!(fs.lookup(ROOT_INO, "src").unwrap().0.ino, src.ino);
    }

    #[test]
    fn denied_and_ignored_paths_do_not_exist() {
        let r = rig();
        r.grant(None);
        let (fs, _) = r.fs("core");
        for name in [
            ".env",
            "server.key",
            "private.txt",
            "build.log",
            "target",
            "nope",
            "..",
            "a/b",
        ] {
            assert_eq!(
                fs.lookup(ROOT_INO, name).err(),
                Some(libc::ENOENT),
                "{name}"
            );
        }
    }

    #[test]
    fn the_root_may_be_the_export_list_or_a_subdirectory() {
        let r = rig();
        r.grant(None);
        let (fs, _) = r.fs("");
        assert_eq!(names(&fs, ROOT_INO), [".", "..", "core"]);
        let core = fs.lookup(ROOT_INO, "core").unwrap().0;
        assert!(core.dir);
        assert!(names(&fs, core.ino).contains(&"README.md".to_string()));
        let (fs, _) = r.fs("/core/src/");
        let lib = fs.lookup(ROOT_INO, "lib.rs").unwrap().0;
        assert_eq!(read_all(&fs, lib.ino), b"pub fn alpha() {}\n");
    }

    #[test]
    fn metadata_is_reused_for_at_most_a_second() {
        let r = rig();
        r.grant(None);
        let (fs, sent) = r.fs("core");
        let (a, ttl) = fs.lookup(ROOT_INO, "README.md").unwrap();
        assert!(ttl <= TTL);
        let before = sent.load(Ordering::SeqCst);
        // Within the second: from the cache, with less time left, and no request.
        write(r.dir.path(), "README.md", "hello zone, changed\n", 0o644);
        let (b, ttl2) = fs.getattr(a.ino).unwrap();
        assert_eq!(b.size, 11);
        assert!(ttl2 <= ttl);
        assert_eq!(sent.load(Ordering::SeqCst), before);
        // A listing fills the cache for its children.
        names(&fs, ROOT_INO);
        let after_ls = sent.load(Ordering::SeqCst);
        fs.lookup(ROOT_INO, "big.txt").unwrap();
        assert_eq!(sent.load(Ordering::SeqCst), after_ls);
        // After the second: asked again, and the change shows.
        std::thread::sleep(TTL + Duration::from_millis(50));
        let (c, ttl3) = fs.getattr(a.ino).unwrap();
        assert_eq!(c.size, 20);
        assert!(ttl3 > Duration::from_millis(500));
        assert!(sent.load(Ordering::SeqCst) > after_ls);
        // Contents are never cached: each read asks again.
        let n = sent.load(Ordering::SeqCst);
        fs.read(a.ino, 0, 4).unwrap();
        fs.read(a.ino, 0, 4).unwrap();
        assert_eq!(sent.load(Ordering::SeqCst), n + 2);
    }

    #[test]
    fn a_lapsed_grant_fails_the_next_read() {
        let r = rig();
        let (_, g) = r.grant(None);
        let (fs, _) = r.fs("core");
        let readme = fs.lookup(ROOT_INO, "README.md").unwrap().0;
        fs.open(readme.ino, libc::O_RDONLY).unwrap();
        assert_eq!(fs.read(readme.ino, 0, 5).unwrap(), b"hello");
        r.server.grants.revoke(&g.grant_id).unwrap();
        assert_eq!(fs.read(readme.ino, 0, 5), Err(libc::ENOENT));

        // A presented grant that expires says so.
        let (jws, _) = r.grant(Some(300));
        let (c, _) = r.connect();
        c.set_grant(Some(jws));
        let fs = ZoneFs::new(c, "core");
        let readme = fs.lookup(ROOT_INO, "README.md").unwrap().0;
        assert_eq!(fs.read(readme.ino, 0, 5).unwrap(), b"hello");
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(fs.read(readme.ino, 0, 5), Err(libc::EACCES));
    }

    #[test]
    fn refusals_map_to_errno() {
        for (code, want) in [
            (Code::Denied, libc::ENOENT),
            (Code::ExportNotFound, libc::ENOENT),
            (Code::Rate, libc::EAGAIN),
            (Code::GrantExpired, libc::EACCES),
            (Code::GrantRevoked, libc::EACCES),
            (Code::Size, libc::EFBIG),
            (Code::Transport, libc::EIO),
        ] {
            assert_eq!(errno(&FsError::new(code, "")), want, "{code:?}");
        }
    }

    #[test]
    fn available_checks_the_platform_only() {
        // No client exists here: the check cannot have touched the network.
        let fuse = Path::new("/dev/fuse").exists();
        match available() {
            Ok(()) => assert!(cfg!(target_os = "linux") && fuse),
            Err(m) => assert!(m.contains("zone fs mount"), "{m}"),
        }
    }

    /// The real thing, through the kernel: ls, cat, and writes refused with EROFS.
    #[cfg(target_os = "linux")]
    #[test]
    fn mounted_read_only_through_the_kernel() {
        if let Err(why) = available() {
            println!("skipped: {why}");
            return;
        }
        let r = rig();
        r.grant(None);
        let (c, _) = r.connect();
        let mnt = tempfile::tempdir().unwrap();
        let session = match kernel::spawn_for_test(c, "core", mnt.path()) {
            Ok(s) => s,
            Err(why) => {
                println!("skipped: cannot mount here: {why}");
                return;
            }
        };
        let m = mnt.path();
        let started = Instant::now();
        let mut top: Vec<String> = fs::read_dir(m)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let ls_took = started.elapsed();
        top.sort();
        assert_eq!(top, [".gitignore", "README.md", "big.txt", "src"]);
        let started = Instant::now();
        assert_eq!(fs::read(m.join("README.md")).unwrap(), b"hello zone\n");
        let cat_took = started.elapsed();
        assert_eq!(
            fs::read(m.join("big.txt")).unwrap(),
            fs::read(r.dir.path().join("big.txt")).unwrap()
        );
        assert!(fs::metadata(m.join("src")).unwrap().is_dir());
        assert!(!m.join(".env").exists());
        println!("mounted ls {ls_took:?}, cat {cat_took:?}");

        let erofs = |res: std::io::Result<()>, what: &str| {
            let e = res.expect_err(what);
            assert_eq!(e.raw_os_error(), Some(libc::EROFS), "{what}: {e}");
        };
        erofs(fs::File::create(m.join("new.txt")).map(drop), "create");
        erofs(
            fs::OpenOptions::new()
                .write(true)
                .open(m.join("README.md"))
                .map(drop),
            "open for write",
        );
        erofs(fs::write(m.join("README.md"), b"x"), "write");
        erofs(fs::create_dir(m.join("d")), "mkdir");
        erofs(fs::remove_file(m.join("README.md")), "unlink");
        erofs(fs::remove_dir(m.join("src/deep")), "rmdir");
        erofs(fs::rename(m.join("README.md"), m.join("R.md")), "rename");
        erofs(fs::hard_link(m.join("README.md"), m.join("h")), "link");
        erofs(
            std::os::unix::fs::symlink("README.md", m.join("s")),
            "symlink",
        );
        erofs(
            fs::set_permissions(m.join("README.md"), fs::Permissions::from_mode(0o777)),
            "chmod",
        );
        let touch = std::process::Command::new("touch")
            .env("LC_ALL", "C")
            .arg(m.join("README.md"))
            .output()
            .unwrap();
        assert!(!touch.status.success());
        assert!(String::from_utf8_lossy(&touch.stderr).contains("Read-only file system"));
        // Nothing changed on the owner's side.
        assert_eq!(
            fs::read(r.dir.path().join("README.md")).unwrap(),
            b"hello zone\n"
        );
        session.umount_and_join().unwrap();
        assert!(fs::read_dir(m).unwrap().next().is_none());
    }
}
