//! Copying overlayfs upper layers.
//!
//! An upper layer is not plain data: deletions are char-0:0 whiteout device
//! nodes, replaced directories carry an `overlay.opaque` xattr, and files may
//! be owned by subordinate uids. `copy_tree` preserves all of that. Plain
//! sync code — it also runs inside the tokio-free re-exec copy helper.
//!
//! **TOCTOU safety.** The source layer is a live sandbox's writable directory:
//! its own processes can be mutating it during the copy (the cgroup freeze is
//! best-effort and a no-op without cgroup v2). So the walk never re-opens by
//! full path — it descends through directory file descriptors with `openat`
//! and `O_NOFOLLOW`. A directory component swapped for a symlink between the
//! `fstatat` and the `openat` makes the `openat` fail (ELOOP/ENOTDIR) rather
//! than redirect the copy outside the layer.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Recursively copy the layer at `src` into `dst` (created if needed),
/// preserving file modes, ownership, xattrs, symlinks, and device nodes
/// (overlayfs whiteouts). Returns the total regular-file bytes copied.
///
/// Ownership preservation and whiteout creation need privilege over the
/// involved ids: run this as root or inside a user namespace mapping them.
/// A privilege failure surfaces as `PermissionDenied` so the caller can retry
/// via the user-namespace copy helper.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<u64> {
    std::fs::create_dir_all(dst)?;
    // The two roots are paths we control; opening them by path (following
    // their own components) is fine. Everything below descends by fd.
    let src_root = open_dir_path(src)?;
    let dst_root = open_dir_path(dst)?;
    let mut bytes = 0;
    copy_dir(src_root.as_raw_fd(), dst_root.as_raw_fd(), &mut bytes)?;
    // Apply the root's own mode/owner/xattrs after its children exist.
    let st = fstat(src_root.as_raw_fd())?;
    set_fd_owner_mode(dst_root.as_raw_fd(), &st)?;
    copy_xattrs_fd(src_root.as_raw_fd(), dst_root.as_raw_fd())?;
    Ok(bytes)
}

/// Copy every entry of the directory `src_fd` into `dst_fd`.
fn copy_dir(src_fd: RawFd, dst_fd: RawFd, bytes: &mut u64) -> io::Result<()> {
    for name in read_entries(src_fd)? {
        let st = fstatat_nofollow(src_fd, &name)?;
        match st.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                // Create with permissive temp perms so a restrictive real mode
                // (e.g. 0500) doesn't block populating the children.
                mkdirat(dst_fd, &name, 0o700)?;
                let cs = openat_dir(src_fd, &name)?; // O_NOFOLLOW inside
                let cd = openat_dir(dst_fd, &name)?;
                copy_dir(cs.as_raw_fd(), cd.as_raw_fd(), bytes)?;
                set_fd_owner_mode(cd.as_raw_fd(), &st)?; // carries overlay.opaque dirs' mode
                copy_xattrs_fd(cs.as_raw_fd(), cd.as_raw_fd())?;
            }
            libc::S_IFLNK => {
                let target = readlinkat(src_fd, &name, st.st_size)?;
                symlinkat(&target, dst_fd, &name)?;
                fchownat_nofollow(dst_fd, &name, st.st_uid, st.st_gid)?;
                // Symlinks carry no overlay metadata; skip their xattrs.
            }
            libc::S_IFREG => {
                let sfd = openat_file(src_fd, &name, libc::O_RDONLY)?;
                let dfd = openat_file(dst_fd, &name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
                *bytes += copy_file_contents(sfd.as_raw_fd(), dfd.as_raw_fd(), st.st_size as u64)?;
                set_fd_owner_mode(dfd.as_raw_fd(), &st)?;
                copy_xattrs_fd(sfd.as_raw_fd(), dfd.as_raw_fd())?; // metacopy files
            }
            _ => {
                // Device nodes (incl. char-0:0 overlayfs whiteouts), fifos,
                // sockets: recreate the node itself, no content.
                mknodat(dst_fd, &name, st.st_mode, st.st_rdev)?;
                fchownat_nofollow(dst_fd, &name, st.st_uid, st.st_gid)?;
                fchmodat_nofollow(dst_fd, &name, st.st_mode & 0o7777)?;
            }
        }
    }
    Ok(())
}

// --- fd-relative syscall wrappers --------------------------------------------

fn open_dir_path(path: &Path) -> io::Result<OwnedFd> {
    let c = cstring(path)?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    checked_fd(fd)
}

fn openat_dir(parent: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    checked_fd(fd)
}

fn openat_file(parent: RawFd, name: &CStr, flags: i32) -> io::Result<OwnedFd> {
    // O_NOFOLLOW: if the entry was swapped for a symlink after we stat'd it as
    // a regular file, fail rather than follow it out of the tree. 0o600 is a
    // placeholder for O_CREAT; the real mode is applied via fchmod afterwards.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    checked_fd(fd)
}

