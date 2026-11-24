//! Pure report computation plus human and JSON rendering for diff, top, and candidates.

use crate::store::{Snapshot, Store};
use crate::sys::System;
use crate::util::{
    base_name, fmt_age, fmt_ts, human, human_delta, is_same_or_under, is_under, json_str,
    normalize_path_str, parent_of, Res,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Directory names that usually hold regenerable output.
pub const REGENERABLE: &[&str] = &[
    "target", "node_modules", "incremental", "DerivedData", ".build", "build", "dist", ".next",
    "__pycache__", ".venv", "venv", ".gradle", "Pods", ".turbo", ".cache",
];

fn bytes_of(s: &Snapshot, p: &str) -> u64 {
    s.rows.get(p).map(|r| r.bytes).unwrap_or(0)
}

/// Map each path to its children, and list the paths with no recorded parent.
fn tree<'a>(paths: &BTreeSet<&'a str>) -> (BTreeMap<&'a str, Vec<&'a str>>, Vec<&'a str>) {
    let mut kids: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut tops = Vec::new();
    for &p in paths {
        match parent_of(p).filter(|par| paths.contains(par)) {
            Some(par) => kids.entry(par).or_default().push(p),
            None => tops.push(p),
        }
    }
    (kids, tops)
}

/// Configured roots of a snapshot, falling back to top-level paths for legacy v0.1.2 snapshots.
fn snapshot_roots(s: &Snapshot) -> BTreeSet<String> {
    if !s.roots.is_empty() {
        return s.roots.iter().cloned().collect();
    }
    let paths: BTreeSet<&str> = s.rows.keys().map(|k| k.as_str()).collect();
    let (_, tops) = tree(&paths);
    tops.into_iter().map(str::to_string).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    Changed,
    New,
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub path: String,
    pub before: u64,
    pub after: u64,
    pub delta: i64,
    pub status: DiffStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReport {
    pub since: u64,
    pub now: u64,
    pub free_before: u64,
    pub free_after: u64,
    pub free_delta: i64,
    pub monitored_delta: i64,
    /// None when roots span multiple volumes so a single free-space delta does not apply.
    pub outside_delta: Option<i64>,
    /// Change in sibling APFS container volumes (e.g. VM swapfile, System, Preboot).
    pub other_volumes_delta: Option<i64>,
    /// Change in unmonitored space strictly on this volume (`outside_delta - other_volumes_delta`).
    pub unmonitored_volume_delta: Option<i64>,
    pub purgeable_before: Option<u64>,
    pub purgeable_after: Option<u64>,
    pub purgeable_delta: Option<i64>,
    pub apfs_snapshots_before: Option<u64>,
    pub apfs_snapshots_after: Option<u64>,
    pub min_bytes: u64,
    pub not_in_baseline: Vec<(String, u64)>,
    pub no_longer_monitored: Vec<(String, u64)>,
    /// All covered rows (used to build the display tree).
    pub rows: BTreeMap<String, DiffRow>,
    /// Covered rows with `|delta| >= min_bytes`, sorted by `|delta|` descending.
    pub changes: Vec<DiffRow>,
    /// Covered top-level roots present in `rows`.
    pub tops: Vec<String>,
}

pub fn compute_diff(base: &Snapshot, cur: &Snapshot, min: u64) -> DiffReport {
    let base_roots = snapshot_roots(base);
    let cur_roots = snapshot_roots(cur);

    let not_in_baseline: Vec<(String, u64)> = cur_roots
        .iter()
        .filter(|r| !base_roots.contains(*r))
        .map(|r| (r.clone(), bytes_of(cur, r)))
        .collect();
    let no_longer_monitored: Vec<(String, u64)> = base_roots
        .iter()
        .filter(|r| !cur_roots.contains(*r))
        .map(|r| (r.clone(), bytes_of(base, r)))
        .collect();
    let covered_roots: BTreeSet<&str> = base_roots
        .intersection(&cur_roots)
        .map(|s| s.as_str())
        .collect();

    let is_covered = |p: &str| covered_roots.iter().any(|r| is_same_or_under(p, r));

    let all_paths: BTreeSet<&str> = base
        .rows
        .keys()
        .chain(cur.rows.keys())
        .map(|s| s.as_str())
        .filter(|p| is_covered(p))
        .collect();

    let mut rows = BTreeMap::new();
    for &p in &all_paths {
        let before = bytes_of(base, p);
        let after = bytes_of(cur, p);
        let delta = after as i64 - before as i64;
        let status = if !base.rows.contains_key(p) {
            DiffStatus::New
        } else if !cur.rows.contains_key(p) {
            DiffStatus::Gone
        } else {
            DiffStatus::Changed
        };
        rows.insert(p.to_string(), DiffRow { path: p.to_string(), before, after, delta, status });
    }

    let (_, raw_tops) = tree(&all_paths);
    let tops: Vec<String> = raw_tops.into_iter().map(str::to_string).collect();

    let monitored_delta: i64 = covered_roots
        .iter()
        .map(|r| bytes_of(cur, r) as i64 - bytes_of(base, r) as i64)
        .sum();
    let free_delta = cur.free as i64 - base.free as i64;
    let single_vol = base.single_volume && cur.single_volume;
    let outside_delta = if single_vol {
        Some(-free_delta - monitored_delta)
    } else {
        None
    };
    let other_volumes_delta = if single_vol {
        match (base.other_volumes_used(), cur.other_volumes_used()) {
            (Some(b), Some(c)) => Some(c as i64 - b as i64),
            _ => None,
        }
    } else {
        None
    };
    let unmonitored_volume_delta = match (outside_delta, other_volumes_delta) {
        (Some(out), Some(other)) => Some(out - other),
        _ => None,
    };
    let purgeable_before = if single_vol { base.purgeable } else { None };
    let purgeable_after = if single_vol { cur.purgeable } else { None };
    let purgeable_delta = match (purgeable_before, purgeable_after) {
        (Some(b), Some(c)) => Some(c as i64 - b as i64),
        _ => None,
    };
    let apfs_snapshots_before = if single_vol { base.apfs_snapshots } else { None };
    let apfs_snapshots_after = if single_vol { cur.apfs_snapshots } else { None };

    let mut changes: Vec<DiffRow> = rows
        .values()
        .filter(|r| r.delta.unsigned_abs() >= min)
        .cloned()
        .collect();
    changes.sort_by(|a, b| {
        b.delta
            .unsigned_abs()
            .cmp(&a.delta.unsigned_abs())
            .then_with(|| a.path.cmp(&b.path))
    });

    DiffReport {
        since: base.ts,
        now: cur.ts,
        free_before: base.free,
        free_after: cur.free,
        free_delta,
        monitored_delta,
        outside_delta,
        other_volumes_delta,
        unmonitored_volume_delta,
        purgeable_before,
        purgeable_after,
        purgeable_delta,
        apfs_snapshots_before,
        apfs_snapshots_after,
        min_bytes: min,
        not_in_baseline,
        no_longer_monitored,
        rows,
        changes,
        tops,
    }
}

impl DiffReport {
    pub fn render_json(&self, w: &mut impl Write) -> io::Result<()> {
        let skipped: Vec<String> = self.not_in_baseline.iter().map(|(u, _)| json_str(u)).collect();
        let removed: Vec<String> = self.no_longer_monitored.iter().map(|(u, _)| json_str(u)).collect();
        let items: Vec<String> = self
            .changes
            .iter()
            .map(|r| {
                format!(
                    "{{\"path\":{},\"before\":{},\"after\":{},\"delta\":{}}}",
                    json_str(&r.path),
                    r.before,
                    r.after,
                    r.delta
                )
            })
            .collect();
        let opt_i64 = |v: Option<i64>| v.map_or_else(|| "null".to_string(), |x| x.to_string());
        let opt_u64 = |v: Option<u64>| v.map_or_else(|| "null".to_string(), |x| x.to_string());
        writeln!(
            w,
            "{{\"since\":{},\"now\":{},\"free_before\":{},\"free_after\":{},\"free_delta\":{},\"monitored_delta\":{},\"outside_delta\":{},\"unmonitored_volume_delta\":{},\"other_volumes_delta\":{},\"purgeable_before\":{},\"purgeable_after\":{},\"purgeable_delta\":{},\"apfs_snapshots_before\":{},\"apfs_snapshots_after\":{},\"min_bytes\":{},\"not_in_baseline\":[{}],\"no_longer_monitored\":[{}],\"changes\":[{}]}}",
            self.since,
            self.now,
            self.free_before,
            self.free_after,
            self.free_delta,
            self.monitored_delta,
            opt_i64(self.outside_delta),
            opt_i64(self.unmonitored_volume_delta),
            opt_i64(self.other_volumes_delta),
            opt_u64(self.purgeable_before),
            opt_u64(self.purgeable_after),
            opt_i64(self.purgeable_delta),
            opt_u64(self.apfs_snapshots_before),
            opt_u64(self.apfs_snapshots_after),
            self.min_bytes,
            skipped.join(","),
            removed.join(","),
            items.join(",")
        )
    }

    pub fn render_human(&self, w: &mut impl Write) -> io::Result<()> {
        writeln!(
            w,
            "Since {} ({} ago). Free space: {} -> {} ({})",
            fmt_ts(self.since),
            fmt_age(self.now.saturating_sub(self.since)),
            human(self.free_before),
            human(self.free_after),
            human_delta(self.free_delta)
        )?;
        writeln!(
            w,
            "{:>9} {:>9}  PATH   (changes of at least {})",
            "CHANGE",
            "NOW",
            human(self.min_bytes)
        )?;

        let paths: BTreeSet<&str> = self.rows.keys().map(|s| s.as_str()).collect();
        let (kids, _) = tree(&paths);

        // Precompute which subtrees contain at least one significant change so
        // cancelling siblings (e.g. +20G and -20G under one root) are still shown.
        let mut has_sig: BTreeMap<&str, bool> = BTreeMap::new();
        for &p in paths.iter().rev() {
            let own = self.rows[p].delta.unsigned_abs() >= self.min_bytes;
            let child_sig = kids
                .get(p)
                .map_or(false, |cs| cs.iter().any(|c| *has_sig.get(c).unwrap_or(&false)));
            has_sig.insert(p, own || child_sig);
        }

        let mut shown = 0;
        for top in &self.tops {
            shown += self.print_diff_node(w, top.as_str(), 0, &kids, &has_sig)?;
        }
        if shown == 0 {
            writeln!(w, "  (nothing changed by {} or more)", human(self.min_bytes))?;
        }
        for (u, bytes) in &self.not_in_baseline {
            writeln!(w, "Not in the baseline, so no change shown: {} ({} now)", u, human(*bytes))?;
        }
        for (u, bytes) in &self.no_longer_monitored {
            writeln!(w, "No longer monitored, so no change shown: {} ({} in baseline)", u, human(*bytes))?;
        }
        match self.outside_delta {
            Some(out) => {
                match (self.unmonitored_volume_delta, self.other_volumes_delta) {
                    (Some(unmon), Some(other)) if other.unsigned_abs() >= (64 << 20) => {
                        writeln!(
                            w,
                            "Monitored folders changed by {}; unmonitored space on this volume by {}; other container volumes (VM/System) by {}.",
                            human_delta(self.monitored_delta),
                            human_delta(unmon),
                            human_delta(other)
                        )?;
                    }
                    _ => {
                        writeln!(
                            w,
                            "Monitored folders changed by {}; everything else on the volume by {}.",
                            human_delta(self.monitored_delta),
                            human_delta(out)
                        )?;
                    }
                }
                if let (Some(pb), Some(pa), Some(pd)) =
                    (self.purgeable_before, self.purgeable_after, self.purgeable_delta)
                {
                    if pd.unsigned_abs() >= (64 << 20) || (pa >= (512 << 20) && out > 0) {
                        writeln!(
                            w,
                            "  APFS purgeable space: {} -> {} ({}).",
                            human(pb),
                            human(pa),
                            human_delta(pd)
                        )?;
                    }
                }
                if let (Some(sb), Some(sa)) =
                    (self.apfs_snapshots_before, self.apfs_snapshots_after)
                {
                    if sa > 0 || sb != sa {
                        writeln!(
                            w,
                            "  Local APFS snapshots: {} -> {} (snapshots can pin freed blocks).",
                            sb, sa
                        )?;
                    }
                }
                let actionable = self.unmonitored_volume_delta.unwrap_or(out);
                if actionable.unsigned_abs() >= self.min_bytes.max(256 << 20) {
                    writeln!(
                        w,
                        "  Hunt unmonitored space with 'diskledger size -c ~' or track a folder with 'diskledger root add <path>'."
                    )?;
                }
            }
            None => {
                writeln!(
                    w,
                    "Monitored folders changed by {} (roots span multiple volumes; volume-wide breakdown omitted).",
                    human_delta(self.monitored_delta)
                )?;
            }
        }
        Ok(())
    }

    fn print_diff_node(
        &self,
        w: &mut impl Write,
        p: &str,
        indent: usize,
        kids: &BTreeMap<&str, Vec<&str>>,
        has_sig: &BTreeMap<&str, bool>,
    ) -> io::Result<usize> {
        if !*has_sig.get(p).unwrap_or(&false) {
            return Ok(0);
        }
        let r = &self.rows[p];
        let tag = match r.status {
            DiffStatus::New => "  [new]",
            DiffStatus::Gone => "  [gone]",
            DiffStatus::Changed => "",
        };
        let name = if indent == 0 { p } else { base_name(p) };
        writeln!(
            w,
            "{:>9} {:>9}  {}{}{}",
            human_delta(r.delta),
            human(r.after),
            "  ".repeat(indent),
            name,
            tag
        )?;
        let mut n = 1;
        if let Some(cs) = kids.get(p) {
            let mut cs = cs.clone();
            cs.sort_by(|a, b| {
                self.rows[*b]
                    .delta
                    .unsigned_abs()
                    .cmp(&self.rows[*a].delta.unsigned_abs())
                    .then_with(|| a.cmp(b))
            });
            for c in cs {
                n += self.print_diff_node(w, c, indent + 1, kids, has_sig)?;
            }
        }
        Ok(n)
    }
}

pub fn diff(base: &Snapshot, cur: &Snapshot, min: u64, json: bool) {
    let rep = compute_diff(base, cur, min);
    let mut out = io::stdout().lock();
    let _ = if json { rep.render_json(&mut out) } else { rep.render_human(&mut out) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopNode {
    pub path: String,
    pub bytes: u64,
    pub entries: u64,
    pub flagged: bool,
    pub depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopReport {
    pub ts: u64,
    pub free: u64,
    pub min_bytes: u64,
    pub max_depth: usize,
    /// Nodes in pre-order display sequence (respecting `max_depth`).
    pub tree_order: Vec<TopNode>,
    /// Nodes within `max_depth` with `bytes >= min_bytes`, sorted by `bytes` descending.
    pub dirs: Vec<TopNode>,
}

pub fn compute_top(s: &Snapshot, min: u64, max_depth: usize, under: Option<&str>) -> TopReport {
    let paths: BTreeSet<&str> = s
        .rows
        .keys()
        .map(|k| k.as_str())
        .filter(|k| under.map_or(true, |u| is_same_or_under(k, u)))
        .collect();
    let (kids, tops) = tree(&paths);

    let mut tree_order = Vec::new();
    for t in tops {
        collect_top(t, 0, &kids, s, min, max_depth, &mut tree_order);
    }

    let mut dirs: Vec<TopNode> = tree_order
        .iter()
        .filter(|n| n.bytes >= min)
        .cloned()
        .collect();
    dirs.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));

    TopReport { ts: s.ts, free: s.free, min_bytes: min, max_depth, tree_order, dirs }
}

fn collect_top(
    p: &str,
    indent: usize,
    kids: &BTreeMap<&str, Vec<&str>>,
    s: &Snapshot,
    min: u64,
    max_depth: usize,
    out: &mut Vec<TopNode>,
) {
    let r = &s.rows[p];
    if r.bytes < min && indent > 0 {
        return;
    }
    out.push(TopNode {
        path: p.to_string(),
        bytes: r.bytes,
        entries: r.entries,
        flagged: r.flagged,
        depth: indent,
    });
    if indent >= max_depth {
        return;
    }
    if let Some(cs) = kids.get(p) {
        let mut cs = cs.clone();
        cs.sort_by(|a, b| bytes_of(s, b).cmp(&bytes_of(s, a)).then_with(|| a.cmp(b)));
        for c in cs {
            collect_top(c, indent + 1, kids, s, min, max_depth, out);
        }
    }
}

impl TopReport {
    pub fn render_json(&self, w: &mut impl Write) -> io::Result<()> {
        let items: Vec<String> = self
            .dirs
            .iter()
            .map(|r| {
                format!(
                    "{{\"path\":{},\"bytes\":{},\"entries\":{},\"flagged\":{}}}",
                    json_str(&r.path),
                    r.bytes,
                    r.entries,
                    r.flagged
                )
            })
            .collect();
        writeln!(w, "{{\"ts\":{},\"free\":{},\"dirs\":[{}]}}", self.ts, self.free, items.join(","))
    }

    pub fn render_human(&self, w: &mut impl Write, now_ts: u64) -> io::Result<()> {
        writeln!(
            w,
            "Snapshot {} ({} ago). Free space: {}",
            fmt_ts(self.ts),
            fmt_age(now_ts.saturating_sub(self.ts)),
            human(self.free)
        )?;
        writeln!(
            w,
            "{:>9} {:>9}  PATH   (at least {}; * = instant kernel record)",
            "SIZE",
            "ENTRIES",
            human(self.min_bytes)
        )?;
        for n in &self.tree_order {
            let name = if n.depth == 0 { n.path.as_str() } else { base_name(&n.path) };
            writeln!(
                w,
                "{:>9} {:>9}  {}{}{}",
                human(n.bytes),
                n.entries,
                "  ".repeat(n.depth),
                name,
                if n.flagged { " *" } else { "" }
            )?;
        }
        Ok(())
    }
}

pub fn top(s: &Snapshot, min: u64, max_depth: usize, under: Option<&str>, now_ts: u64, json: bool) {
    let rep = compute_top(s, min, max_depth, under);
    let mut out = io::stdout().lock();
    let _ = if json { rep.render_json(&mut out) } else { rep.render_human(&mut out, now_ts) };
}

/// Git repositories at or below `p`, determined purely from snapshot data (no disk I/O).
fn nested_repos(cur: &Snapshot, p: &str) -> Vec<String> {
    let mut found: BTreeSet<String> = cur
        .subtree(p)
        .filter(|(_, r)| r.has_git)
        .map(|(k, _)| k.clone())
        .collect();
    for g in &cur.extra_git_repos {
        if is_same_or_under(g, p) {
            found.insert(g.clone());
        }
    }
    found.into_iter().collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateItem {
    pub path: String,
    pub bytes: u64,
    pub unchanged_since: u64,
    pub unchanged_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixedItem {
    pub path: String,
    pub bytes: u64,
    pub nested_repos: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidatesReport {
    pub ts: u64,
    pub min_bytes: u64,
    pub history_starts: u64,
    pub total_bytes: u64,
    pub has_git_metadata: bool,
    pub candidates: Vec<CandidateItem>,
    pub mixed: Vec<MixedItem>,
}

/// Regenerable-looking directories, largest first, with how long each has been unchanged.
///
/// A directory with a build-output name that holds git repositories is reported as `mixed`
/// instead of as a candidate; its repository-free direct children take its place.
/// Pure with respect to the monitored filesystem: reads only `cur` and stored snapshots.
pub fn compute_candidates(store: &Store, cur: &Snapshot, min: u64) -> CandidatesReport {
    let mut picks: Vec<&str> = Vec::new();
    let mut mixed: Vec<(&str, Vec<String>)> = Vec::new();
    // Name-matched directories that hold repositories; their direct children get considered.
    let mut opened: BTreeSet<&str> = BTreeSet::new();
    // Rows iterate parents before children, so `picks` already holds any picked ancestor.
    for (p, r) in &cur.rows {
        let p = p.as_str();
        if r.bytes < min {
            continue;
        }
        let named = REGENERABLE.contains(&base_name(p));
        let promoted = parent_of(p).map_or(false, |a| opened.contains(a));
        if !named && !promoted {
            continue;
        }
        if picks.iter().any(|a| is_under(p, a)) {
            continue;
        }
        let repos = nested_repos(cur, p);
        if repos.is_empty() {
            picks.push(p);
        } else if named {
            opened.insert(p);
            mixed.push((p, repos));
        }
    }
    picks.sort_by(|a, b| bytes_of(cur, b).cmp(&bytes_of(cur, a)).then_with(|| a.cmp(b)));

    // Walk full stored history backwards to find how long each pick has been unchanged.
    let mut unchanged_since: BTreeMap<&str, u64> = picks.iter().map(|p| (*p, cur.ts)).collect();
    let mut live: BTreeSet<&str> = picks.iter().copied().collect();
    let history = store.list();
    let oldest = history.first().copied().unwrap_or(cur.ts);
    for ts in history.iter().rev().filter(|t| **t < cur.ts) {
        if live.is_empty() {
            break;
        }
        let Ok(old) = store.load(*ts) else { continue };
        live.retain(|p| {
            let same = old.rows.get(*p).map_or(false, |o| {
                let c = &cur.rows[*p];
                o.bytes == c.bytes
                    && o.entries == c.entries
                    && (!o.flagged || !c.flagged || o.gen == c.gen)
            });
            if same {
                unchanged_since.insert(p, *ts);
            }
            same
        });
    }

    let total_bytes: u64 = picks.iter().map(|p| bytes_of(cur, p)).sum();
    let candidates = picks
        .iter()
        .map(|p| {
            let since = unchanged_since[p];
            CandidateItem {
                path: (*p).to_string(),
                bytes: bytes_of(cur, p),
                unchanged_since: since,
                unchanged_secs: cur.ts.saturating_sub(since),
            }
        })
        .collect();
    let mixed = mixed
        .into_iter()
        .map(|(p, repos)| MixedItem {
            path: p.to_string(),
            bytes: bytes_of(cur, p),
            nested_repos: repos,
        })
        .collect();

    CandidatesReport {
        ts: cur.ts,
        min_bytes: min,
        history_starts: oldest,
        total_bytes,
        has_git_metadata: cur.has_git_metadata,
        candidates,
        mixed,
    }
}

impl CandidatesReport {
    pub fn render_json(&self, w: &mut impl Write) -> io::Result<()> {
        let items: Vec<String> = self
            .candidates
            .iter()
            .map(|c| {
                format!(
                    "{{\"path\":{},\"bytes\":{},\"unchanged_since\":{},\"unchanged_secs\":{}}}",
                    json_str(&c.path),
                    c.bytes,
                    c.unchanged_since,
                    c.unchanged_secs
                )
            })
            .collect();
        let mixed_items: Vec<String> = self
            .mixed
            .iter()
            .map(|m| {
                let sample: Vec<String> = m.nested_repos.iter().take(5).map(|r| json_str(r)).collect();
                format!(
                    "{{\"path\":{},\"bytes\":{},\"nested_repo_count\":{},\"nested_repos\":[{}]}}",
                    json_str(&m.path),
                    m.bytes,
                    m.nested_repos.len(),
                    sample.join(",")
                )
            })
            .collect();
        writeln!(
            w,
            "{{\"ts\":{},\"history_starts\":{},\"total_bytes\":{},\"candidates\":[{}],\"mixed\":[{}]}}",
            self.ts,
            self.history_starts,
            self.total_bytes,
            items.join(","),
            mixed_items.join(",")
        )
    }

    pub fn render_human(&self, w: &mut impl Write) -> io::Result<()> {
        writeln!(
            w,
            "{} regenerable-looking directories of at least {}, {} in total. History goes back {}.",
            self.candidates.len(),
            human(self.min_bytes),
            human(self.total_bytes),
            fmt_age(self.ts.saturating_sub(self.history_starts))
        )?;
        writeln!(w, "{:>9} {:>10}  PATH", "SIZE", "UNCHANGED")?;
        for c in &self.candidates {
            writeln!(
                w,
                "{:>9} {:>10}  {}",
                human(c.bytes),
                fmt_age(c.unchanged_secs),
                c.path
            )?;
        }
        if !self.mixed.is_empty() {
            writeln!(w, "Not candidates: named like build output but holding git repositories.")?;
            for m in &self.mixed {
                writeln!(
                    w,
                    "{:>9} {:>10}  {}",
                    human(m.bytes),
                    format!("{} repos", m.nested_repos.len()),
                    m.path
                )?;
            }
        }
        if !self.has_git_metadata {
            writeln!(
                w,
                "Note: snapshot predates git-repo metadata; run without --no-scan to check for nested git repos."
            )?;
        }
        writeln!(w, "Matched by directory name only. Not checked: whether a process is using it, or who owns it.")
    }
}

pub fn candidates(store: &Store, cur: &Snapshot, min: u64, json: bool) {
    let rep = compute_candidates(store, cur, min);
    let mut out = io::stdout().lock();
    let _ = if json { rep.render_json(&mut out) } else { rep.render_human(&mut out) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitoredState {
    Unmonitored,
    Partial,
    Covered,
}

impl MonitoredState {
    pub fn as_str(&self) -> &'static str {
        match self {
            MonitoredState::Unmonitored => "none",
            MonitoredState::Partial => "partial",
            MonitoredState::Covered => "covered",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SizeChildrenOpts {
    pub min_bytes: u64,
    pub limit: Option<usize>,
    pub unmonitored_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSizeRow {
    pub path: String,
    pub bytes: u64,
    pub unmonitored_bytes: u64,
    pub entries: u64,
    pub instant: bool,
    pub is_dir: bool,
    pub monitored: MonitoredState,
}

impl ChildSizeRow {
    pub fn shown_bytes(&self, unmonitored_only: bool) -> u64 {
        if unmonitored_only { self.unmonitored_bytes } else { self.bytes }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeChildrenReport {
    pub dir: String,
    pub unmonitored_only: bool,
    pub min_bytes: u64,
    pub total_bytes: u64,
    pub omitted_count: usize,
    pub omitted_bytes: u64,
    pub entries: Vec<ChildSizeRow>,
    pub errors: Vec<String>,
}

pub fn compute_size_children(
    sys: &dyn System,
    dir: &Path,
    roots: &[String],
    opts: &SizeChildrenOpts,
) -> Res<SizeChildrenReport> {
    let dir_key = normalize_path_str(&dir.to_string_lossy());
    let parent_dev = match sys.dir_dev(dir) {
        Ok(Some(d)) => d,
        Ok(None) => return Err(format!("{dir_key}: not a directory")),
        Err(e) => return Err(format!("{dir_key}: {e}")),
    };
    let raw_entries = sys.list_entries(dir).map_err(|e| format!("{dir_key}: {e}"))?;

    struct WorkItem {
        path: std::path::PathBuf,
        key: String,
        is_dir: bool,
        file_bytes: u64,
        monitored: MonitoredState,
        nested_roots: Vec<String>,
    }

    let mut work: Vec<WorkItem> = Vec::with_capacity(raw_entries.len());
    for ent in raw_entries {
        if ent.dev != parent_dev {
            continue;
        }
        let key = normalize_path_str(&ent.path.to_string_lossy());
        let covered = roots.iter().any(|r| is_same_or_under(&key, r));
        if covered && opts.unmonitored_only {
            continue;
        }
        let (monitored, nested_roots) = if covered {
            (MonitoredState::Covered, Vec::new())
        } else if ent.is_dir {
            let nested: Vec<String> = roots
                .iter()
                .filter(|r| is_under(r, &key))
                .cloned()
                .collect();
            if nested.is_empty() {
                (MonitoredState::Unmonitored, Vec::new())
            } else {
                (MonitoredState::Partial, nested)
            }
        } else {
            (MonitoredState::Unmonitored, Vec::new())
        };
        work.push(WorkItem {
            path: ent.path,
            key,
            is_dir: ent.is_dir,
            file_bytes: ent.file_bytes,
            monitored,
            nested_roots,
        });
    }

    let results: Mutex<Vec<ChildSizeRow>> = Mutex::new(Vec::with_capacity(work.len()));
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 6))
        .unwrap_or(4)
        .min(work.len().max(1));

    let run_worker = || {
        loop {
            let idx = next.fetch_add(1, Ordering::Relaxed);
            if idx >= work.len() {
                break;
            }
            let item = &work[idx];
            if item.is_dir {
                match sys.query(&item.path) {
                    Ok(st) => {
                        let unmon = match item.monitored {
                            MonitoredState::Covered => 0,
                            MonitoredState::Unmonitored => st.bytes,
                            MonitoredState::Partial => {
                                let mut mon_sum = 0u64;
                                for r in &item.nested_roots {
                                    if let Ok(rs) = sys.query(Path::new(r)) {
                                        mon_sum = mon_sum.saturating_add(rs.bytes);
                                    }
                                }
                                st.bytes.saturating_sub(mon_sum)
                            }
                        };
                        results.lock().unwrap().push(ChildSizeRow {
                            path: item.key.clone(),
                            bytes: st.bytes,
                            unmonitored_bytes: unmon,
                            entries: st.entries,
                            instant: st.has_record,
                            is_dir: true,
                            monitored: item.monitored,
                        });
                    }
                    Err(e) => {
                        errors.lock().unwrap().push(format!("{}: {e}", item.key));
                    }
                }
            } else {
                let unmon = if item.monitored == MonitoredState::Covered { 0 } else { item.file_bytes };
                results.lock().unwrap().push(ChildSizeRow {
                    path: item.key.clone(),
                    bytes: item.file_bytes,
                    unmonitored_bytes: unmon,
                    entries: 0,
                    instant: false,
                    is_dir: false,
                    monitored: item.monitored,
                });
            }
        }
    };

    if threads <= 1 {
        run_worker();
    } else {
        std::thread::scope(|s| {
            for _ in 0..(threads - 1) {
                s.spawn(run_worker);
            }
            run_worker();
        });
    }

    let mut rows = results.into_inner().unwrap();
    let mut errs = errors.into_inner().unwrap();
    errs.sort();

    rows.sort_by(|a, b| {
        b.shown_bytes(opts.unmonitored_only)
            .cmp(&a.shown_bytes(opts.unmonitored_only))
            .then_with(|| a.path.cmp(&b.path))
    });

    let total_bytes: u64 = rows
        .iter()
        .map(|r| r.shown_bytes(opts.unmonitored_only))
        .sum();

    let mut kept = Vec::new();
    let mut omitted_count = 0usize;
    let mut omitted_bytes = 0u64;
    for r in rows {
        let b = r.shown_bytes(opts.unmonitored_only);
        let below_min = b < opts.min_bytes;
        let over_limit = opts.limit.map_or(false, |lim| kept.len() >= lim);
        if below_min || over_limit {
            omitted_count += 1;
            omitted_bytes = omitted_bytes.saturating_add(b);
        } else {
            kept.push(r);
        }
    }

    Ok(SizeChildrenReport {
        dir: dir_key,
        unmonitored_only: opts.unmonitored_only,
        min_bytes: opts.min_bytes,
        total_bytes,
        omitted_count,
        omitted_bytes,
        entries: kept,
        errors: errs,
    })
}

impl SizeChildrenReport {
    pub fn render_json(&self, w: &mut impl Write) -> io::Result<()> {
        let items: Vec<String> = self
            .entries
            .iter()
            .map(|r| {
                format!(
                    "{{\"path\":{},\"bytes\":{},\"unmonitored_bytes\":{},\"entries\":{},\"instant\":{},\"is_dir\":{},\"monitored\":{}}}",
                    json_str(&r.path),
                    r.bytes,
                    r.unmonitored_bytes,
                    r.entries,
                    r.instant,
                    r.is_dir,
                    json_str(r.monitored.as_str())
                )
            })
            .collect();
        let errs: Vec<String> = self.errors.iter().map(|e| json_str(e)).collect();
        writeln!(
            w,
            "{{\"path\":{},\"unmonitored_only\":{},\"min_bytes\":{},\"total_bytes\":{},\"omitted_count\":{},\"omitted_bytes\":{},\"entries\":[{}],\"errors\":[{}]}}",
            json_str(&self.dir),
            self.unmonitored_only,
            self.min_bytes,
            self.total_bytes,
            self.omitted_count,
            self.omitted_bytes,
            items.join(","),
            errs.join(",")
        )
    }

    pub fn render_kb(&self, w: &mut impl Write) -> io::Result<()> {
        for r in &self.entries {
            let b = r.shown_bytes(self.unmonitored_only);
            writeln!(w, "{}\t{}", b.div_ceil(1024), r.path)?;
        }
        Ok(())
    }

    pub fn render_human(&self, w: &mut impl Write) -> io::Result<()> {
        let hdr = if self.unmonitored_only { "UNMON" } else { "SIZE" };
        writeln!(
            w,
            "{:>9} {:>9}  PATH   (* = instant kernel record)",
            hdr, "ENTRIES"
        )?;
        for r in &self.entries {
            let shown = r.shown_bytes(self.unmonitored_only);
            let star = if r.instant { " *" } else { "" };
            let tag = match (self.unmonitored_only, r.monitored) {
                (false, MonitoredState::Covered) => "  [monitored]".to_string(),
                (false, MonitoredState::Partial) => {
                    format!("  [partial: {} unmonitored]", human(r.unmonitored_bytes))
                }
                (true, MonitoredState::Partial) => {
                    format!("  [partial: of {} total]", human(r.bytes))
                }
                _ => String::new(),
            };
            writeln!(
                w,
                "{:>9} {:>9}  {}{}{}",
                human(shown),
                r.entries,
                r.path,
                star,
                tag
            )?;
        }
        if self.omitted_count > 0 {
            writeln!(
                w,
                "  ... and {} smaller entries ({} total)",
                self.omitted_count,
                human(self.omitted_bytes)
            )?;
        }
        writeln!(
            w,
            "Total{}: {} across {}",
            if self.unmonitored_only { " (unmonitored)" } else { "" },
            human(self.total_bytes),
            self.dir
        )
    }
}
