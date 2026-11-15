//! Walk the monitored roots, record directory sizes, and (optionally) give
//! large directories their own kernel record so later reads are instant.
//!
//! Cost model:
//! - A directory with a kernel record answers in microseconds.
//! - A directory without one makes the kernel walk its subtree.
//! - A recorded directory whose generation count matches the previous
//!   snapshot is unchanged, so its whole subtree is copied from that snapshot
//!   without touching the disk.

use crate::kstat;
use crate::store::{Config, Row, Snapshot};
use crate::util::now;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Instant;

pub struct ScanOpts {
    /// Flag directories that qualify. When false, only report what would be flagged.
    pub flag: bool,
    pub verbose: bool,
}

#[derive(Default)]
pub struct ScanReport {
    pub snapshot: Snapshot,
    /// Directories asked of the kernel.
    pub queried: u64,
    /// Of those, how many had no record and were walked.
    pub walked: u64,
    /// Rows copied from the previous snapshot because nothing changed.
    pub reused: u64,
    pub flagged_new: Vec<String>,
    pub would_flag: Vec<String>,
    pub errors: Vec<String>,
    pub secs: f64,
}

impl ScanReport {
    pub fn summary(&self) -> String {
        format!(
            "{} dirs recorded: {} queried ({} walked), {} reused unchanged; {} newly flagged; {} errors; {:.1}s",
            self.snapshot.rows.len(),
            self.queried,
            self.walked,
            self.reused,
            self.flagged_new.len(),
            self.errors.len(),
            self.secs
        )
    }
}

struct Ctx<'a> {
    cfg: &'a Config,
    prev: Option<&'a Snapshot>,
    opts: &'a ScanOpts,
    rep: ScanReport,
}

pub fn run(cfg: &Config, prev: Option<&Snapshot>, opts: &ScanOpts) -> ScanReport {
    let start = Instant::now();
    let mut cx = Ctx { cfg, prev, opts, rep: ScanReport::default() };
    cx.rep.snapshot.ts = now();
    if let Some(root) = cfg.roots.first() {
        if let Ok((free, total)) = kstat::volume_space(root) {
            cx.rep.snapshot.free = free;
            cx.rep.snapshot.total = total;
        }
    }
    for root in &cfg.roots {
        match fs::metadata(root) {
            Ok(m) if m.is_dir() => {
                cx.visit(root, 0, m.dev());
            }
            Ok(_) => cx.rep.errors.push(format!("{}: not a directory", root.display())),
            Err(e) => cx.rep.errors.push(format!("{}: {e}", root.display())),
        }
    }
    cx.rep.secs = start.elapsed().as_secs_f64();
    cx.rep
}

impl Ctx<'_> {
    /// Returns how many entries at or below `path` are covered by a kernel
    /// record, so the caller can tell how much of itself is still unrecorded.
    fn visit(&mut self, path: &Path, depth: u32, dev: u64) -> u64 {
        let key = path.to_string_lossy().into_owned();
        let mut st = match kstat::query(path) {
            Ok(s) => s,
            Err(e) => {
                self.rep.errors.push(format!("{key}: {e}"));
                return 0;
            }
        };
        self.rep.queried += 1;
        if !st.has_record {
            self.rep.walked += 1;
        }

        if st.has_record && self.reuse_unchanged(&key, &st) {
            return st.entries + 1;
        }

        let mut covered = 0u64;
        // Children first. The kernel reuses a child's record when it walks or
        // flags the parent, so flagging bottom-up walks each file once instead
        // of once per level.
        if st.bytes >= self.cfg.record_min_bytes && depth < self.cfg.max_depth {
            for child in self.child_dirs(path, &key) {
                // Stay on the root's volume.
                if fs::symlink_metadata(&child).map(|m| m.dev() == dev).unwrap_or(false) {
                    covered += self.visit(&child, depth + 1, dev);
                }
            }
        }

        // Roots always qualify. Below them, a directory qualifies when the part
        // of it not already covered by a child's record is big enough that
        // walking it is slow. A chain of nested folders gets one flag, not one each.
        let uncovered = st.entries.saturating_sub(covered);
        if !st.has_record && (depth == 0 || uncovered >= self.cfg.flag_min_entries) {
            if self.opts.flag {
                if self.opts.verbose {
                    eprintln!("flagging {key} ({} entries)", st.entries);
                }
                match kstat::flag(path).and_then(|_| kstat::query(path)) {
                    Ok(s) => {
                        st = s;
                        self.rep.flagged_new.push(key.clone());
                    }
                    Err(e) => self.rep.errors.push(format!("flag {key}: {e}")),
                }
            } else {
                self.rep.would_flag.push(key.clone());
            }
        }
        let pending = !st.has_record && (depth == 0 || uncovered >= self.cfg.flag_min_entries);

        if depth == 0 || pending || st.bytes >= self.cfg.record_min_bytes {
            self.rep.snapshot.rows.insert(
                key,
                Row { bytes: st.bytes, entries: st.entries, gen: st.gen, flagged: st.has_record, pending },
            );
        }
        // A pending directory counts as covered so that a read-only scan
        // predicts the same set a flagging scan would produce.
        if st.has_record || pending {
            st.entries + 1
        } else {
            covered
        }
    }

    /// Immediate subdirectories, or none if there are too many to itemise.
    fn child_dirs(&mut self, path: &Path, key: &str) -> Vec<std::path::PathBuf> {
        let rd = match fs::read_dir(path) {
            Ok(rd) => rd,
            Err(e) => {
                self.rep.errors.push(format!("{key}: {e}"));
                return Vec::new();
            }
        };
        let mut children = Vec::new();
        for ent in rd.flatten() {
            // file_type() does not follow symlinks.
            if ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                children.push(ent.path());
                if children.len() > self.cfg.max_children {
                    return Vec::new();
                }
            }
        }
        children
    }

    /// If the kernel's change counter says nothing beneath `key` changed since
    /// the previous snapshot, copy that snapshot's rows and skip the subtree.
    fn reuse_unchanged(&mut self, key: &str, st: &kstat::DirStat) -> bool {
        let Some(prev) = self.prev else { return false };
        let Some(old) = prev.rows.get(key) else { return false };
        if !old.flagged || old.gen != st.gen || old.bytes != st.bytes {
            return false;
        }
        // When flagging, do not skip a subtree that still has work to do.
        if self.opts.flag && prev.subtree(key).any(|(_, r)| r.pending) {
            return false;
        }
        let mut n = 0;
        let min = self.cfg.record_min_bytes;
        // Drop rows that an older, finer-grained config recorded.
        for (p, r) in prev.subtree(key).filter(|(p, r)| p.as_str() == key || r.pending || r.bytes >= min) {
            if r.pending {
                self.rep.would_flag.push(p.clone());
            }
            self.rep.snapshot.rows.insert(p.clone(), r.clone());
            n += 1;
        }
        self.rep.reused += n;
        true
    }
}
