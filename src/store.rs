//! On-disk state: config, snapshots, named checkpoints.
//!
//! Everything lives under one directory (`$DISKLEDGER_HOME`, default
//! `~/.diskledger`). Snapshots are small TSV files; a failed write (for
//! example a full disk) never stops a command from printing its result.

use crate::util::{is_same_or_under, normalize_path_str, now, tsv_esc, tsv_unesc, Res};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    /// Directories with at least this many descendants get a kernel record.
    pub flag_min_entries: u64,
    /// Directories smaller than this are not recorded or descended into.
    pub record_min_bytes: u64,
    pub max_depth: u32,
    /// Do not descend into directories with more child directories than this.
    pub max_children: usize,
    /// Recent snapshots kept in full. Older ones thin to one per day for 30 days.
    pub keep_snapshots: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: Vec::new(),
            flag_min_entries: 5_000,
            record_min_bytes: 64 << 20,
            max_depth: 12,
            max_children: 2_000,
            keep_snapshots: 100,
        }
    }
}

impl Config {
    /// Normalize trailing slashes and reject empty, duplicate, or overlapping roots.
    pub fn validate(&mut self) -> Res<()> {
        if self.roots.is_empty() {
            return Err("config has no 'root' lines".into());
        }
        let mut normalized: Vec<String> = Vec::with_capacity(self.roots.len());
        for r in &self.roots {
            let s = normalize_path_str(&r.to_string_lossy());
            if s.is_empty() {
                return Err("config has an empty 'root' path".into());
            }
            normalized.push(s);
        }
        for i in 0..normalized.len() {
            for j in (i + 1)..normalized.len() {
                let (a, b) = (&normalized[i], &normalized[j]);
                if a == b {
                    return Err(format!("duplicate root '{a}'"));
                }
                if is_same_or_under(b, a) {
                    return Err(format!("overlapping roots: '{b}' is already inside '{a}'"));
                }
                if is_same_or_under(a, b) {
                    return Err(format!("overlapping roots: '{a}' is already inside '{b}'"));
                }
            }
        }
        self.roots = normalized.into_iter().map(PathBuf::from).collect();
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    pub bytes: u64,
    pub entries: u64,
    pub gen: u64,
    pub flagged: bool,
    /// Qualifies for a kernel record but does not have one yet.
    pub pending: bool,
    /// Direct `.git` entry observed inside this directory during scan.
    pub has_git: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub ts: u64,
    pub free: u64,
    pub total: u64,
    /// APFS purgeable bytes macOS can reclaim on demand.
    pub purgeable: Option<u64>,
    /// Bytes used by this APFS volume alone (`ATTR_VOL_SPACEUSED`).
    pub volume_used: Option<u64>,
    /// Number of local APFS snapshots on this volume at scan time.
    pub apfs_snapshots: Option<u64>,
    /// Configured roots at scan time (empty when loaded from legacy v0.1.2 snapshots).
    pub roots: Vec<String>,
    /// True when all configured roots live on the same volume.
    pub single_volume: bool,
    /// Direct-child git repos beneath regenerable dirs that were too small to have a Row.
    pub extra_git_repos: BTreeSet<String>,
    /// False only when loaded from a legacy v0.1.2 (5-column) snapshot that lacked git metadata.
    pub has_git_metadata: bool,
    pub rows: BTreeMap<String, Row>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            ts: 0,
            free: 0,
            total: 0,
            purgeable: None,
            volume_used: None,
            apfs_snapshots: None,
            roots: Vec::new(),
            single_volume: true,
            extra_git_repos: BTreeSet::new(),
            has_git_metadata: true,
            rows: BTreeMap::new(),
        }
    }
}

impl Snapshot {
    /// Bytes used by sibling volumes in the same APFS container (e.g. VM swap, System).
    pub fn other_volumes_used(&self) -> Option<u64> {
        let vu = self.volume_used?;
        Some(self.total.saturating_sub(self.free).saturating_sub(vu))
    }

