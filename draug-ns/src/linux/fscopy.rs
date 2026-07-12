//! Copying overlayfs upper layers.
//!
//! An upper layer is not plain data: deletions are char-0:0 whiteout device
//! nodes, replaced directories carry an `overlay.opaque` xattr, and files may
//! be owned by subordinate uids. `copy_tree` preserves all of that. Plain
//! sync code — it also runs inside the tokio-free re-exec copy helper.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{lchown, MetadataExt, PermissionsExt};
use std::path::Path;

/// Recursively copy the layer at `src` into `dst` (created if needed),
/// preserving file modes, ownership, xattrs, symlinks, and device nodes
/// (overlayfs whiteouts). Returns the total regular-file bytes copied.
///
/// Ownership preservation and whiteout creation need privilege over the
/// involved ids: run this as root or inside a user namespace mapping them.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(src)?;
    fs::create_dir_all(dst)?;
    let mut bytes = 0;
    copy_children(src, dst, &mut bytes)?;
    copy_dir_meta(src, dst, &meta)?;
    Ok(bytes)
}

fn copy_children(src: &Path, dst: &Path, bytes: &mut u64) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let meta = entry.metadata()?; // does not follow symlinks
        let s = entry.path();
        let d = dst.join(entry.file_name());
        let ft = meta.file_type();

        if ft.is_dir() {
            fs::create_dir(&d)?;
            copy_children(&s, &d, bytes)?;
            // Metadata last: a mode like 0500 must not block child creation.
            copy_dir_meta(&s, &d, &meta)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&s)?;
            std::os::unix::fs::symlink(&target, &d)?;
            lchown(&d, Some(meta.uid()), Some(meta.gid()))?;
            copy_xattrs(&s, &d)?;
        } else if ft.is_file() {
            *bytes += copy_file(&s, &d, &meta)?;
        } else {
            // Device nodes (incl. char-0:0 overlayfs whiteouts), fifos,
            // sockets: recreate the node itself.
            mknod_like(&d, &meta)?;
            lchown(&d, Some(meta.uid()), Some(meta.gid()))?;
            fs::set_permissions(&d, fs::Permissions::from_mode(meta.mode()))?;
            copy_xattrs(&s, &d)?;
        }
    }
    Ok(())
}

fn copy_dir_meta(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    lchown(dst, Some(meta.uid()), Some(meta.gid()))?;
    fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode()))?;
    copy_xattrs(src, dst) // carries overlay.opaque for replaced dirs
}

fn copy_file(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<u64> {
    let reader = fs::File::open(src)?;
    let writer = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    let copied = copy_file_contents(&reader, &writer, meta.len())?;
    writer.set_permissions(fs::Permissions::from_mode(meta.mode()))?;
    lchown(dst, Some(meta.uid()), Some(meta.gid()))?;
    copy_xattrs(src, dst)?;
    Ok(copied)
}

/// Copy file contents, cheapest mechanism first: FICLONE (a true reflink —
/// instant, shares extents on XFS/btrfs), then `copy_file_range` (in-kernel
/// copy; also reflinks where the fs supports it), then a plain read/write
/// loop for filesystems that support neither.
fn copy_file_contents(reader: &fs::File, writer: &fs::File, len: u64) -> io::Result<u64> {
    const FICLONE: libc::c_ulong = 0x40049409;
    if unsafe { libc::ioctl(writer.as_raw_fd(), FICLONE, reader.as_raw_fd()) } == 0 {
        return Ok(len);
    }

    let mut copied: u64 = 0;
    loop {
        let n = unsafe {
            libc::copy_file_range(
                reader.as_raw_fd(),
                std::ptr::null_mut(),
                writer.as_raw_fd(),
                std::ptr::null_mut(),
                1 << 24,
                0,
            )
        };
        match n {
            0 => return Ok(copied),
            n if n > 0 => copied += n as u64,
            _ => {
                let err = io::Error::last_os_error();
                // Unsupported (or cross-device on pre-5.3 kernels): fall
                // back to userspace copying, but only if nothing has been
                // copied yet — a mid-stream failure is a real error.
                let fallback = matches!(
                    err.raw_os_error(),
                    Some(libc::EINVAL | libc::EXDEV | libc::ENOSYS | libc::EOPNOTSUPP)
                );
                if fallback && copied == 0 {
                    let mut r = io::BufReader::new(reader);
                    let mut w = io::BufWriter::new(writer);
                    return io::copy(&mut r, &mut w);
                }
                return Err(err);
            }
        }
    }
}

/// Recreate a device node / fifo / socket with the same type and rdev.
/// Whiteouts (char 0:0) need no privilege on kernels >= 5.8; real device
/// nodes need CAP_MKNOD over the filesystem (root, in practice).
fn mknod_like(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let cpath = cstring(path)?;
    let mode = meta.mode() as libc::mode_t;
    if unsafe { libc::mknod(cpath.as_ptr(), mode, meta.rdev() as libc::dev_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// --- xattrs ------------------------------------------------------------------
//
// Overlay metadata lives in xattrs: `{trusted,user}.overlay.opaque` marks a
// directory as fully replacing the base's. Losing one silently corrupts a
// snapshot, so failures on overlay.* names are hard errors; other names
// (security.selinux, acls, ...) are copied best-effort.

pub fn copy_xattrs(src: &Path, dst: &Path) -> io::Result<()> {
    for name in list_xattrs(src)? {
        let Some(value) = get_xattr(src, &name)? else {
            continue; // removed between list and get
        };
        if let Err(e) = set_xattr(dst, &name, &value) {
            if name.starts_with("user.overlay.") || name.starts_with("trusted.overlay.") {
                return Err(e);
            }
        }
    }
    Ok(())
}

fn list_xattrs(path: &Path) -> io::Result<Vec<String>> {
    let cpath = cstring(path)?;
    let size = unsafe { libc::llistxattr(cpath.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        let err = io::Error::last_os_error();
        // Symlinks/devices on some filesystems don't do xattrs at all.
        return match err.raw_os_error() {
            Some(libc::EOPNOTSUPP) => Ok(vec![]),
            _ => Err(err),
        };
    }
    let mut buf = vec![0u8; size as usize];
    let size = unsafe {
        libc::llistxattr(cpath.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len())
    };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(size as usize);
    Ok(buf
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect())
}

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

fn set_xattr(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let cpath = cstring(path)?;
    let cname = CString::new(name).map_err(|_| io::ErrorKind::InvalidInput)?;
    let r = unsafe {
        libc::lsetxattr(
            cpath.as_ptr(),
            cname.as_ptr(),
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

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, FileTypeExt};

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
        assert_eq!(
            fs::read_link(dst.join("link")).unwrap(),
            Path::new("a.txt")
        );
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
}
