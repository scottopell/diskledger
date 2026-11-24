//! Walk the monitored roots, record directory sizes, and (optionally) give
//! large directories their own kernel record so later reads are instant.
//!
//! Cost model:
//! - A directory with a kernel record answers in microseconds.
//! - A directory without one makes the kernel walk its subtree.
//! - A recorded directory whose generation count matches the previous
//!   snapshot is unchanged, so its whole subtree is copied from that snapshot
//!   without touching the disk.
//! - Once previously-flagged and large children account for all but
//!   `< record_min_bytes` and `< flag_min_entries` of a parent, remaining
//!   unflagged siblings are skipped without walking them.

use crate::kstat::{self, DirStat};
use crate::report::REGENERABLE;
use crate::store::{Config, Row, Snapshot};
use crate::sys::System;
use crate::util::{base_name, is_under, normalize_path_str, parent_of};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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

#[derive(Default)]
struct ScanOut {
    rows: std::collections::BTreeMap<String, Row>,
    extra_git_repos: BTreeSet<String>,
    flagged_new: Vec<String>,
    would_flag: Vec<String>,
    errors: Vec<String>,
}

struct SharedCtx<'a> {
    sys: &'a dyn System,
    cfg: &'a Config,
    prev: Option<&'a Snapshot>,
    opts: &'a ScanOpts,
    queried: AtomicU64,
    walked: AtomicU64,
    reused: AtomicU64,
    out: Mutex<ScanOut>,
}

struct Budget {
    covered: u64,
    visited_bytes: u64,
    visited_entries: u64,
}

impl Budget {
    fn can_prune(
        &self,
        st: &DirStat,
        depth: u32,
        parent_needs_covered: bool,
        cfg: &Config,
    ) -> bool {
        let rem_bytes = st.bytes.saturating_sub(self.visited_bytes);
        let rem_entries = st.entries.saturating_sub(self.visited_entries);
        if rem_bytes >= cfg.record_min_bytes || rem_entries >= cfg.flag_min_entries {
            return false;
        }
        let p_definitely_pending_or_flagged = st.has_record
            || depth == 0
            || st.entries.saturating_sub(self.covered.saturating_add(rem_entries))
                >= cfg.flag_min_entries;
        let p_definitely_not_pending = !st.has_record
            && depth > 0
            && st.entries.saturating_sub(self.covered) < cfg.flag_min_entries;
        p_definitely_pending_or_flagged
            || (!parent_needs_covered && p_definitely_not_pending)
            || rem_entries == 0
    }
}

struct ChildTask {
    path: PathBuf,
    key: String,
    prefetched: Option<DirStat>,
    /// True when bytes/entries/covered were already added to the parent budget in Pass 1.
    already_accounted: bool,
}

struct ChildBatch {
    tasks: Vec<ChildTask>,
    visited: Vec<AtomicBool>,
    next_idx: AtomicUsize,
    remaining_workers: AtomicUsize,
    pruned: AtomicBool,
    parent_st: DirStat,
    parent_depth: u32,
    dev: u64,
    parent_needs_covered: bool,
    child_needs_covered: bool,
    is_regen: bool,
    budget: Mutex<Budget>,
}

struct RootBatch<'a> {
    roots: Vec<(&'a Path, u64)>,
    next_idx: AtomicUsize,
    remaining_workers: AtomicUsize,
}

enum Job<'a> {
    Roots(Arc<RootBatch<'a>>),
    Children(Arc<ChildBatch>),
}

/// Bounded work-helping thread pool scoped to a single `scan::run` call.
/// Waiting parent threads pop and execute jobs from the shared queue, making
/// nested fan-out deadlock-free at any depth.
struct Pool<'a> {
    workers: usize,
    queue: Mutex<Vec<Job<'a>>>,
    done: AtomicBool,
    cv: Condvar,
}

impl<'a> Pool<'a> {
    fn new(threads: usize) -> Self {
        Pool {
            workers: threads.max(1),
            queue: Mutex::new(Vec::new()),
            done: AtomicBool::new(false),
            cv: Condvar::new(),
        }
    }

    fn push_many(&self, jobs: impl IntoIterator<Item = Job<'a>>) {
        let mut q = self.queue.lock().unwrap();
        let mut count = 0usize;
        for j in jobs {
            q.push(j);
            count += 1;
        }
        if count == 1 {
            self.cv.notify_one();
        } else if count > 1 {
            self.cv.notify_all();
        }
    }

    fn worker_loop(&self, cx: &SharedCtx<'a>) {
        loop {
            let job = {
                let mut q = self.queue.lock().unwrap();
                loop {
                    if let Some(j) = q.pop() {
                        break Some(j);
                    }
                    if self.done.load(Ordering::Acquire) {
                        break None;
                    }
                    q = self.cv.wait(q).unwrap();
                }
            };
            match job {
                Some(j) => self.run_job(cx, j),
                None => return,
            }
        }
    }