fn checked_fd(fd: RawFd) -> io::Result<OwnedFd> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn fstatat_nofollow(dirfd: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dirfd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

/// Read all entry names of `dirfd` (excluding `.`/`..`) up front, so we don't
/// interleave `readdir` with mutations of the same directory stream.
fn read_entries(dirfd: RawFd) -> io::Result<Vec<CString>> {
    // fdopendir takes ownership of the fd it's given; dup so the caller's
    // dirfd stays valid after closedir.
    let dup = unsafe { libc::dup(dirfd) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    let dirp = unsafe { libc::fdopendir(dup) };
    if dirp.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let mut names = Vec::new();
    loop {
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            break; // end of stream (readdir leaves errno unchanged at EOD)
        }
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(name.to_owned());
    }
    unsafe { libc::closedir(dirp) }; // closes `dup`
    Ok(names)
}

fn mkdirat(dirfd: RawFd, name: &CStr, mode: libc::mode_t) -> io::Result<()> {
    if unsafe { libc::mkdirat(dirfd, name.as_ptr(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mknodat(dirfd: RawFd, name: &CStr, mode: libc::mode_t, rdev: libc::dev_t) -> io::Result<()> {
    if unsafe { libc::mknodat(dirfd, name.as_ptr(), mode, rdev) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn symlinkat(target: &CStr, dirfd: RawFd, name: &CStr) -> io::Result<()> {
    if unsafe { libc::symlinkat(target.as_ptr(), dirfd, name.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn readlinkat(dirfd: RawFd, name: &CStr, size_hint: i64) -> io::Result<CString> {
    let mut cap = if size_hint > 0 { size_hint as usize + 1 } else { 256 };
    loop {
        let mut buf = vec![0u8; cap];
        let n = unsafe {
            libc::readlinkat(
                dirfd,
                name.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        if n < buf.len() {
            buf.truncate(n);
            return CString::new(buf)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "symlink target has NUL"));
        }
        cap *= 2; // truncated; grow and retry
    }
}

fn fchownat_nofollow(dirfd: RawFd, name: &CStr, uid: libc::uid_t, gid: libc::gid_t) -> io::Result<()> {
    if unsafe { libc::fchownat(dirfd, name.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fchmodat_nofollow(dirfd: RawFd, name: &CStr, mode: libc::mode_t) -> io::Result<()> {
    // The node was just created by us and is not a symlink, so a plain
    // fchmodat (flags 0) cannot be redirected.
    if unsafe { libc::fchmodat(dirfd, name.as_ptr(), mode, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Set owner and mode on an open dir/regular-file fd (cannot be redirected).
fn set_fd_owner_mode(fd: RawFd, st: &libc::stat) -> io::Result<()> {
    if unsafe { libc::fchown(fd, st.st_uid, st.st_gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fchmod(fd, st.st_mode & 0o7777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// --- file contents -----------------------------------------------------------

/// Copy file contents between two open fds, cheapest mechanism first: FICLONE
/// (a true reflink — instant, shares extents on XFS/btrfs), then
/// `copy_file_range` (in-kernel copy; also reflinks where supported), then a
/// plain read/write loop for filesystems that support neither.
fn copy_file_contents(reader: RawFd, writer: RawFd, len: u64) -> io::Result<u64> {
    const FICLONE: libc::c_ulong = 0x40049409;
    if unsafe { libc::ioctl(writer, FICLONE, reader) } == 0 {
        return Ok(len);
    }

    let mut copied: u64 = 0;
    loop {
        let n = unsafe {
            libc::copy_file_range(reader, std::ptr::null_mut(), writer, std::ptr::null_mut(), 1 << 24, 0)
        };
        match n {
            0 => return Ok(copied),
            n if n > 0 => copied += n as u64,
            _ => {
                let err = io::Error::last_os_error();
                // Unsupported (or cross-device on pre-5.3 kernels): fall back
                // to userspace copying, but only if nothing has been copied
                // yet — a mid-stream failure is a real error.
                let fallback = matches!(
                    err.raw_os_error(),
                    Some(libc::EINVAL | libc::EXDEV | libc::ENOSYS | libc::EOPNOTSUPP)
                );
                if fallback && copied == 0 {
                    return copy_loop(reader, writer);
                }
                return Err(err);
            }
        }
    }
}

fn copy_loop(reader: RawFd, writer: RawFd) -> io::Result<u64> {
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = unsafe { libc::read(reader, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(total);
        }
        let n = n as usize;
        let mut off = 0;
        while off < n {
            let w = unsafe {
                libc::write(
                    writer,
                    buf[off..].as_ptr() as *const libc::c_void,
                    n - off,
                )
            };
            if w < 0 {
                return Err(io::Error::last_os_error());
            }
            off += w as usize;
        }
        total += n as u64;
    }
}

// --- xattrs ------------------------------------------------------------------
//
// Overlay metadata lives in xattrs: `{trusted,user}.overlay.opaque` marks a
// directory as fully replacing the base's; metacopy/redirect appear on dirs
// and regular files. All carriers are dirs or regular files, for which we hold
// a real fd — so we copy via the f*xattr syscalls (no path re-resolution, no
// TOCTOU). Losing an overlay.* xattr silently corrupts a snapshot, so those
// are hard errors; other names (SELinux labels, ACLs, ...) are best-effort.

fn copy_xattrs_fd(src_fd: RawFd, dst_fd: RawFd) -> io::Result<()> {
    for name in flistxattr(src_fd)? {
        let Some(value) = fgetxattr(src_fd, &name)? else {
            continue; // removed between list and get
        };
        if let Err(e) = fsetxattr(dst_fd, &name, &value) {
            let n = name.to_bytes();
            if n.starts_with(b"user.overlay.") || n.starts_with(b"trusted.overlay.") {
                return Err(e);
            }
        }
    }
    Ok(())
}

fn flistxattr(fd: RawFd) -> io::Result<Vec<CString>> {
    let size = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
    if size < 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::EOPNOTSUPP) => Ok(vec![]),
            _ => Err(err),
        };
    }
    let mut buf = vec![0u8; size as usize];
    let size = unsafe { libc::flistxattr(fd, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(size as usize);
    Ok(buf
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| CString::new(s).ok())
        .collect())
}

fn fgetxattr(fd: RawFd, name: &CStr) -> io::Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; 256];
    loop {
        let size = unsafe {
            libc::fgetxattr(
                fd,
                name.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if size >= 0 {
            buf.truncate(size as usize);
            return Ok(Some(buf));
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENODATA) | Some(libc::EOPNOTSUPP) => return Ok(None),
            Some(libc::ERANGE) => buf.resize(buf.len() * 2, 0),
            _ => return Err(io::Error::last_os_error()),
        }
    }
}

fn fsetxattr(fd: RawFd, name: &CStr, value: &[u8]) -> io::Result<()> {
    let r = unsafe {
        libc::fsetxattr(
            fd,
            name.as_ptr(),
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// --- path-based xattr read (for the diff walker, not the copy) ---------------

/// Read one xattr by path (follows nothing on the final component via
/// `lgetxattr`). Used by the diff walker, which walks by path.
pub fn get_xattr(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    let cpath = cstring(path)?;
    let cname = CString::new(name).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut buf = vec![0u8; 256];
    loop {
        let size = unsafe {
            libc::lgetxattr(
                cpath.as_ptr(),
                cname.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if size >= 0 {
            buf.truncate(size as usize);
            return Ok(Some(buf));
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENODATA) | Some(libc::EOPNOTSUPP) => return Ok(None),
            Some(libc::ERANGE) => buf.resize(buf.len() * 2, 0),
            _ => return Err(io::Error::last_os_error()),
        }
    }
}

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt};

    #[test]
    fn copies_files_symlinks_and_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("sub/b.txt"), b"world!").unwrap();
        fs::set_permissions(src.join("a.txt"), fs::Permissions::from_mode(0o600)).unwrap();
        symlink("a.txt", src.join("link")).unwrap();

        let bytes = copy_tree(&src, &dst).unwrap();
        assert_eq!(bytes, 11);
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("sub/b.txt")).unwrap(), b"world!");
        assert_eq!(
            fs::symlink_metadata(dst.join("a.txt")).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_link(dst.join("link")).unwrap(), Path::new("a.txt"));
    }

    #[test]
    fn copies_whiteouts() {
        // Whiteout creation is unprivileged on >= 5.8; skip where it isn't.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let meta = {
            let cpath = cstring(&src.join("gone")).unwrap();
            if unsafe { libc::mknod(cpath.as_ptr(), libc::S_IFCHR, 0) } != 0 {
                eprintln!("skipping whiteout copy test: mknod not permitted");
                return;
            }
            fs::symlink_metadata(src.join("gone")).unwrap()
        };
        assert!(meta.file_type().is_char_device());

        let dst = tmp.path().join("dst");
        copy_tree(&src, &dst).unwrap();
        let copied = fs::symlink_metadata(dst.join("gone")).unwrap();
        assert!(copied.file_type().is_char_device());
        assert_eq!(copied.rdev(), 0);
    }

    #[test]
    fn preserves_directory_mode_and_nesting() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("a/b/c")).unwrap();
        fs::write(src.join("a/b/c/deep.txt"), b"x").unwrap();
        // A restrictive dir mode must not block copying its children.
        fs::set_permissions(src.join("a/b"), fs::Permissions::from_mode(0o500)).unwrap();

        copy_tree(&src, &dst).unwrap();
        assert_eq!(fs::read(dst.join("a/b/c/deep.txt")).unwrap(), b"x");
        assert_eq!(
            fs::symlink_metadata(dst.join("a/b")).unwrap().mode() & 0o777,
            0o500
        );
    }
}