    /// Rows at `path` and beneath it.
    pub fn subtree<'a>(&'a self, path: &'a str) -> impl Iterator<Item = (&'a String, &'a Row)> + 'a {
        self.rows
            .range(path.to_string()..)
            .take_while(move |(k, _)| k.starts_with(path))
            .filter(move |(k, _)| is_same_or_under(k, path))
    }
}

pub struct Store {
    pub home: PathBuf,
}

impl Store {
    pub fn open() -> Res<Store> {
        let home = match std::env::var_os("DISKLEDGER_HOME") {
            Some(h) => PathBuf::from(h),
            None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME not set")?).join(".diskledger"),
        };
        Ok(Store { home })
    }

    pub fn open_at(home: PathBuf) -> Store {
        Store { home }
    }

    fn ensure_dirs(&self) -> Res<()> {
        for d in ["snapshots", "checkpoints"] {
            fs::create_dir_all(self.home.join(d)).map_err(|e| format!("create {}: {e}", self.home.display()))?;
        }
        Ok(())
    }

    pub fn config_path(&self) -> PathBuf {
        self.home.join("config")
    }

    pub fn write_config(&self, cfg: &Config) -> Res<()> {
        let mut validated = cfg.clone();
        validated.validate()?;
        self.ensure_dirs()?;
        let mut s = String::from("# diskledger config. One setting per line.\n");
        for r in &validated.roots {
            s.push_str(&format!("root {}\n", r.display()));
        }
        s.push_str(&format!(
            "\n# Give a directory its own kernel record once it holds this many entries.\nflag_min_entries {}\n",
            validated.flag_min_entries
        ));
        s.push_str(&format!(
            "# Ignore directories smaller than this many bytes.\nrecord_min_bytes {}\n",
            validated.record_min_bytes
        ));
        s.push_str(&format!(
            "max_depth {}\nmax_children {}\nkeep_snapshots {}\n",
            validated.max_depth, validated.max_children, validated.keep_snapshots
        ));
        let tmp = self.home.join(unique_tmp_name("config"));
        fs::write(&tmp, s).and_then(|_| fs::rename(&tmp, self.config_path())).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("write config: {e}")
        })
    }

    pub fn config(&self) -> Res<Config> {
        let text = fs::read_to_string(self.config_path())
            .map_err(|_| format!("no config at {}. Run: diskledger init <dir>...", self.config_path().display()))?;
        let mut cfg = Config::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, val) = line.split_once(char::is_whitespace).ok_or(format!("config line {}: missing value", n + 1))?;
            let val = val.trim();
            let num = || val.parse::<u64>().map_err(|_| format!("config line {}: '{val}' is not a number", n + 1));
            match key {
                "root" => cfg.roots.push(PathBuf::from(val)),
                "flag_min_entries" => cfg.flag_min_entries = num()?,
                "record_min_bytes" => cfg.record_min_bytes = num()?,
                "max_depth" => {
                    let v = num()?;
                    cfg.max_depth = u32::try_from(v).map_err(|_| format!("config line {}: max_depth '{v}' out of range", n + 1))?;
                }
                "max_children" => {
                    let v = num()?;
                    cfg.max_children = usize::try_from(v).map_err(|_| format!("config line {}: max_children '{v}' out of range", n + 1))?;
                }
                "keep_snapshots" => {
                    let v = num()?;
                    cfg.keep_snapshots = usize::try_from(v).map_err(|_| format!("config line {}: keep_snapshots '{v}' out of range", n + 1))?;
                }
                other => return Err(format!("config line {}: unknown setting '{other}'", n + 1)),
            }
        }
        cfg.validate()?;
        Ok(cfg)
    }

    fn snap_path(&self, ts: u64) -> PathBuf {
        self.home.join("snapshots").join(format!("{ts}.tsv"))
    }

    pub fn save(&self, s: &Snapshot) -> Res<()> {
        self.ensure_dirs()?;
        let mut out = format!(
            "#diskledger v1\tts={}\tfree={}\ttotal={}\tsingle_volume={}",
            s.ts,
            s.free,
            s.total,
            if s.single_volume { 1 } else { 0 }
        );
        if let Some(p) = s.purgeable {
            out.push_str(&format!("\tpurgeable={p}"));
        }
        if let Some(vu) = s.volume_used {
            out.push_str(&format!("\tvol_used={vu}"));
        }
        if let Some(sn) = s.apfs_snapshots {
            out.push_str(&format!("\tapfs_snaps={sn}"));
        }
        out.push('\n');
        if !s.roots.is_empty() {
            out.push_str("#roots");
            for r in &s.roots {
                out.push('\t');
                out.push_str(&tsv_esc(r));
            }
            out.push('\n');
        }
        for g in &s.extra_git_repos {
            out.push_str(&format!("#git\t{}\n", tsv_esc(g)));
        }
        for (p, r) in &s.rows {
            let state = if r.flagged { 1 } else if r.pending { 2 } else { 0 };
            let git = if r.has_git { 1 } else { 0 };
            out.push_str(&format!("{}\t{}\t{}\t{}\t{}\t{}\n", tsv_esc(p), r.bytes, r.entries, r.gen, state, git));
        }
        let tmp = self.home.join("snapshots").join(unique_tmp_name(&s.ts.to_string()));
        fs::write(&tmp, out).and_then(|_| fs::rename(&tmp, self.snap_path(s.ts))).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("save snapshot: {e}")
        })
    }

    pub fn load(&self, ts: u64) -> Res<Snapshot> {
        let text = fs::read_to_string(self.snap_path(ts)).map_err(|e| format!("read snapshot {ts}: {e}"))?;
        let mut s = Snapshot { ts, ..Default::default() };
        for (idx, line) in text.lines().enumerate() {
            let lineno = idx + 1;
            if line.is_empty() {
                continue;
            }
            if let Some(h) = line.strip_prefix('#') {
                if let Some(rest) = h.strip_prefix("roots\t") {
                    for r in rest.split('\t') {
                        if !r.is_empty() {
                            s.roots.push(normalize_path_str(&tsv_unesc(r)));
                        }
                    }
                } else if let Some(rest) = h.strip_prefix("git\t") {
                    s.extra_git_repos.insert(tsv_unesc(rest));
                } else {
                    for f in h.split('\t') {
                        if let Some(v) = f.strip_prefix("free=") {
                            s.free = v.parse().map_err(|_| format!("corrupt snapshot {ts} line {lineno}: bad free '{v}'"))?;
                        } else if let Some(v) = f.strip_prefix("total=") {
                            s.total = v.parse().map_err(|_| format!("corrupt snapshot {ts} line {lineno}: bad total '{v}'"))?;
                        } else if let Some(v) = f.strip_prefix("single_volume=") {
                            s.single_volume = match v {
                                "1" => true,
                                "0" => false,
                                _ => return Err(format!("corrupt snapshot {ts} line {lineno}: bad single_volume '{v}'")),
                            };
                        } else if let Some(v) = f.strip_prefix("purgeable=") {
                            s.purgeable = Some(v.parse().map_err(|_| format!("corrupt snapshot {ts} line {lineno}: bad purgeable '{v}'"))?);
                        } else if let Some(v) = f.strip_prefix("vol_used=") {
                            s.volume_used = Some(v.parse().map_err(|_| format!("corrupt snapshot {ts} line {lineno}: bad vol_used '{v}'"))?);
                        } else if let Some(v) = f.strip_prefix("apfs_snaps=") {
                            s.apfs_snapshots = Some(v.parse().map_err(|_| format!("corrupt snapshot {ts} line {lineno}: bad apfs_snaps '{v}'"))?);
                        }
                    }
                }
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 5 && f.len() != 6 {
                return Err(format!("corrupt snapshot {ts} line {lineno}: expected 5 or 6 columns, got {}", f.len()));
            }
            let bytes = f[1].parse().map_err(|_| format!("corrupt snapshot {ts} line {lineno}: bad bytes '{}'", f[1]))?;
            let entries = f[2].parse().map_err(|_| format!("corrupt snapshot {ts} line {lineno}: bad entries '{}'", f[2]))?;
            let gen = f[3].parse().map_err(|_| format!("corrupt snapshot {ts} line {lineno}: bad gen '{}'", f[3]))?;
            let (flagged, pending) = match f[4] {
                "0" => (false, false),
                "1" => (true, false),
                "2" => (false, true),
                other => return Err(format!("corrupt snapshot {ts} line {lineno}: bad state '{other}'")),
            };
            let has_git = if f.len() == 6 {
                match f[5] {
                    "0" => false,
                    "1" => true,
                    other => return Err(format!("corrupt snapshot {ts} line {lineno}: bad git flag '{other}'")),
                }
            } else {
                s.has_git_metadata = false;
                false
            };
            s.rows.insert(
                tsv_unesc(f[0]),
                Row { bytes, entries, gen, flagged, pending, has_git },
            );
        }
        Ok(s)
    }

    /// Snapshot timestamps, oldest first.
    pub fn list(&self) -> Vec<u64> {
        let mut v: Vec<u64> = fs::read_dir(self.home.join("snapshots"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".tsv")).and_then(|n| n.parse().ok()))
            .collect();
        v.sort_unstable();
        v
    }

    pub fn latest(&self) -> Option<Snapshot> {
        self.list().last().and_then(|&ts| self.load(ts).ok())
    }

    pub fn set_checkpoint(&self, name: &str, ts: u64) -> Res<()> {
        if name.is_empty()
            || name.contains('/')
            || name.contains('\0')
            || name.starts_with('.')
            || name.chars().any(|c| c.is_control())
        {
            return Err(format!("bad checkpoint name '{name}'"));
        }
        self.ensure_dirs()?;
        let dir = self.home.join("checkpoints");
        let tmp = dir.join(unique_tmp_name("cp"));
        fs::write(&tmp, ts.to_string())
            .and_then(|_| fs::rename(&tmp, dir.join(name)))
            .map_err(|e| {
                let _ = fs::remove_file(&tmp);
                format!("write checkpoint: {e}")
            })
    }

    pub fn checkpoint(&self, name: &str) -> Option<u64> {
        fs::read_to_string(self.home.join("checkpoints").join(name)).ok()?.trim().parse().ok()
    }

    pub fn checkpoints(&self) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = fs::read_dir(self.home.join("checkpoints"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                if name.starts_with('.') {
                    return None;
                }
                Some((name.clone(), self.checkpoint(&name)?))
            })
            .collect();
        v.sort_by_key(|(_, ts)| *ts);
        v
    }

    /// Keep the newest `keep` snapshots. Of the older ones keep the last of
    /// each UTC day for 30 days. Never delete one a checkpoint points at.
    pub fn prune(&self, keep: usize) {
        self.prune_at(keep, now());
    }

    pub fn prune_at(&self, keep: usize, now_ts: u64) {
        let all = self.list();
        if all.len() <= keep {
            return;
        }
        let pinned: HashSet<u64> = self.checkpoints().into_iter().map(|(_, ts)| ts).collect();
        let cutoff = now_ts.saturating_sub(30 * 86400);
        let mut days_kept = HashSet::new();
        for ts in all[..all.len() - keep].iter().rev() {
            let daily = *ts >= cutoff && days_kept.insert(*ts / 86400);
            if !daily && !pinned.contains(ts) {
                let _ = fs::remove_file(self.snap_path(*ts));
            }
        }
    }

    /// Exclusive lock so two scans never flag or write at once. Held until the file drops.
    pub fn lock(&self) -> Res<fs::File> {
        self.ensure_dirs()?;
        let f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.home.join("scan.lock"))
            .map_err(|e| format!("open lock: {e}"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("another diskledger scan is running".into());
        }
        Ok(f)
    }

    /// Overwritten on every scan so it never grows.
    pub fn write_last_scan(&self, text: &str) {
        if let Ok(mut f) = fs::File::create(self.home.join("last-scan.txt")) {
            let _ = f.write_all(text.as_bytes());
        }
    }

    pub fn last_scan(&self) -> Option<String> {
        fs::read_to_string(self.home.join("last-scan.txt")).ok()
    }
}

fn unique_tmp_name(tag: &str) -> String {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(".{tag}.{}.{seq}.tmp", std::process::id())
}

pub fn canonical(p: &Path) -> Res<PathBuf> {
    fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))
}
