//! Human and JSON output for diff, top and candidates.

use crate::store::{Snapshot, Store};
use crate::util::{base_name, fmt_age, fmt_ts, human, human_delta, json_str, now, parent_of};
use std::collections::{BTreeMap, BTreeSet};

/// Directory names that usually hold regenerable output.
const REGENERABLE: &[&str] = &[
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

pub fn diff(base: &Snapshot, cur: &Snapshot, min: u64, json: bool) {
    let paths: BTreeSet<&str> = base.rows.keys().chain(cur.rows.keys()).map(|s| s.as_str()).collect();
    let delta = |p: &str| bytes_of(cur, p) as i64 - bytes_of(base, p) as i64;
    let free_delta = cur.free as i64 - base.free as i64;
    let (kids, tops) = tree(&paths);
    // A monitored directory added after the baseline was taken has no
    // "before", so report it separately instead of as growth.
    let unbaselined: Vec<&str> = tops.iter().copied().filter(|t| !base.rows.contains_key(*t)).collect();
    let covered = |p: &str| {
        !unbaselined.iter().any(|u| p == *u || (p.starts_with(u) && p.as_bytes().get(u.len()) == Some(&b'/')))
    };
    // How much of the free-space change the monitored folders account for.
    // The rest happened somewhere this tool is not watching.
    let monitored_delta: i64 = tops.iter().copied().filter(|t| covered(t)).map(|t| delta(t)).sum();
    let outside_delta = -free_delta - monitored_delta;

    if json {
        let mut rows: Vec<&str> = paths.iter().copied().filter(|p| covered(p) && delta(p).unsigned_abs() >= min).collect();
        rows.sort_by_key(|p| std::cmp::Reverse(delta(p).unsigned_abs()));
        let skipped: Vec<String> = unbaselined.iter().map(|u| json_str(u)).collect();
        let items: Vec<String> = rows
            .iter()
            .map(|p| {
                format!(
                    "{{\"path\":{},\"before\":{},\"after\":{},\"delta\":{}}}",
                    json_str(p),
                    bytes_of(base, p),
                    bytes_of(cur, p),
                    delta(p)
                )
            })
            .collect();
        println!(
            "{{\"since\":{},\"now\":{},\"free_before\":{},\"free_after\":{},\"free_delta\":{},\"monitored_delta\":{},\"outside_delta\":{},\"min_bytes\":{},\"not_in_baseline\":[{}],\"changes\":[{}]}}",
            base.ts,
            cur.ts,
            base.free,
            cur.free,
            free_delta,
            monitored_delta,
            outside_delta,
            min,
            skipped.join(","),
            items.join(",")
        );
        return;
    }

    println!(
        "Since {} ({} ago). Free space: {} -> {} ({})",
        fmt_ts(base.ts),
        fmt_age(cur.ts.saturating_sub(base.ts)),
        human(base.free),
        human(cur.free),
        human_delta(free_delta)
    );
    println!("{:>9} {:>9}  PATH   (changes of at least {})", "CHANGE", "NOW", human(min));
    let mut shown = 0;
    for top in tops.iter().copied().filter(|t| covered(t)) {
        shown += print_diff(top, 0, &kids, &delta, cur, base, min);
    }
    if shown == 0 {
        println!("  (nothing changed by {} or more)", human(min));
    }
    for u in &unbaselined {
        println!("Not in the baseline, so no change shown: {} ({} now)", u, human(bytes_of(cur, u)));
    }
    println!(
        "Monitored folders changed by {}; everything else on the volume by {}.",
        human_delta(monitored_delta),
        human_delta(outside_delta)
    );
    if outside_delta.unsigned_abs() >= min.max(256 << 20) {
        println!("  That is space used outside the monitored folders. Find it with 'diskledger size <path>' or add the folder as a root.");
    }
}

fn print_diff(
    p: &str,
    indent: usize,
    kids: &BTreeMap<&str, Vec<&str>>,
    delta: &dyn Fn(&str) -> i64,
    cur: &Snapshot,
    base: &Snapshot,
    min: u64,
) -> usize {
    let d = delta(p);
    if d.unsigned_abs() < min {
        return 0;
    }
    let tag = if !base.rows.contains_key(p) {
        "  [new]"
    } else if !cur.rows.contains_key(p) {
        "  [gone]"
    } else {
        ""
    };
    let name = if indent == 0 { p } else { base_name(p) };
    println!("{:>9} {:>9}  {}{}{}", human_delta(d), human(bytes_of(cur, p)), "  ".repeat(indent), name, tag);
    let mut n = 1;
    if let Some(cs) = kids.get(p) {
        let mut cs = cs.clone();
        cs.sort_by_key(|c| std::cmp::Reverse(delta(c).unsigned_abs()));
        for c in cs {
            n += print_diff(c, indent + 1, kids, delta, cur, base, min);
        }
    }
    n
}

pub fn top(s: &Snapshot, min: u64, max_depth: usize, under: Option<&str>, json: bool) {
    let paths: BTreeSet<&str> = s
        .rows
        .keys()
        .map(|k| k.as_str())
        .filter(|k| under.map_or(true, |u| *k == u || (k.starts_with(u) && k.as_bytes().get(u.len()) == Some(&b'/'))))
        .collect();
    if json {
        let mut rows: Vec<&str> = paths.iter().copied().filter(|p| bytes_of(s, p) >= min).collect();
        rows.sort_by_key(|p| std::cmp::Reverse(bytes_of(s, p)));
        let items: Vec<String> = rows
            .iter()
            .map(|p| {
                let r = &s.rows[*p];
                format!("{{\"path\":{},\"bytes\":{},\"entries\":{},\"flagged\":{}}}", json_str(p), r.bytes, r.entries, r.flagged)
            })
            .collect();
        println!("{{\"ts\":{},\"free\":{},\"dirs\":[{}]}}", s.ts, s.free, items.join(","));
        return;
    }
    println!("Snapshot {} ({} ago). Free space: {}", fmt_ts(s.ts), fmt_age(now().saturating_sub(s.ts)), human(s.free));
    println!("{:>9} {:>9}  PATH   (at least {}; * = instant kernel record)", "SIZE", "ENTRIES", human(min));
    let (kids, tops) = tree(&paths);
    for t in tops {
        print_top(t, 0, &kids, s, min, max_depth);
    }
}

fn print_top(p: &str, indent: usize, kids: &BTreeMap<&str, Vec<&str>>, s: &Snapshot, min: u64, max_depth: usize) {
    let r = &s.rows[p];
    if r.bytes < min && indent > 0 {
        return;
    }
    let name = if indent == 0 { p } else { base_name(p) };
    println!("{:>9} {:>9}  {}{}{}", human(r.bytes), r.entries, "  ".repeat(indent), name, if r.flagged { " *" } else { "" });
    if indent >= max_depth {
        return;
    }
    if let Some(cs) = kids.get(p) {
        let mut cs = cs.clone();
        cs.sort_by_key(|c| std::cmp::Reverse(bytes_of(s, c)));
        for c in cs {
            print_top(c, indent + 1, kids, s, min, max_depth);
        }
    }
}

/// Git repositories at or below `p`: recorded directories, plus `p`'s direct children, that
/// hold a `.git` entry. Small repositories nested deeper than one level are not seen.
fn nested_repos(cur: &Snapshot, p: &str) -> Vec<String> {
    let is_repo = |d: &str| std::path::Path::new(d).join(".git").exists();
    let sub = format!("{p}/");
    let mut found: BTreeSet<String> = cur
        .rows
        .range::<str, _>((std::ops::Bound::Included(p), std::ops::Bound::Unbounded))
        .map(|(k, _)| k.as_str())
        .take_while(|k| k.starts_with(p))
        .filter(|k| (*k == p || k.starts_with(&sub)) && is_repo(k))
        .map(str::to_string)
        .collect();
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if let Some(c) = e.path().to_str() {
                if e.file_type().map_or(false, |t| t.is_dir()) && is_repo(c) {
                    found.insert(c.to_string());
                }
            }
        }
    }
    found.into_iter().collect()
}

