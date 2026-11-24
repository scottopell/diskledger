//! External OS/APFS/clock boundary so scanning and state transitions can run
//! deterministically against an in-memory simulator as well as real macOS APFS.

use crate::kstat::{self, DirStat};
use crate::util;
use std::fs;
use std::io;
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
        let rd = fs::read_dir(path)?;
        let mut out = DirListing::default();
        for ent in rd.flatten() {
            if ent.file_name() == ".git" {
                out.has_git = true;
            }
            // file_type() does not follow symlinks.
            if ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.subdirs.push(ent.path());
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
