//! External OS/APFS/clock boundary so scanning and state transitions can run
//! deterministically against an in-memory simulator as well as real macOS APFS.

use crate::kstat::{self, DirStat};
use crate::util;
use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct DirListing {
    /// Immediate non-symlink subdirectories (empty when `too_many_children` is true).
    pub subdirs: Vec<PathBuf>,
    /// True when the directory had more than `max_children` subdirectories.
    pub too_many_children: bool,
    /// True when a direct `.git` entry (file or directory) exists inside this directory.
    pub has_git: bool,
    /// True when the directory contains at least one direct non-directory, non-symlink entry
    /// (e.g. a regular file, FIFO, or unknown entry type).
    pub has_direct_files: bool,
}

pub trait System: Send + Sync {
    fn query(&self, path: &Path) -> io::Result<DirStat>;
    fn flag(&self, path: &Path) -> io::Result<()>;
    fn volume_space(&self, path: &Path) -> io::Result<(u64, u64)>;
    /// If `path` is a directory (following symlinks at the root), return `Ok(Some(dev))`.
    /// If it exists and is not a directory, return `Ok(None)`.
    fn dir_dev(&self, path: &Path) -> io::Result<Option<u64>>;
    /// Device ID without following symlinks.
    fn symlink_dev(&self, path: &Path) -> io::Result<u64>;
    /// List immediate non-symlink subdirectories (up to `max_children`) and detect direct `.git`.
    fn list_dir(&self, path: &Path, max_children: usize) -> io::Result<DirListing>;
    /// True when `path/.git` exists (file or directory).
    fn has_git(&self, path: &Path) -> bool;
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

    fn dir_dev(&self, path: &Path) -> io::Result<Option<u64>> {
        let m = fs::metadata(path)?;
        if m.is_dir() {
            Ok(Some(m.dev()))
        } else {
            Ok(None)
        }
    }

    fn symlink_dev(&self, path: &Path) -> io::Result<u64> {
        Ok(fs::symlink_metadata(path)?.dev())
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
                out.has_git = true;
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
                    if !out.has_git {
                        out.has_git = path.join(".git").exists();
                    }
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    fn has_git(&self, path: &Path) -> bool {
        path.join(".git").exists()
    }

    fn now(&self) -> u64 {
        util::now()
    }
}