    fn wait_for(&self, cx: &SharedCtx<'a>, remaining: &AtomicUsize) {
        loop {
            if remaining.load(Ordering::Acquire) == 0 {
                return;
            }
            let job = {
                let mut q = self.queue.lock().unwrap();
                loop {
                    if remaining.load(Ordering::Acquire) == 0 {
                        break None;
                    }
                    if let Some(j) = q.pop() {
                        break Some(j);
                    }
                    q = self.cv.wait(q).unwrap();
                }
            };
            match job {
                Some(j) => self.run_job(cx, j),
                None => return,
            }
        }
    }

    fn finish_worker(&self, remaining: &AtomicUsize) {
        if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _g = self.queue.lock().unwrap();
            self.cv.notify_all();
        }
    }

    fn run_job(&self, cx: &SharedCtx<'a>, job: Job<'a>) {
        match job {
            Job::Roots(batch) => {
                loop {
                    let i = batch.next_idx.fetch_add(1, Ordering::Relaxed);
                    if i >= batch.roots.len() {
                        break;
                    }
                    let (root, dev) = batch.roots[i];
                    cx.visit(root, 0, dev, false, None, Some(self));
                }
                self.finish_worker(&batch.remaining_workers);
            }
            Job::Children(batch) => {
                loop {
                    let i = batch.next_idx.fetch_add(1, Ordering::Relaxed);
                    if i >= batch.tasks.len() {
                        break;
                    }
                    let task = &batch.tasks[i];
                    // Always visit already-accounted dirty Tier 0 children even if budget
                    // pruned the remaining unflagged siblings.
                    if !task.already_accounted && batch.pruned.load(Ordering::Acquire) {
                        continue;
                    }
                    if !task.already_accounted {
                        let b = batch.budget.lock().unwrap();
                        if b.can_prune(
                            &batch.parent_st,
                            batch.parent_depth,
                            batch.parent_needs_covered,
                            cx.cfg,
                        ) {
                            batch.pruned.store(true, Ordering::Release);
                            continue;
                        }
                    }
                    // For prefetched children, symlink_dev was already verified in Pass 1.
                    let same_dev = task.prefetched.is_some()
                        || cx.sys.symlink_dev(&task.path).map(|d| d == batch.dev).unwrap_or(false);
                    if !same_dev {
                        batch.visited[i].store(true, Ordering::Relaxed);
                        continue;
                    }
                    batch.visited[i].store(true, Ordering::Relaxed);
                    let res = cx.visit(
                        &task.path,
                        batch.parent_depth + 1,
                        batch.dev,
                        batch.child_needs_covered,
                        task.prefetched,
                        Some(self),
                    );
                    if !task.already_accounted {
                        let mut b = batch.budget.lock().unwrap();
                        b.covered += res.covered;
                        b.visited_bytes = b.visited_bytes.saturating_add(res.bytes);
                        b.visited_entries = b.visited_entries.saturating_add(res.entries_plus_self);
                        if b.can_prune(
                            &batch.parent_st,
                            batch.parent_depth,
                            batch.parent_needs_covered,
                            cx.cfg,
                        ) {
                            batch.pruned.store(true, Ordering::Release);
                        }
                    }
                    if batch.is_regen {
                        let has_row = cx.out.lock().unwrap().rows.contains_key(&task.key);
                        if !has_row && cx.sys.has_git(&task.path) {
                            cx.out.lock().unwrap().extra_git_repos.insert(task.key.clone());
                        }
                    }
                }
                self.finish_worker(&batch.remaining_workers);
            }
        }
    }
}

fn default_threads() -> usize {
    if let Ok(v) = std::env::var("DISKLEDGER_THREADS") {
        if let Ok(n) = v.parse::<usize>() {
            return n.max(1);
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 6))
        .unwrap_or(4)
}

pub fn run(sys: &dyn System, cfg: &Config, prev: Option<&Snapshot>, opts: &ScanOpts) -> ScanReport {
    run_with_threads(sys, cfg, prev, opts, default_threads())
}

