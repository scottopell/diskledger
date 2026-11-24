//! External OS/APFS/clock boundary so scanning and state transitions can run
//! deterministically against an in-memory simulator as well as real macOS APFS.

use crate::kstat::{self, DirStat, VolumeInfo};
use crate::store::GitKind;
use crate::util;
use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirMeta {
    pub dev: u64,
    pub mtime: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub dev: u64,
    /// Allocated bytes (`blocks * 512`) for non-directory entries; 0 for directories.
    pub file_bytes: u64,
}

#[derive(Debug, Default)]
pub struct DirListing {
    /// Immediate non-symlink subdirectories (empty when `too_many_children` is true).
    pub subdirs: Vec<PathBuf>,
    /// True when the directory had more than `max_children` subdirectories.
    pub too_many_children: bool,
    /// Direct `.git` classification observed inside this directory.
    pub git_kind: GitKind,
    /// True when the directory contains at least one direct non-directory, non-symlink entry
    /// (e.g. a regular file, FIFO, or unknown entry type).
    pub has_direct_files: bool,
}

pub trait System: Send + Sync {
    fn query(&self, path: &Path) -> io::Result<DirStat>;
    fn flag(&self, path: &Path) -> io::Result<()>;
    fn volume_space(&self, path: &Path) -> io::Result<(u64, u64)>;
    fn volume_info(&self, path: &Path) -> io::Result<VolumeInfo> {
        let (free, total) = self.volume_space(path)?;
        Ok(VolumeInfo { free, total, ..Default::default() })
    }
    /// If `path` is a directory (following symlinks at the root), return `Ok(Some(DirMeta))`.
    /// If it exists and is not a directory, return `Ok(None)`.
    fn dir_meta(&self, path: &Path) -> io::Result<Option<DirMeta>>;
    /// Convenience wrapper for `dir_meta(path).map(|o| o.map(|m| m.dev))`.
    fn dir_dev(&self, path: &Path) -> io::Result<Option<u64>> {
        Ok(self.dir_meta(path)?.map(|m| m.dev))
    }
    /// Device ID and mtime without following symlinks.
    fn symlink_meta(&self, path: &Path) -> io::Result<DirMeta>;
    /// List immediate non-symlink subdirectories (up to `max_children`) and detect direct `.git`.
    fn list_dir(&self, path: &Path, max_children: usize) -> io::Result<DirListing>;
    /// List immediate entries (directories, files, symlinks) with device ID and non-dir allocated bytes.
    fn list_entries(&self, path: &Path) -> io::Result<Vec<DirectEntry>>;
    /// Classify `path/.git` (Repo if directory, Worktree if regular file, else None).
    fn git_kind(&self, path: &Path) -> GitKind;
    /// Current Unix timestamp in seconds.
    fn now(&self) -> u64;
}

pub struct RealSystem;

impl System for RealSystem {
    fn query(&self, path: &Path) -> io::Result<DirStat> {
        kstat::query(path)
    }

    fn flag(&self, path: &Path) -> io::Result<()> {
        kstat::flag(path)
    }

    fn volume_space(&self, path: &Path) -> io::Result<(u64, u64)> {
        kstat::volume_space(path)
    }

    fn volume_info(&self, path: &Path) -> io::Result<VolumeInfo> {
        kstat::volume_info(path)
    }

    fn dir_meta(&self, path: &Path) -> io::Result<Option<DirMeta>> {
        let m = fs::metadata(path)?;
        if m.is_dir() {
            Ok(Some(DirMeta {
                dev: m.dev(),
                mtime: m.mtime().max(0) as u64,
            }))
        } else {
            Ok(None)
        }
    }

    fn symlink_meta(&self, path: &Path) -> io::Result<DirMeta> {
        let m = fs::symlink_metadata(path)?;
        Ok(DirMeta {
            dev: m.dev(),
            mtime: m.mtime().max(0) as u64,
        })
    }

    fn list_dir(&self, path: &Path, max_children: usize) -> io::Result<DirListing> {
        struct DirGuard(*mut libc::DIR);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                unsafe { libc::closedir(self.0) };
            }
        }

        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let raw = unsafe { libc::opendir(c.as_ptr()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let _guard = DirGuard(raw);
        let mut out = DirListing::default();

        loop {
            let ent = unsafe { libc::readdir(raw) };
            if ent.is_null() {
                break;
            }
            let (name_bytes, d_type) = unsafe {
                let d = &*ent;
                let bytes = CStr::from_ptr(d.d_name.as_ptr()).to_bytes();
                (bytes, d.d_type)
            };
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            if name_bytes == b".git" {
                out.git_kind = match d_type {
                    libc::DT_DIR => GitKind::Repo,
                    libc::DT_REG => GitKind::Worktree,
                    _ => self.git_kind(path),
                };
            }
            let is_dir = match d_type {
                libc::DT_DIR => true,
                libc::DT_LNK => false,
                libc::DT_UNKNOWN => {
                    let child = path.join(OsStr::from_bytes(name_bytes));
                    match fs::symlink_metadata(&child) {
                        Ok(m) if m.is_dir() => true,
                        Ok(m) if m.file_type().is_symlink() => false,
                        _ => {
                            out.has_direct_files = true;
                            false
                        }
                    }
                }
                _ => {
                    out.has_direct_files = true;
                    false
                }
            };
            if is_dir {
                out.subdirs.push(path.join(OsStr::from_bytes(name_bytes)));
                if out.subdirs.len() > max_children {
                    out.subdirs.clear();
                    out.too_many_children = true;
                    if !out.git_kind.is_git() {
                        out.git_kind = self.git_kind(path);
                    }
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    fn list_entries(&self, path: &Path) -> io::Result<Vec<DirectEntry>> {
        struct DirGuard(*mut libc::DIR);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                unsafe { libc::closedir(self.0) };
            }
        }

        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let raw = unsafe { libc::opendir(c.as_ptr()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let _guard = DirGuard(raw);
        let dirfd = unsafe { libc::dirfd(raw) };
        let mut out = Vec::new();

        loop {
            let ent = unsafe { libc::readdir(raw) };
            if ent.is_null() {
                break;
            }
            let (name_ptr, name_bytes) = unsafe {
                let d = &*ent;
                let ptr = d.d_name.as_ptr();
                (ptr, CStr::from_ptr(ptr).to_bytes())
            };
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            let child_path = path.join(OsStr::from_bytes(name_bytes));
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let rc = if dirfd >= 0 {
                unsafe { libc::fstatat(dirfd, name_ptr, &mut st, libc::AT_SYMLINK_NOFOLLOW) }
            } else {
                let cc = match CString::new(child_path.as_os_str().as_bytes()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                unsafe { libc::lstat(cc.as_ptr(), &mut st) }
            };
            if rc != 0 {
                continue;
            }
            let mode = st.st_mode & libc::S_IFMT;
            let is_dir = mode == libc::S_IFDIR;
            let is_symlink = mode == libc::S_IFLNK;
            let file_bytes = if is_dir {
                0
            } else {
                (st.st_blocks.max(0) as u64).saturating_mul(512)
            };
            out.push(DirectEntry {
                path: child_path,
                is_dir,
                is_symlink,
                dev: st.st_dev as u64,
                file_bytes,
            });
        }
        Ok(out)
    }

    fn git_kind(&self, path: &Path) -> GitKind {
        match fs::metadata(path.join(".git")) {
            Ok(m) if m.is_dir() => GitKind::Repo,
            Ok(_) => GitKind::Worktree,
            Err(_) => GitKind::None,
        }
    }

    fn now(&self) -> u64 {
        util::now()
    }
}
