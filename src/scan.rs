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
use crate::report::REGENERABLE;
use crate::store::{Config, Row, Snapshot};
use crate::sys::System;
use crate::util::{base_name, normalize_path_str, parent_of};
use std::collections::{BTreeSet, HashMap, HashSet};
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
    sys: &'a dyn System,
    cfg: &'a Config,
    prev: Option<&'a Snapshot>,
    opts: &'a ScanOpts,
    rep: ScanReport,
}

pub fn run(sys: &dyn System, cfg: &Config, prev: Option<&Snapshot>, opts: &ScanOpts) -> ScanReport {
    let start = Instant::now();
    let mut cx = Ctx { sys, cfg, prev, opts, rep: ScanReport::default() };
    cx.rep.snapshot.ts = sys.now();
    cx.rep.snapshot.roots = cfg
        .roots
        .iter()
        .map(|r| normalize_path_str(&r.to_string_lossy()))
        .collect();

    let mut root_devs = HashSet::new();
    let mut got_vol = false;
    for root in &cfg.roots {
        match sys.dir_dev(root) {
            Ok(Some(dev)) => {
                root_devs.insert(dev);
                if !got_vol {
                    if let Ok((free, total)) = sys.volume_space(root) {
                        cx.rep.snapshot.free = free;
                        cx.rep.snapshot.total = total;
                        got_vol = true;
                    }
                }
                cx.visit(root, 0, dev);
            }
            Ok(None) => cx.rep.errors.push(format!("{}: not a directory", root.display())),
            Err(e) => cx.rep.errors.push(format!("{}: {e}", root.display())),
        }
    }
    if !got_vol {
        if let Some(root) = cfg.roots.first() {
            if let Ok((free, total)) = sys.volume_space(root) {
                cx.rep.snapshot.free = free;
                cx.rep.snapshot.total = total;
            }
        }
    }
    cx.rep.snapshot.single_volume = root_devs.len() <= 1;
    cx.rep.secs = start.elapsed().as_secs_f64();
    cx.rep
}

impl Ctx<'_> {
    /// Returns how many entries at or below `path` are covered by a kernel
    /// record, so the caller can tell how much of itself is still unrecorded.
    fn visit(&mut self, path: &Path, depth: u32, dev: u64) -> u64 {
        let key = normalize_path_str(&path.to_string_lossy());
        let mut st = match self.sys.query(path) {
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

        if st.has_record && self.reuse_unchanged(&key, &st, depth) {
            return st.entries + 1;
        }

        let mut covered = 0u64;
        let mut has_git = None;
        let is_regen = REGENERABLE.contains(&base_name(&key));
        // Children first. The kernel reuses a child's record when it walks or
        // flags the parent, so flagging bottom-up walks each file once instead
        // of once per level.
        if st.bytes >= self.cfg.record_min_bytes && depth < self.cfg.max_depth {
            match self.sys.list_dir(path, self.cfg.max_children) {
                Ok(listing) => {
                    has_git = Some(listing.has_git);
                    for child in listing.subdirs {
                        // Stay on the root's volume.
                        if self.sys.symlink_dev(&child).map(|d| d == dev).unwrap_or(false) {
                            let child_key = normalize_path_str(&child.to_string_lossy());
                            covered += self.visit(&child, depth + 1, dev);
                            if is_regen
                                && !self.rep.snapshot.rows.contains_key(&child_key)
                                && self.sys.has_git(&child)
                            {
                                self.rep.snapshot.extra_git_repos.insert(child_key);
                            }
                        }
                    }
                }
                Err(e) => {
                    self.rep.errors.push(format!("{key}: {e}"));
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
                match self.sys.flag(path).and_then(|_| self.sys.query(path)) {
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
            let has_git = has_git.unwrap_or_else(|| self.sys.has_git(path));
            self.rep.snapshot.rows.insert(
                key,
                Row {
                    bytes: st.bytes,
                    entries: st.entries,
                    gen: st.gen,
                    flagged: st.has_record,
                    pending,
                    has_git,
                },
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

    /// If the kernel's change counter says nothing beneath `key` changed since
    /// the previous snapshot, copy that snapshot's rows and skip the subtree.
    fn reuse_unchanged(&mut self, key: &str, st: &kstat::DirStat, depth: u32) -> bool {
        let Some(prev) = self.prev else { return false };
        let Some(old) = prev.rows.get(key) else { return false };
        if !old.flagged || old.gen != st.gen || old.bytes != st.bytes || old.entries != st.entries {
            return false;
        }
        // When flagging, do not skip a subtree that still has work to do.
        if self.opts.flag && prev.subtree(key).any(|(_, r)| r.pending) {
            return false;
        }
        let min = self.cfg.record_min_bytes;
        // A flagged non-root smaller than the recording threshold would not be
        // recorded by a full scan either, so skip descending and omit the row.
        if depth > 0 && st.bytes < min {
            return true;
        }
        let mut n = 0;
        let mut descendable: BTreeSet<&str> = BTreeSet::new();
        let mut depths: HashMap<&str, u32> = HashMap::new();
        // Rows iterate parents before children. Only retain a descendant if its
        // immediate parent is both kept and eligible to be descended into, so a
        // raised record_min_bytes or lowered max_depth can never leave orphan rows.
        for (p, r) in prev.subtree(key) {
            if p.as_str() == key {
                self.rep.snapshot.rows.insert(p.clone(), r.clone());
                depths.insert(p.as_str(), depth);
                if r.bytes >= min && depth < self.cfg.max_depth {
                    descendable.insert(p.as_str());
                }
                n += 1;
                continue;
            }
            let Some(par) = parent_of(p) else { continue };
            if !descendable.contains(par) {
                continue;
            }
            let p_depth = depths[par] + 1;
            if p_depth > self.cfg.max_depth || (!r.pending && r.bytes < min) {
                continue;
            }
            if r.pending {
                self.rep.would_flag.push(p.clone());
            }
            self.rep.snapshot.rows.insert(p.clone(), r.clone());
            depths.insert(p.as_str(), p_depth);
            if r.bytes >= min && p_depth < self.cfg.max_depth {
                descendable.insert(p.as_str());
            }
            n += 1;
        }
        for g in &prev.extra_git_repos {
            if parent_of(g).map_or(false, |par| descendable.contains(par))
                && !self.rep.snapshot.rows.contains_key(g)
            {
                self.rep.snapshot.extra_git_repos.insert(g.clone());
            }
        }
        self.rep.reused += n;
        true
    }
}