pub fn run_with_threads(
    sys: &dyn System,
    cfg: &Config,
    prev: Option<&Snapshot>,
    opts: &ScanOpts,
    threads: usize,
) -> ScanReport {
    let start = Instant::now();
    let cx = SharedCtx {
        sys,
        cfg,
        prev,
        opts,
        queried: AtomicU64::new(0),
        walked: AtomicU64::new(0),
        reused: AtomicU64::new(0),
        out: Mutex::new(ScanOut::default()),
    };

    let ts = sys.now();
    let snap_roots: Vec<String> = cfg
        .roots
        .iter()
        .map(|r| normalize_path_str(&r.to_string_lossy()))
        .collect();

    let mut root_devs = HashSet::new();
    let mut valid_roots: Vec<(&Path, u64)> = Vec::new();
    let mut free = 0u64;
    let mut total = 0u64;
    let mut got_vol = false;

    for root in &cfg.roots {
        match sys.dir_dev(root) {
            Ok(Some(dev)) => {
                root_devs.insert(dev);
                if !got_vol {
                    if let Ok((f, t)) = sys.volume_space(root) {
                        free = f;
                        total = t;
                        got_vol = true;
                    }
                }
                valid_roots.push((root.as_path(), dev));
            }
            Ok(None) => cx.out.lock().unwrap().errors.push(format!("{}: not a directory", root.display())),
            Err(e) => cx.out.lock().unwrap().errors.push(format!("{}: {e}", root.display())),
        }
    }
    if !got_vol {
        if let Some(root) = cfg.roots.first() {
            if let Ok((f, t)) = sys.volume_space(root) {
                free = f;
                total = t;
            }
        }
    }

    let threads = threads.max(1);
    if threads == 1 {
        for (root, dev) in valid_roots {
            cx.visit(root, 0, dev, false, None, None);
        }
    } else if !valid_roots.is_empty() {
        let pool = Pool::new(threads);
        let n_tasks = valid_roots.len().min(threads);
        let root_batch = Arc::new(RootBatch {
            roots: valid_roots,
            next_idx: AtomicUsize::new(0),
            remaining_workers: AtomicUsize::new(n_tasks),
        });
        std::thread::scope(|s| {
            for _ in 0..(threads - 1) {
                s.spawn(|| pool.worker_loop(&cx));
            }
            pool.push_many((0..n_tasks).map(|_| Job::Roots(Arc::clone(&root_batch))));
            pool.wait_for(&cx, &root_batch.remaining_workers);
            pool.done.store(true, Ordering::Release);
            let _g = pool.queue.lock().unwrap();
            pool.cv.notify_all();
        });
    }

    let mut out = cx.out.into_inner().unwrap();
    out.flagged_new.sort();
    out.would_flag.sort();
    out.errors.sort();

    ScanReport {
        snapshot: Snapshot {
            ts,
            free,
            total,
            roots: snap_roots,
            single_volume: root_devs.len() <= 1,
            extra_git_repos: out.extra_git_repos,
            has_git_metadata: true,
            rows: out.rows,
        },
        queried: cx.queried.load(Ordering::Relaxed),
        walked: cx.walked.load(Ordering::Relaxed),
        reused: cx.reused.load(Ordering::Relaxed),
        flagged_new: out.flagged_new,
        would_flag: out.would_flag,
        errors: out.errors,
        secs: start.elapsed().as_secs_f64(),
    }
}

struct VisitRes {
    covered: u64,
    bytes: u64,
    entries_plus_self: u64,
}

