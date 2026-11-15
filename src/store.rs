//! On-disk state: config, snapshots, named checkpoints.
//!
//! Everything lives under one directory (`$DISKLEDGER_HOME`, default
//! `~/.diskledger`). Snapshots are small TSV files; a failed write (for
//! example a full disk) never stops a command from printing its result.

use crate::util::{tsv_esc, tsv_unesc, Res};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    /// Directories with at least this many descendants get a kernel record.
    pub flag_min_entries: u64,
    /// Directories smaller than this are not recorded or descended into.
    pub record_min_bytes: u64,
    pub max_depth: u32,
    /// Do not descend into directories with more child directories than this.
    pub max_children: usize,
    pub keep_snapshots: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: Vec::new(),
            flag_min_entries: 20_000,
            record_min_bytes: 16 << 20,
            max_depth: 12,
            max_children: 2_000,
            keep_snapshots: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub bytes: u64,
    pub entries: u64,
    pub gen: u64,
    pub flagged: bool,
    /// Qualifies for a kernel record but does not have one yet.
    pub pending: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub ts: u64,
    pub free: u64,
    pub total: u64,
    pub rows: BTreeMap<String, Row>,
}

impl Snapshot {
    /// Rows at `path` and beneath it.
    pub fn subtree<'a>(&'a self, path: &'a str) -> impl Iterator<Item = (&'a String, &'a Row)> + 'a {
        self.rows
            .range(path.to_string()..)
            .take_while(move |(k, _)| k.starts_with(path))
            .filter(move |(k, _)| k.len() == path.len() || k.as_bytes()[path.len()] == b'/')
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
        self.ensure_dirs()?;
        let mut s = String::from("# diskledger config. One setting per line.\n");
        for r in &cfg.roots {
            s.push_str(&format!("root {}\n", r.display()));
        }
        s.push_str(&format!(
            "\n# Give a directory its own kernel record once it holds this many entries.\nflag_min_entries {}\n",
            cfg.flag_min_entries
        ));
        s.push_str(&format!(
            "# Ignore directories smaller than this many bytes.\nrecord_min_bytes {}\n",
            cfg.record_min_bytes
        ));
        s.push_str(&format!("max_depth {}\nmax_children {}\nkeep_snapshots {}\n", cfg.max_depth, cfg.max_children, cfg.keep_snapshots));
        fs::write(self.config_path(), s).map_err(|e| format!("write config: {e}"))
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
                "max_depth" => cfg.max_depth = num()? as u32,
                "max_children" => cfg.max_children = num()? as usize,
                "keep_snapshots" => cfg.keep_snapshots = num()? as usize,
                other => return Err(format!("config line {}: unknown setting '{other}'", n + 1)),
            }
        }
        if cfg.roots.is_empty() {
            return Err("config has no 'root' lines".into());
        }
        Ok(cfg)
    }

    fn snap_path(&self, ts: u64) -> PathBuf {
        self.home.join("snapshots").join(format!("{ts}.tsv"))
    }

    pub fn save(&self, s: &Snapshot) -> Res<()> {
        self.ensure_dirs()?;
        let mut out = format!("#diskledger v1\tts={}\tfree={}\ttotal={}\n", s.ts, s.free, s.total);
        for (p, r) in &s.rows {
            out.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", tsv_esc(p), r.bytes, r.entries, r.gen, if r.flagged { 1 } else if r.pending { 2 } else { 0 }));
        }
        let tmp = self.home.join("snapshots").join(format!(".{}.tmp", s.ts));
        fs::write(&tmp, out).and_then(|_| fs::rename(&tmp, self.snap_path(s.ts))).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("save snapshot: {e}")
        })
    }

    pub fn load(&self, ts: u64) -> Res<Snapshot> {
        let text = fs::read_to_string(self.snap_path(ts)).map_err(|e| format!("read snapshot {ts}: {e}"))?;
        let mut s = Snapshot { ts, ..Default::default() };
        for line in text.lines() {
            if let Some(h) = line.strip_prefix('#') {
                for f in h.split('\t') {
                    if let Some(v) = f.strip_prefix("free=") {
                        s.free = v.parse().unwrap_or(0);
                    } else if let Some(v) = f.strip_prefix("total=") {
                        s.total = v.parse().unwrap_or(0);
                    }
                }
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 5 {
                continue;
            }
            s.rows.insert(
                tsv_unesc(f[0]),
                Row {
                    bytes: f[1].parse().unwrap_or(0),
                    entries: f[2].parse().unwrap_or(0),
                    gen: f[3].parse().unwrap_or(0),
                    flagged: f[4] == "1",
                    pending: f[4] == "2",
                },
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
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return Err(format!("bad checkpoint name '{name}'"));
        }
        self.ensure_dirs()?;
        fs::write(self.home.join("checkpoints").join(name), ts.to_string()).map_err(|e| format!("write checkpoint: {e}"))
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
                Some((name.clone(), self.checkpoint(&name)?))
            })
            .collect();
        v.sort_by_key(|(_, ts)| *ts);
        v
    }

    /// Delete the oldest snapshots beyond `keep`, never one a checkpoint points at.
    pub fn prune(&self, keep: usize) {
        let all = self.list();
        if all.len() <= keep {
            return;
        }
        let pinned: Vec<u64> = self.checkpoints().into_iter().map(|(_, ts)| ts).collect();
        for ts in &all[..all.len() - keep] {
            if !pinned.contains(ts) {
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

pub fn canonical(p: &Path) -> Res<PathBuf> {
    fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))
}
