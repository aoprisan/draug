//! Structural diff of an overlayfs upper layer against its base (lowerdir).
//!
//! The upper layer records only deltas, but not as plain files: a deletion
//! is a character device with rdev 0:0 (a "whiteout"), and a directory that
//! fully replaces the base's carries an `{trusted,user}.overlay.opaque`
//! xattr. The walker decodes both instead of reporting them literally.

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

use draug_core::{DiffEntry, DiffKind, Error, Result};

use super::fscopy::get_xattr;

/// Walk the layer at `upper` and report every path it adds, modifies, or
/// deletes relative to the base image at `base`. Directories present on
/// both sides are traversal structure, not changes, and are not reported.
/// Entries come back sorted by path.
pub fn diff_upper(upper: &Path, base: &Path) -> Result<Vec<DiffEntry>> {
    let mut out = Vec::new();
    walk(upper, base, "", &mut out).map_err(|e| Error::io("diff upper layer", e))?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(upper: &Path, base: &Path, rel: &str, out: &mut Vec<DiffEntry>) -> std::io::Result<()> {
    for entry in fs::read_dir(upper)? {
        let entry = entry?;
        let name = entry.file_name();
        let upath = entry.path();
        let bpath = base.join(&name);
        let rel_path = join_rel(rel, &name.to_string_lossy());
        let umeta = entry.metadata()?; // does not follow symlinks
        let bmeta = fs::symlink_metadata(&bpath).ok();

        if is_whiteout(&umeta) {
            // A whiteout with nothing under it in the base is overlay
            // bookkeeping (e.g. re-deleting an already-absent path), not a
            // change. When it covers a base directory, the whole subtree is
            // gone.
            if let Some(bm) = bmeta {
                push_deleted(&bpath, rel_path, &bm, out)?;
            }
            continue;
        }

        if umeta.is_dir() {
            match &bmeta {
                Some(bm) if bm.is_dir() => {
                    // Merged dir: only a change if marked opaque, in which
                    // case base children missing from the upper are gone.
                    if is_opaque(&upath)? {
                        report_shadowed(&upath, &bpath, &rel_path, out)?;
                    }
                }
                Some(_) => {
                    // Non-dir replaced by a dir.
                    out.push(entry_for(rel_path.clone(), DiffKind::Modified, &umeta));
                }
                None => out.push(entry_for(rel_path.clone(), DiffKind::Added, &umeta)),
            }
            walk(&upath, &bpath, &rel_path, out)?;
        } else {
            match &bmeta {
                Some(bm) if bm.is_dir() => {
                    // Dir replaced by a non-dir: the path changed type and
                    // everything that lived under the old directory is gone.
                    out.push(entry_for(rel_path.clone(), DiffKind::Modified, &umeta));
                    for child in fs::read_dir(&bpath)? {
                        let child = child?;
                        push_deleted(
                            &child.path(),
                            join_rel(&rel_path, &child.file_name().to_string_lossy()),
                            &child.metadata()?,
                            out,
                        )?;
                    }
                }
                Some(_) => out.push(entry_for(rel_path, DiffKind::Modified, &umeta)),
                None => out.push(entry_for(rel_path, DiffKind::Added, &umeta)),
            }
        }
    }
    Ok(())
}

/// For an opaque upper dir: every base child with no upper entry at all is
/// shadowed away — report it deleted. (Base children that the upper whites
/// out or replaces are handled by the main walk.)
fn report_shadowed(
    upper: &Path,
    base: &Path,
    rel: &str,
    out: &mut Vec<DiffEntry>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name();
        if fs::symlink_metadata(upper.join(&name)).is_err() {
            push_deleted(
                &entry.path(),
                join_rel(rel, &name.to_string_lossy()),
                &entry.metadata()?,
                out,
            )?;
        }
    }
    Ok(())
}

/// Emit a `Deleted` entry for the base path `bpath` (rel `rel`), and — when
/// it is a directory — for every descendant, so a removed subtree is
/// reported at file granularity rather than as one opaque directory.
fn push_deleted(
    bpath: &Path,
    rel: String,
    bmeta: &fs::Metadata,
    out: &mut Vec<DiffEntry>,
) -> std::io::Result<()> {
    out.push(entry_for(rel.clone(), DiffKind::Deleted, bmeta));
    if bmeta.is_dir() {
        for child in fs::read_dir(bpath)? {
            let child = child?;
            push_deleted(
                &child.path(),
                join_rel(&rel, &child.file_name().to_string_lossy()),
                &child.metadata()?,
                out,
            )?;
        }
    }
    Ok(())
}