/// Regenerable-looking directories, largest first, with how long each has been unchanged.
///
/// A directory with a build-output name that holds git repositories is reported as `mixed`
/// instead of as a candidate; its repository-free direct children take its place.
pub fn candidates(store: &Store, cur: &Snapshot, min: u64, json: bool) {
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
        if picks.iter().any(|a| p.starts_with(a) && p.as_bytes().get(a.len()) == Some(&b'/')) {
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
    picks.sort_by_key(|p| std::cmp::Reverse(bytes_of(cur, p)));

    // Walk history backwards to find how long each pick has been unchanged.
    let mut unchanged_since: BTreeMap<&str, u64> = picks.iter().map(|p| (*p, cur.ts)).collect();
    let mut live: BTreeSet<&str> = picks.iter().copied().collect();
    let history = store.list();
    let oldest = history.first().copied().unwrap_or(cur.ts);
    for ts in history.iter().rev().filter(|t| **t < cur.ts).take(200) {
        if live.is_empty() {
            break;
        }
        let Ok(old) = store.load(*ts) else { continue };
        live.retain(|p| {
            let same = old.rows.get(*p).map_or(false, |o| {
                let c = &cur.rows[*p];
                o.bytes == c.bytes && (!o.flagged || !c.flagged || o.gen == c.gen)
            });
            if same {
                unchanged_since.insert(p, *ts);
            }
            same
        });
    }

    let total: u64 = picks.iter().map(|p| bytes_of(cur, p)).sum();
    if json {
        let items: Vec<String> = picks
            .iter()
            .map(|p| {
                format!(
                    "{{\"path\":{},\"bytes\":{},\"unchanged_since\":{},\"unchanged_secs\":{}}}",
                    json_str(p),
                    bytes_of(cur, p),
                    unchanged_since[p],
                    cur.ts.saturating_sub(unchanged_since[p])
                )
            })
            .collect();
        let mixed_items: Vec<String> = mixed
            .iter()
            .map(|(p, repos)| {
                let sample: Vec<String> = repos.iter().take(5).map(|r| json_str(r)).collect();
                format!(
                    "{{\"path\":{},\"bytes\":{},\"nested_repo_count\":{},\"nested_repos\":[{}]}}",
                    json_str(p),
                    bytes_of(cur, p),
                    repos.len(),
                    sample.join(",")
                )
            })
            .collect();
        println!(
            "{{\"ts\":{},\"history_starts\":{},\"total_bytes\":{},\"candidates\":[{}],\"mixed\":[{}]}}",
            cur.ts,
            oldest,
            total,
            items.join(","),
            mixed_items.join(",")
        );
        return;
    }
    println!(
        "{} regenerable-looking directories of at least {}, {} in total. History goes back {}.",
        picks.len(),
        human(min),
        human(total),
        fmt_age(cur.ts.saturating_sub(oldest))
    );
    println!("{:>9} {:>10}  PATH", "SIZE", "UNCHANGED");
    for p in &picks {
        println!("{:>9} {:>10}  {}", human(bytes_of(cur, p)), fmt_age(cur.ts.saturating_sub(unchanged_since[p])), p);
    }
    if !mixed.is_empty() {
        println!("Not candidates: named like build output but holding git repositories.");
        for (p, repos) in &mixed {
            println!("{:>9} {:>10}  {}", human(bytes_of(cur, p)), format!("{} repos", repos.len()), p);
        }
    }
    println!("Matched by directory name only. Not checked: whether a process is using it, or who owns it.");
}