impl<'a> SharedCtx<'a> {
    /// Visit `path` and return its covered entry count plus exact APFS totals
    /// so the parent can tighten its remaining byte/entry budget.
    fn visit(
        &self,
        path: &Path,
        depth: u32,
        dev: u64,
        parent_needs_covered: bool,
        prefetched: Option<DirStat>,
        pool: Option<&Pool<'a>>,
    ) -> VisitRes {
        let key = normalize_path_str(&path.to_string_lossy());

        // Bottom-up fast path for previously-unflagged large container directories
        // that have no direct regular files and a small set of subdirectories:
        // summing the children directly avoids a redundant full-subtree kernel walk
        // on the parent before walking those same children.
        let mut prefetched_listing: Option<crate::sys::DirListing> = None;
        if prefetched.is_none() && !self.opts.flag && depth > 0 && depth < self.cfg.max_depth {
            if let Some(old) = self.prev.and_then(|p| p.rows.get(&key)) {
                if !old.flagged && old.bytes >= self.cfg.record_min_bytes {
                    if let Ok(listing) = self.sys.list_dir(path, self.cfg.max_children) {
                        let unknown_kids = listing
                            .subdirs
                            .iter()
                            .filter(|c| {
                                let ck = normalize_path_str(&c.to_string_lossy());
                                !self.prev.map_or(false, |p| p.rows.contains_key(&ck))
                            })
                            .count();
                        if !listing.too_many_children
                            && !listing.has_direct_files
                            && (unknown_kids == 0 || listing.subdirs.len() <= 16)
                        {
                            return self.visit_container_bottom_up(
                                &key,
                                depth,
                                dev,
                                listing,
                                pool,
                            );
                        }
                        prefetched_listing = Some(listing);
                    }
                }
            }
        }

        let mut st = match prefetched {
            Some(s) => s,
            None => match self.sys.query(path) {
                Ok(s) => {
                    self.queried.fetch_add(1, Ordering::Relaxed);
                    if !s.has_record {
                        self.walked.fetch_add(1, Ordering::Relaxed);
                    }
                    s
                }
                Err(e) => {
                    self.out.lock().unwrap().errors.push(format!("{key}: {e}"));
                    return VisitRes { covered: 0, bytes: 0, entries_plus_self: 0 };
                }
            },
        };

        if prefetched.is_none() && st.has_record && self.reuse_unchanged(&key, &st, depth) {
            return VisitRes {
                covered: st.entries + 1,
                bytes: st.bytes,
                entries_plus_self: st.entries + 1,
            };
        }

        if st.has_record && self.try_reuse_via_frontier(&key, &st, depth, dev, pool) {
            return VisitRes {
                covered: st.entries + 1,
                bytes: st.bytes,
                entries_plus_self: st.entries + 1,
            };
        }

        let mut covered = 0u64;
        let mut has_git = None;
        let is_regen = REGENERABLE.contains(&base_name(&key));
        // Children first. The kernel reuses a child's record when it walks or
        // flags the parent, so flagging bottom-up walks each file once instead
        // of once per level.
        if st.bytes >= self.cfg.record_min_bytes && depth < self.cfg.max_depth {
            let listing_res = match prefetched_listing {
                Some(l) => Ok(l),
                None => self.sys.list_dir(path, self.cfg.max_children),
            };
            match listing_res {
                Ok(listing) => {
                    has_git = Some(listing.has_git);
                    // Order children: Tier 0 = previously flagged, Tier 1 = previously recorded,
                    // Tier 2 = unrecorded. Within each tier, largest first for faster pruning.
                    let mut children: Vec<(PathBuf, String, u8, u64)> = listing
                        .subdirs
                        .into_iter()
                        .map(|c| {
                            let ck = normalize_path_str(&c.to_string_lossy());
                            let (tier, prev_bytes) = match self.prev.and_then(|p| p.rows.get(&ck)) {
                                Some(r) if r.flagged => (0u8, r.bytes),
                                Some(r) => (1u8, r.bytes),
                                None => (2u8, 0),
                            };
                            (c, ck, tier, prev_bytes)
                        })
                        .collect();
                    children.sort_by(|a, b| {
                        a.2.cmp(&b.2)
                            .then_with(|| b.3.cmp(&a.3))
                            .then_with(|| a.1.cmp(&b.1))
                    });

                    let mut budget = Budget { covered: 0, visited_bytes: 0, visited_entries: 0 };
                    let child_needs_covered = !st.has_record && depth > 0;

                    // Phase 1: probe Tier 0 (previously-flagged) children in microseconds.
                    // Unchanged ones reuse immediately; dirty ones still report their exact
                    // APFS totals right away, tightening the budget before any unflagged walk.
                    let mut tier01_tasks: Vec<ChildTask> = Vec::new();
                    let mut idx = 0usize;
                    while idx < children.len() && children[idx].2 == 0 {
                        let (ref child, ref child_key, _, _) = children[idx];
                        idx += 1;
                        if !self.sys.symlink_dev(child).map(|d| d == dev).unwrap_or(false) {
                            continue;
                        }
                        match self.sys.query(child) {
                            Ok(cst) => {
                                self.queried.fetch_add(1, Ordering::Relaxed);
                                if !cst.has_record {
                                    self.walked.fetch_add(1, Ordering::Relaxed);
                                }
                                if cst.has_record {
                                    budget.covered += cst.entries + 1;
                                    budget.visited_bytes = budget.visited_bytes.saturating_add(cst.bytes);
                                    budget.visited_entries =
                                        budget.visited_entries.saturating_add(cst.entries + 1);
                                    if self.reuse_unchanged(child_key, &cst, depth + 1) {
                                        if is_regen {
                                            let has_row = self.out.lock().unwrap().rows.contains_key(child_key);
                                            if !has_row && self.sys.has_git(child) {
                                                self.out.lock().unwrap().extra_git_repos.insert(child_key.clone());
                                            }
                                        }
                                        continue;
                                    }
                                    tier01_tasks.push(ChildTask {
                                        path: child.clone(),
                                        key: child_key.clone(),
                                        prefetched: Some(cst),
                                        already_accounted: true,
                                    });
                                } else {
                                    // Previously flagged dir was recreated unflagged; visit in Phase 2a.
                                    tier01_tasks.push(ChildTask {
                                        path: child.clone(),
                                        key: child_key.clone(),
                                        prefetched: Some(cst),
                                        already_accounted: false,
                                    });
                                }
                            }
                            Err(e) => {
                                self.out.lock().unwrap().errors.push(format!("{child_key}: {e}"));
                            }
                        }
                    }

                    let mut tier2_tasks: Vec<ChildTask> = Vec::new();
                    let pruned_after_tier0 =
                        budget.can_prune(&st, depth, parent_needs_covered, self.cfg);

                    if pruned_after_tier0 {
                        // All remaining Tier 1/2 children are pruned; still check direct `.git` if regen.
                        if is_regen {
                            for (child, child_key, _, _) in &children[idx..] {
                                if self.sys.symlink_dev(child).map(|d| d == dev).unwrap_or(false)
                                    && self.sys.has_git(child)
                                {
                                    self.out.lock().unwrap().extra_git_repos.insert(child_key.clone());
                                }
                            }
                        }
                    } else {
                        for (child, child_key, tier, _) in children.into_iter().skip(idx) {
                            let task = ChildTask {
                                path: child,
                                key: child_key,
                                prefetched: None,
                                already_accounted: false,
                            };
                            if tier == 1 {
                                tier01_tasks.push(task);
                            } else {
                                tier2_tasks.push(task);
                            }
                        }
                    }

                    // Phase 2a: visit dirty Tier 0 + Tier 1 (previously large) before Tier 2 so
                    // worker threads do not race into unrecorded small siblings before large ones
                    // tighten the remaining byte/entry budget.
                    if !tier01_tasks.is_empty() {
                        budget = self.run_child_tasks(
                            tier01_tasks,
                            budget,
                            st,
                            depth,
                            dev,
                            parent_needs_covered,
                            child_needs_covered,
                            is_regen,
                            pool,
                        );
                    }

                    // Phase 2b: visit Tier 2 (previously small/unrecorded) only if budget remains.
                    if !tier2_tasks.is_empty() {
                        if budget.can_prune(&st, depth, parent_needs_covered, self.cfg) {
                            if is_regen {
                                for task in &tier2_tasks {
                                    if self.sys.symlink_dev(&task.path).map(|d| d == dev).unwrap_or(false)
                                        && self.sys.has_git(&task.path)
                                    {
                                        self.out.lock().unwrap().extra_git_repos.insert(task.key.clone());
                                    }
                                }
                            }
                        } else {
                            budget = self.run_child_tasks(
                                tier2_tasks,
                                budget,
                                st,
                                depth,
                                dev,
                                parent_needs_covered,
                                child_needs_covered,
                                is_regen,
                                pool,
                            );
                        }
                    }
                    covered = budget.covered;
                }
                Err(e) => {
                    self.out.lock().unwrap().errors.push(format!("{key}: {e}"));
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
                        self.out.lock().unwrap().flagged_new.push(key.clone());
                    }
                    Err(e) => self.out.lock().unwrap().errors.push(format!("flag {key}: {e}")),
                }
            } else {
                self.out.lock().unwrap().would_flag.push(key.clone());
            }
        }
        let pending = !st.has_record && (depth == 0 || uncovered >= self.cfg.flag_min_entries);