fn is_whiteout(meta: &fs::Metadata) -> bool {
    meta.file_type().is_char_device() && meta.rdev() == 0
}

/// Opaque marker: `overlay.opaque = "y"` in the trusted (mounted by root)
/// or user (rootless, `userxattr`) namespace.
fn is_opaque(dir: &Path) -> std::io::Result<bool> {
    for name in ["trusted.overlay.opaque", "user.overlay.opaque"] {
        if let Some(v) = get_xattr(dir, name)? {
            if v == b"y" {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn entry_for(path: String, kind: DiffKind, meta: &fs::Metadata) -> DiffEntry {
    DiffEntry {
        path,
        kind,
        // Directory sizes are fs-internal noise; report None.
        size: (!meta.is_dir()).then_some(meta.len()),
        mode: Some(format!("{:o}", meta.permissions().mode())),
    }
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// Create a char-0:0 whiteout node; returns false if the host forbids
    /// mknod (unprivileged, no CAP_MKNOD) so the test can skip.
    fn whiteout(path: &Path) -> bool {
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR, 0) == 0 }
    }

    fn kinds(entries: &[DiffEntry]) -> HashMap<String, DiffKind> {
        entries.iter().map(|e| (e.path.clone(), e.kind)).collect()
    }

    #[test]
    fn added_modified_and_directory_whiteout() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let upper = tmp.path().join("upper");
        fs::create_dir_all(base.join("keepdir")).unwrap();
        fs::create_dir_all(base.join("gonedir/nested")).unwrap();
        fs::write(base.join("edit.txt"), "old").unwrap();
        fs::write(base.join("keepdir/stable.txt"), "x").unwrap();
        fs::write(base.join("gonedir/a.txt"), "a").unwrap();
        fs::write(base.join("gonedir/nested/b.txt"), "b").unwrap();

        fs::create_dir_all(&upper).unwrap();
        fs::write(upper.join("edit.txt"), "brand new longer").unwrap();
        fs::write(upper.join("added.txt"), "hi").unwrap();
        if !whiteout(&upper.join("gonedir")) {
            eprintln!("skipping directory-whiteout test: mknod not permitted");
            return;
        }

        let entries = diff_upper(&upper, &base).unwrap();
        let k = kinds(&entries);
        assert_eq!(k.get("added.txt"), Some(&DiffKind::Added));
        assert_eq!(k.get("edit.txt"), Some(&DiffKind::Modified));
        // The whited-out directory and its whole subtree are deletions.
        assert_eq!(k.get("gonedir"), Some(&DiffKind::Deleted));
        assert_eq!(k.get("gonedir/a.txt"), Some(&DiffKind::Deleted));
        assert_eq!(k.get("gonedir/nested"), Some(&DiffKind::Deleted));
        assert_eq!(k.get("gonedir/nested/b.txt"), Some(&DiffKind::Deleted));
        // An unchanged base dir/file is not reported.
        assert!(!k.contains_key("keepdir"));
        assert!(!k.contains_key("keepdir/stable.txt"));

        // The modified file's size/mode reflect the upper's version.
        let edit = entries.iter().find(|e| e.path == "edit.txt").unwrap();
        assert_eq!(edit.size, Some("brand new longer".len() as u64));
    }

    #[test]
    fn opaque_directory_shadows_base_children() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let upper = tmp.path().join("upper");
        fs::create_dir_all(base.join("d")).unwrap();
        fs::write(base.join("d/old.txt"), "old").unwrap();
        fs::write(base.join("d/keep.txt"), "keep").unwrap();

        fs::create_dir_all(upper.join("d")).unwrap();
        fs::write(upper.join("d/new.txt"), "new").unwrap();
        // Re-add keep.txt in the upper so it survives the opaque replacement.
        fs::write(upper.join("d/keep.txt"), "keep2").unwrap();
        let c = CString::new(upper.join("d").as_os_str().as_bytes()).unwrap();
        let name = CString::new("user.overlay.opaque").unwrap();
        let set = unsafe {
            libc::lsetxattr(
                c.as_ptr(),
                name.as_ptr(),
                b"y".as_ptr() as *const libc::c_void,
                1,
                0,
            )
        };
        if set != 0 {
            eprintln!("skipping opaque-dir test: cannot set overlay xattr here");
            return;
        }

        let k = kinds(&diff_upper(&upper, &base).unwrap());
        assert_eq!(k.get("d/new.txt"), Some(&DiffKind::Added));
        assert_eq!(k.get("d/keep.txt"), Some(&DiffKind::Modified));
        // old.txt is present in base, absent from the opaque upper dir → gone.
        assert_eq!(k.get("d/old.txt"), Some(&DiffKind::Deleted));
    }
}