        if depth == 0 || pending || st.bytes >= self.cfg.record_min_bytes {
            let has_git = has_git.unwrap_or_else(|| self.sys.has_git(path));
            self.out.lock().unwrap().rows.insert(
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
        let out_covered = if st.has_record || pending {
            st.entries + 1
        } else {
            covered
        };
        VisitRes {
            covered: out_covered,
            bytes: st.bytes,
            entries_plus_self: st.entries + 1,
        }
    }

    /// Execute a batch of child tasks either in parallel via `pool` or inline.
    fn run_child_tasks(
        &self,
        tasks: Vec<ChildTask>,
        mut budget: Budget,
        st: DirStat,
        depth: u32,
        dev: u64,
        parent_needs_covered: bool,
        child_needs_covered: bool,
        is_regen: bool,
        pool: Option<&Pool<'a>>,
    ) -> Budget {
        if let Some(pool) = pool.filter(|_| tasks.len() >= 2) {
            let n_tasks = tasks.len().min(pool.workers);
            let visited: Vec<AtomicBool> =
                (0..tasks.len()).map(|_| AtomicBool::new(false)).collect();
            let batch = Arc::new(ChildBatch {
                tasks,
                visited,
                next_idx: AtomicUsize::new(0),
                remaining_workers: AtomicUsize::new(n_tasks),
                pruned: AtomicBool::new(false),
                parent_st: st,
                parent_depth: depth,
                dev,
                parent_needs_covered,
                child_needs_covered,
                is_regen,
                budget: Mutex::new(budget),
            });
            pool.push_many((0..n_tasks).map(|_| Job::Children(Arc::clone(&batch))));
            pool.wait_for(self, &batch.remaining_workers);

            if is_regen {
                for (i, task) in batch.tasks.iter().enumerate() {
                    if !batch.visited[i].load(Ordering::Relaxed)
                        && self.sys.symlink_dev(&task.path).map(|d| d == dev).unwrap_or(false)
                    {
                        let has_row = self.out.lock().unwrap().rows.contains_key(&task.key);
                        if !has_row && self.sys.has_git(&task.path) {
                            self.out.lock().unwrap().extra_git_repos.insert(task.key.clone());
                        }
                    }
                }
            }
            let b = batch.budget.lock().unwrap();
            Budget {
                covered: b.covered,
                visited_bytes: b.visited_bytes,
                visited_entries: b.visited_entries,
            }
        } else {
            let mut pruned_from = None;
            for (idx, task) in tasks.iter().enumerate() {
                if !task.already_accounted
                    && budget.can_prune(&st, depth, parent_needs_covered, self.cfg)
                {
                    pruned_from = Some(idx);
                    break;
                }
                let same_dev = task.prefetched.is_some()
                    || self.sys.symlink_dev(&task.path).map(|d| d == dev).unwrap_or(false);
                if !same_dev {
                    continue;
                }
                let res = self.visit(
                    &task.path,
                    depth + 1,
                    dev,
                    child_needs_covered,
                    task.prefetched,
                    pool,
                );
                if !task.already_accounted {
                    budget.covered += res.covered;
                    budget.visited_bytes = budget.visited_bytes.saturating_add(res.bytes);
                    budget.visited_entries =
                        budget.visited_entries.saturating_add(res.entries_plus_self);
                }
                if is_regen {
                    let has_row = self.out.lock().unwrap().rows.contains_key(&task.key);
                    if !has_row && self.sys.has_git(&task.path) {
                        self.out.lock().unwrap().extra_git_repos.insert(task.key.clone());
                    }
                }
            }
            if is_regen {
                if let Some(start) = pruned_from {
                    for task in &tasks[start..] {
                        if self.sys.symlink_dev(&task.path).map(|d| d == dev).unwrap_or(false) {
                            let has_row = self.out.lock().unwrap().rows.contains_key(&task.key);
                            if !has_row && self.sys.has_git(&task.path) {
                                self.out.lock().unwrap().extra_git_repos.insert(task.key.clone());
                            }
                        }
                    }
                }
            }
            budget
        }
    }

    /// Compute an unflagged, zero-direct-file container directory's exact APFS
    /// totals bottom-up from its same-volume subdirectories without issuing a
    /// redundant kernel walk on the parent first.
    fn visit_container_bottom_up(
        &self,
        key: &str,
        depth: u32,
        dev: u64,
        listing: crate::sys::DirListing,
        pool: Option<&Pool<'a>>,
    ) -> VisitRes {
        let is_regen = REGENERABLE.contains(&base_name(key));
        let mut tasks: Vec<ChildTask> = Vec::with_capacity(listing.subdirs.len());
        for child in listing.subdirs {
            if !self.sys.symlink_dev(&child).map(|d| d == dev).unwrap_or(false) {
                continue;
            }
            let ck = normalize_path_str(&child.to_string_lossy());
            tasks.push(ChildTask {
                path: child,
                key: ck,
                prefetched: None,
                already_accounted: false,
            });
        }

        // No budget pruning here because parent totals are computed from children.
        // Pass dummy max DirStat so `can_prune` never fires.
        let sentinel = DirStat { has_record: false, gen: 0, entries: u64::MAX, bytes: u64::MAX };
        let budget = if tasks.is_empty() {
            Budget { covered: 0, visited_bytes: 0, visited_entries: 0 }
        } else {
            self.run_child_tasks(
                tasks,
                Budget { covered: 0, visited_bytes: 0, visited_entries: 0 },
                sentinel,
                depth,
                dev,
                true,
                true,
                is_regen,
                pool,
            )
        };

        let st = DirStat {
            has_record: false,
            gen: 0,
            entries: budget.visited_entries,
            bytes: budget.visited_bytes,
        };
        let uncovered = st.entries.saturating_sub(budget.covered);
        let pending = uncovered >= self.cfg.flag_min_entries;

        if st.bytes < self.cfg.record_min_bytes {
            // Directory shrank below record_min_bytes since `prev`: a full scan would
            // not have descended into it, so drop any descendant rows/would_flag recorded.
            let mut out = self.out.lock().unwrap();
            let prefix = format!("{key}/");
            out.rows.retain(|k, _| !k.starts_with(&prefix));
            out.would_flag.retain(|k| !k.starts_with(&prefix));
            out.extra_git_repos.retain(|k| !k.starts_with(&prefix));
            if pending {
                out.would_flag.push(key.to_string());
                out.rows.insert(
                    key.to_string(),
                    Row {
                        bytes: st.bytes,
                        entries: st.entries,
                        gen: 0,
                        flagged: false,
                        pending: true,
                        has_git: listing.has_git,
                    },
                );
            }
        } else {
            let mut out = self.out.lock().unwrap();
            if pending {
                out.would_flag.push(key.to_string());
            }
            out.rows.insert(
                key.to_string(),
                Row {
                    bytes: st.bytes,
                    entries: st.entries,
                    gen: 0,
                    flagged: false,
                    pending,
                    has_git: listing.has_git,
                },
            );
        }

        let out_covered = if pending { st.entries + 1 } else { budget.covered };
        VisitRes {
            covered: out_covered,
            bytes: st.bytes,
            entries_plus_self: st.entries + 1,
        }
    }

    /// If the kernel's change counter says nothing beneath `key` changed since
    /// the previous snapshot, copy that snapshot's rows and skip the subtree.
    fn reuse_unchanged(&self, key: &str, st: &kstat::DirStat, depth: u32) -> bool {
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
        let mut local_rows: Vec<(String, Row)> = Vec::new();
        let mut local_would: Vec<String> = Vec::new();
        // Rows iterate parents before children. Only retain a descendant if its
        // immediate parent is both kept and eligible to be descended into, so a
        // raised record_min_bytes or lowered max_depth can never leave orphan rows.
        for (p, r) in prev.subtree(key) {
            if p.as_str() == key {
                local_rows.push((p.clone(), r.clone()));
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
                local_would.push(p.clone());
            }
            local_rows.push((p.clone(), r.clone()));
            depths.insert(p.as_str(), p_depth);
            if r.bytes >= min && p_depth < self.cfg.max_depth {
                descendable.insert(p.as_str());
            }
            n += 1;
        }
        let mut out = self.out.lock().unwrap();
        for (p, r) in local_rows {
            out.rows.insert(p, r);
        }
        out.would_flag.extend(local_would);
        for g in &prev.extra_git_repos {
            if parent_of(g).map_or(false, |par| descendable.contains(par))
                && !out.rows.contains_key(g)
            {
                out.extra_git_repos.insert(g.clone());
            }
        }
        drop(out);
        self.reused.fetch_add(n, Ordering::Relaxed);
        true
    }

    /// APFS propagates every mutation's generation-count increment identically up
    /// through all flagged ancestors. When a dirty flagged directory's `(dgen,
    /// dbytes, dentries)` is 100% accounted for by its maximal flagged descendants
    /// from `prev`, nothing outside that flagged frontier changed: reuse all
    /// unflagged rows in between (adjusting intermediate ancestor totals) and only
    /// recurse into the dirty frontier subtrees.
    fn try_reuse_via_frontier(
        &self,
        key: &str,
        st: &DirStat,
        depth: u32,
        dev: u64,
        pool: Option<&Pool<'a>>,
    ) -> bool {
        let Some(prev) = self.prev else { return false };
        let Some(old) = prev.rows.get(key) else { return false };
        if !old.flagged || old.gen == 0 || st.gen < old.gen {
            return false;
        }
        let min = self.cfg.record_min_bytes;
        if st.bytes < min || depth >= self.cfg.max_depth {
            return false;
        }

        // Collect maximal flagged strict descendants of `key` from `prev`.
        let mut frontier: Vec<(&str, &Row, u32)> = Vec::new();
        for (p, r) in prev.subtree(key) {
            if p.as_str() == key {
                continue;
            }
            if frontier.iter().any(|(f, _, _)| is_under(p, f)) {
                continue;
            }
            if r.flagged {
                let rel = if key == "/" {
                    p.matches('/').count() as u32
                } else {
                    p[key.len()..].matches('/').count() as u32
                };
                let f_depth = depth + rel;
                if f_depth > self.cfg.max_depth {
                    return false;
                }
                frontier.push((p.as_str(), r, f_depth));
            }
        }
        if frontier.is_empty() {
            return false;
        }

        // Probe every frontier directory (each is flagged -> microseconds).
        let mut sum_dgen: u64 = 0;
        let mut sum_dbytes: i128 = 0;
        let mut sum_dentries: i128 = 0;
        let mut dirty: Vec<(&str, DirStat, i128, i128, u32)> = Vec::new();
        let mut clean: Vec<(&str, DirStat, u32)> = Vec::new();

        for &(fp, old_f, f_depth) in &frontier {
            let p = Path::new(fp);
            if !self.sys.symlink_dev(p).map(|d| d == dev).unwrap_or(false) {
                return false;
            }
            let Ok(cst) = self.sys.query(p) else { return false };
            self.queried.fetch_add(1, Ordering::Relaxed);
            if !cst.has_record {
                self.walked.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            if cst.gen < old_f.gen {
                return false;
            }
            let dg = cst.gen - old_f.gen;
            let db = cst.bytes as i128 - old_f.bytes as i128;
            let de = cst.entries as i128 - old_f.entries as i128;
            sum_dgen = match sum_dgen.checked_add(dg) {
                Some(v) => v,
                None => return false,
            };
            sum_dbytes += db;
            sum_dentries += de;
            if dg != 0 || db != 0 || de != 0 {
                if cst.bytes < min {
                    return false;
                }
                dirty.push((fp, cst, db, de, f_depth));
            } else {
                clean.push((fp, cst, f_depth));
            }
        }

        // Exact APFS conservation check: all mutations happened strictly inside `frontier`.
        let want_dgen = st.gen - old.gen;
        let want_dbytes = st.bytes as i128 - old.bytes as i128;
        let want_dentries = st.entries as i128 - old.entries as i128;
        if sum_dgen != want_dgen || sum_dbytes != want_dbytes || sum_dentries != want_dentries {
            return false;
        }

        // Verify all non-frontier rows in `prev.subtree(key)` stay valid and tree-closed.
        let mut descendable: BTreeSet<&str> = BTreeSet::new();
        let mut depths: HashMap<&str, u32> = HashMap::new();
        let mut local_rows: Vec<(String, Row)> = Vec::new();
        let mut local_would: Vec<String> = Vec::new();

        for (p, r) in prev.subtree(key) {
            if p.as_str() == key {
                local_rows.push((
                    p.clone(),
                    Row {
                        bytes: st.bytes,
                        entries: st.entries,
                        gen: st.gen,
                        flagged: true,
                        pending: false,
                        has_git: r.has_git,
                    },
                ));
                depths.insert(p.as_str(), depth);
                descendable.insert(p.as_str());
                continue;
            }
            if frontier.iter().any(|(f, _, _)| p.as_str() == *f || is_under(p, f)) {
                continue;
            }
            if self.opts.flag && r.pending {
                return false;
            }
            let Some(par) = parent_of(p) else { continue };
            if !descendable.contains(par) {
                continue;
            }
            let p_depth = depths[par] + 1;
            if p_depth > self.cfg.max_depth {
                continue;
            }
            let mut db = 0i128;
            let mut de = 0i128;
            let mut has_dirty_desc = false;
            for &(fp, _, fdb, fde, _) in &dirty {
                if is_under(fp, p) {
                    db += fdb;
                    de += fde;
                    has_dirty_desc = true;
                }
            }
            let Ok(new_bytes) = u64::try_from(r.bytes as i128 + db) else { return false };
            let Ok(new_entries) = u64::try_from(r.entries as i128 + de) else { return false };
            if has_dirty_desc && new_bytes < min {
                return false;
            }
            if !r.pending && new_bytes < min {
                continue;
            }
            if r.pending {
                local_would.push(p.clone());
            }
            local_rows.push((
                p.clone(),
                Row {
                    bytes: new_bytes,
                    entries: new_entries,
                    gen: r.gen,
                    flagged: r.flagged,
                    pending: r.pending,
                    has_git: r.has_git,
                },
            ));
            depths.insert(p.as_str(), p_depth);
            if new_bytes >= min && p_depth < self.cfg.max_depth {
                descendable.insert(p.as_str());
            }
        }

        // Every frontier item's parent must be descendable (in case max_depth shrank).
        for &(fp, _, _) in &frontier {
            if let Some(par) = parent_of(fp) {
                if !descendable.contains(par) {
                    return false;
                }
            }
        }

        // Commit reused non-frontier rows and preserved `extra_git_repos`.
        let reused_n = local_rows.len() as u64;
        {
            let mut out = self.out.lock().unwrap();
            for (p, r) in local_rows {
                out.rows.insert(p, r);
            }
            out.would_flag.extend(local_would);
            for g in &prev.extra_git_repos {
                if parent_of(g).map_or(false, |par| descendable.contains(par))
                    && !frontier.iter().any(|(f, _, _)| g.as_str() == *f || is_under(g, f))
                    && !out.rows.contains_key(g)
                {
                    out.extra_git_repos.insert(g.clone());
                }
            }
        }
        self.reused.fetch_add(reused_n, Ordering::Relaxed);

        // Reuse clean frontier subtrees (or move them to `dirty` if flagging & pending inside).
        let mut to_visit: Vec<(PathBuf, DirStat, u32)> = Vec::new();
        for (fp, cst, f_depth) in clean {
            if !self.reuse_unchanged(fp, &cst, f_depth) {
                to_visit.push((PathBuf::from(fp), cst, f_depth));
            }
        }
        for (fp, cst, _, _, f_depth) in dirty {
            to_visit.push((PathBuf::from(fp), cst, f_depth));
        }

        // Visit dirty frontier subtrees (each fans out internally via `pool`).
        for (fp, cst, f_depth) in to_visit {
            self.visit(&fp, f_depth, dev, false, Some(cst), pool);
        }

        true
    }
}
