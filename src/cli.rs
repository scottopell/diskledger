use crate::kstat;
use crate::report;
use crate::scan::{self, ScanOpts, ScanReport};
use crate::store::{self, Config, Snapshot, Store};
use crate::sys::{RealSystem, System};
use crate::util::{fmt_age, fmt_ts, human, json_str, normalize_path_str, now, parse_duration, parse_size, Res};
use std::io;
use std::path::PathBuf;

pub const HELP: &str = "\
diskledger: what grew, what shrank, and what could be freed (macOS / APFS)

USAGE
  diskledger init <dir>...          Create the config with these monitored directories
  diskledger scan [--dry-run] [--full]
                                    Record sizes and flag large directories for instant
                                    kernel totals. This is what the periodic job runs.
                                    --dry-run lists what would be flagged and flags nothing.
                                    --full re-examines every directory; use it after
                                    changing flag_min_entries in the config.
  diskledger diff [--since X]       Show what changed. X is a checkpoint name, an age
                                    (90m, 2h, 3d) or a snapshot timestamp. Default: the
                                    'checkin' checkpoint, else the previous snapshot.
  diskledger checkin                Mark now as the baseline for the next plain 'diff'
  diskledger checkpoint <name>      Save a named baseline
  diskledger size <path>...         Size of any path right now, like 'du -s'. Instant for
                                    flagged directories; otherwise the kernel walks it.
                                    -k prints 'KiB<TAB>path', the same as 'du -sk'.
  diskledger top [dir]              Largest directories in the latest snapshot
  diskledger candidates             Regenerable-looking directories (target, node_modules, ...)
  diskledger status                 Free space, roots, snapshots, checkpoints
  diskledger doctor                 Verify the private APFS fsctl ABI against apfs.util -S
  diskledger agent install|uninstall|print [--interval SECS]
                                    Run 'scan' periodically via launchd (default 900s)
  diskledger --version

OPTIONS
  --json            Machine-readable output (diff, size, top, candidates, status, doctor)
  --min SIZE        Smallest size or change to show, e.g. 100M, 2G
  --depth N         Levels to show in 'top' (default 4)
  --no-scan         Use the latest stored snapshot; do not look at the disk
  -v, --verbose     Print each directory as it is flagged

Only 'scan' flags directories. Flagging cannot be undone and walks the directory once.
State lives in $DISKLEDGER_HOME (default ~/.diskledger).";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    Install { interval: u64 },
    Uninstall,
    Print { interval: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Init { dirs: Vec<String>, force: bool },
    Scan { dry_run: bool, full: bool, verbose: bool },
    Diff { since: Option<String>, min: u64, no_scan: bool, json: bool },
    Checkin { no_scan: bool },
    Checkpoint { name: String, no_scan: bool },
    Size { paths: Vec<String>, kb: bool, json: bool },
    Top { under: Option<String>, min: u64, depth: usize, no_scan: bool, json: bool },
    Candidates { min: u64, no_scan: bool, json: bool },
    Status { json: bool },
    Doctor { json: bool },
    Agent { action: AgentAction },
}

/// Tokenize flags and positionals for one subcommand, honoring `--` and `--opt=val`.
struct RawTokens {
    pos: Vec<String>,
    opts: Vec<(String, Option<String>)>,
    help: bool,
}

fn tokenize(raw: &[String], valued: &[&str]) -> Res<RawTokens> {
    let mut out = RawTokens { pos: Vec::new(), opts: Vec::new(), help: false };
    let mut i = 0;
    let mut only_pos = false;
    while i < raw.len() {
        let arg = &raw[i];
        i += 1;
        if only_pos {
            out.pos.push(arg.clone());
            continue;
        }
        if arg == "--" {
            only_pos = true;
            continue;
        }
        if arg == "-h" || arg == "--help" {
            out.help = true;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if let Some((name, val)) = rest.split_once('=') {
                out.opts.push((format!("--{name}"), Some(val.to_string())));
            } else if valued.contains(&arg.as_str()) {
                let val = raw.get(i).ok_or_else(|| format!("{arg} needs a value"))?;
                i += 1;
                out.opts.push((arg.clone(), Some(val.clone())));
            } else {
                out.opts.push((arg.clone(), None));
            }
        } else if arg.starts_with('-') && arg.len() > 1 {
            out.opts.push((arg.clone(), None));
        } else {
            out.pos.push(arg.clone());
        }
    }
    Ok(out)
}

fn require_no_val(opt: &str, val: Option<String>) -> Res<()> {
    if val.is_some() {
        Err(format!("{opt} does not take a value"))
    } else {
        Ok(())
    }
}

fn require_val(opt: &str, val: Option<String>) -> Res<String> {
    val.ok_or_else(|| format!("{opt} needs a value"))
}

fn reject_extra_pos(cmd: &str, pos: &[String], max: usize) -> Res<()> {
    if pos.len() > max {
        Err(format!("unexpected argument '{}' for '{cmd}'", pos[max]))
    } else {
        Ok(())
    }
}

pub fn parse_args(raw: &[String]) -> Res<Command> {
    let Some(first) = raw.first() else {
        return Ok(Command::Help);
    };
    if first == "help" || first == "--help" || first == "-h" {
        return Ok(Command::Help);
    }
    if first == "version" || first == "--version" || first == "-V" {
        return Ok(Command::Version);
    }
    let cmd = first.as_str();
    let rest = &raw[1..];

    match cmd {
        "init" => {
            let t = tokenize(rest, &[])?;
            if t.help {
                return Ok(Command::Help);
            }
            let mut force = false;
            for (k, v) in t.opts {
                match k.as_str() {
                    "--force" => {
                        require_no_val(&k, v)?;
                        force = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'init'")),
                }
            }
            if t.pos.is_empty() {
                return Err("usage: diskledger init [--force] <dir>...".into());
            }
            Ok(Command::Init { dirs: t.pos, force })
        }
        "scan" => {
            let t = tokenize(rest, &[])?;
            if t.help {
                return Ok(Command::Help);
            }
            let (mut dry_run, mut full, mut verbose) = (false, false, false);
            for (k, v) in t.opts {
                match k.as_str() {
                    "--dry-run" => {
                        require_no_val(&k, v)?;
                        dry_run = true;
                    }
                    "--full" => {
                        require_no_val(&k, v)?;
                        full = true;
                    }
                    "-v" | "--verbose" => {
                        require_no_val(&k, v)?;
                        verbose = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'scan'")),
                }
            }
            reject_extra_pos("scan", &t.pos, 0)?;
            Ok(Command::Scan { dry_run, full, verbose })
        }
        "diff" => {
            let t = tokenize(rest, &["--since", "--min"])?;
            if t.help {
                return Ok(Command::Help);
            }
            let mut since = None;
            let mut min = 100u64 << 20;
            let (mut no_scan, mut json) = (false, false);
            for (k, v) in t.opts {
                match k.as_str() {
                    "--since" => since = Some(require_val(&k, v)?),
                    "--min" => min = parse_size(&require_val(&k, v)?)?,
                    "--no-scan" => {
                        require_no_val(&k, v)?;
                        no_scan = true;
                    }
                    "--json" => {
                        require_no_val(&k, v)?;
                        json = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'diff'")),
                }
            }
            reject_extra_pos("diff", &t.pos, 0)?;
            Ok(Command::Diff { since, min, no_scan, json })
        }
        "checkin" => {
            let t = tokenize(rest, &[])?;
            if t.help {
                return Ok(Command::Help);
            }
            let mut no_scan = false;
            for (k, v) in t.opts {
                match k.as_str() {
                    "--no-scan" => {
                        require_no_val(&k, v)?;
                        no_scan = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'checkin'")),
                }
            }
            reject_extra_pos("checkin", &t.pos, 0)?;
            Ok(Command::Checkin { no_scan })
        }
        "checkpoint" => {
            let t = tokenize(rest, &[])?;
            if t.help {
                return Ok(Command::Help);
            }
            let mut no_scan = false;
            for (k, v) in t.opts {
                match k.as_str() {
                    "--no-scan" => {
                        require_no_val(&k, v)?;
                        no_scan = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'checkpoint'")),
                }
            }
            reject_extra_pos("checkpoint", &t.pos, 1)?;
            let name = t.pos.into_iter().next().ok_or("usage: diskledger checkpoint <name>")?;
            Ok(Command::Checkpoint { name, no_scan })
        }
        "size" => {
            let t = tokenize(rest, &[])?;
            if t.help {
                return Ok(Command::Help);
            }
            let (mut kb, mut json) = (false, false);
            for (k, v) in t.opts {
                match k.as_str() {
                    "-k" | "--kb" => {
                        require_no_val(&k, v)?;
                        kb = true;
                    }
                    "--json" => {
                        require_no_val(&k, v)?;
                        json = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'size'")),
                }
            }
            if t.pos.is_empty() {
                return Err("usage: diskledger size [-k] [--json] <path>...".into());
            }
            Ok(Command::Size { paths: t.pos, kb, json })
        }
        "top" => {
            let t = tokenize(rest, &["--min", "--depth"])?;
            if t.help {
                return Ok(Command::Help);
            }
            let mut min = 1u64 << 30;
            let mut depth = 4usize;
            let (mut no_scan, mut json) = (false, false);
            for (k, v) in t.opts {
                match k.as_str() {
                    "--min" => min = parse_size(&require_val(&k, v)?)?,
                    "--depth" => {
                        let s = require_val(&k, v)?;
                        depth = s.parse().map_err(|_| "--depth needs a non-negative integer".to_string())?;
                    }
                    "--no-scan" => {
                        require_no_val(&k, v)?;
                        no_scan = true;
                    }
                    "--json" => {
                        require_no_val(&k, v)?;
                        json = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'top'")),
                }
            }
            reject_extra_pos("top", &t.pos, 1)?;
            Ok(Command::Top { under: t.pos.into_iter().next(), min, depth, no_scan, json })
        }
        "candidates" => {
            let t = tokenize(rest, &["--min"])?;
            if t.help {
                return Ok(Command::Help);
            }
            let mut min = 500u64 << 20;
            let (mut no_scan, mut json) = (false, false);
            for (k, v) in t.opts {
                match k.as_str() {
                    "--min" => min = parse_size(&require_val(&k, v)?)?,
                    "--no-scan" => {
                        require_no_val(&k, v)?;
                        no_scan = true;
                    }
                    "--json" => {
                        require_no_val(&k, v)?;
                        json = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'candidates'")),
                }
            }
            reject_extra_pos("candidates", &t.pos, 0)?;
            Ok(Command::Candidates { min, no_scan, json })
        }
        "status" => {
            let t = tokenize(rest, &[])?;
            if t.help {
                return Ok(Command::Help);
            }
            let mut json = false;
            for (k, v) in t.opts {
                match k.as_str() {
                    "--json" => {
                        require_no_val(&k, v)?;
                        json = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'status'")),
                }
            }
            reject_extra_pos("status", &t.pos, 0)?;
            Ok(Command::Status { json })
        }
        "doctor" => {
            let t = tokenize(rest, &[])?;
            if t.help {
                return Ok(Command::Help);
            }
            let mut json = false;
            for (k, v) in t.opts {
                match k.as_str() {
                    "--json" => {
                        require_no_val(&k, v)?;
                        json = true;
                    }
                    _ => return Err(format!("unknown option '{k}' for 'doctor'")),
                }
            }
            reject_extra_pos("doctor", &t.pos, 0)?;
            Ok(Command::Doctor { json })
        }
        "agent" => {
            let t = tokenize(rest, &["--interval"])?;
            if t.help {
                return Ok(Command::Help);
            }
            let mut interval: Option<u64> = None;
            for (k, v) in t.opts {
                match k.as_str() {
                    "--interval" => {
                        let s = require_val(&k, v)?;
                        let secs: u64 = s.parse().map_err(|_| "--interval needs seconds".to_string())?;
                        if secs < 60 {
                            return Err("--interval must be at least 60 seconds".into());
                        }
                        interval = Some(secs);
                    }
                    _ => return Err(format!("unknown option '{k}' for 'agent'")),
                }
            }
            reject_extra_pos("agent", &t.pos, 1)?;
            let action_str = t.pos.first().map(|s| s.as_str()).unwrap_or("");
            let action = match action_str {
                "install" => AgentAction::Install { interval: interval.unwrap_or(900) },
                "print" => AgentAction::Print { interval: interval.unwrap_or(900) },
                "uninstall" => {
                    if interval.is_some() {
                        return Err("--interval is not valid for 'agent uninstall'".into());
                    }
                    AgentAction::Uninstall
                }
                _ => return Err("usage: diskledger agent install|uninstall|print [--interval SECS]".into()),
            };
            Ok(Command::Agent { action })
        }
        other => Err(format!("unknown command '{other}'. Try: diskledger help")),
    }
}

pub fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&raw) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("diskledger: {e}");
            1
        }
    };
    std::process::exit(code);
}

pub fn run(raw: &[String]) -> Res<()> {
    let cmd = parse_args(raw)?;
    match cmd {
        Command::Help => {
            println!("{HELP}");
            return Ok(());
        }
        Command::Version => {
            println!("diskledger {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Command::Size { paths, kb, json } => return cmd_size(&paths, kb, json),
        _ => {}
    }

    let store = Store::open()?;
    let sys = RealSystem;
    match cmd {
        Command::Init { dirs, force } => cmd_init(&store, &dirs, force),
        Command::Scan { dry_run, full, verbose } => cmd_scan(&sys, &store, dry_run, full, verbose),
        Command::Diff { since, min, no_scan, json } => cmd_diff(&sys, &store, since.as_deref(), min, no_scan, json),
        Command::Checkin { no_scan } => cmd_checkpoint(&sys, &store, no_scan, "checkin"),
        Command::Checkpoint { name, no_scan } => cmd_checkpoint(&sys, &store, no_scan, &name),
        Command::Top { under, min, depth, no_scan, json } => cmd_top(&sys, &store, under.as_deref(), min, depth, no_scan, json),
        Command::Candidates { min, no_scan, json } => cmd_candidates(&sys, &store, min, no_scan, json),
        Command::Status { json } => cmd_status(&store, json),
        Command::Doctor { json } => cmd_doctor(&store, json),
        Command::Agent { action } => cmd_agent(&store, action),
        Command::Help | Command::Version | Command::Size { .. } => unreachable!(),
    }
}

fn cmd_init(store: &Store, dirs: &[String], force: bool) -> Res<()> {
    if store.config_path().exists() && !force {
        return Err(format!(
            "{} already exists. Edit it, or pass --force to replace it.",
            store.config_path().display()
        ));
    }
    let mut cfg = Config::default();
    for p in dirs {
        let c = store::canonical(&PathBuf::from(p))?;
        if !c.is_dir() {
            return Err(format!("{}: not a directory", c.display()));
        }
        // Fail early on volumes that do not support the call.
        kstat::query(&c).map_err(|e| {
            format!("{}: kernel dir-stats unavailable ({e}). Is this an APFS volume?", c.display())
        })?;
        cfg.roots.push(c);
    }
    cfg.validate()?;
    store.write_config(&cfg)?;
    println!("Wrote {}", store.config_path().display());
    println!("Nothing has been flagged yet. Preview with: diskledger scan --dry-run");
    Ok(())
}

/// Scan using `sys`, store the snapshot, prune old snapshots, and return the report.
/// A failed save is reported and ignored so read commands still work on a full disk.
pub fn fresh_scan_with(
    sys: &dyn System,
    store: &Store,
    cfg: &Config,
    flag: bool,
    verbose: bool,
    full: bool,
) -> Res<ScanReport> {
    // Flagging scans are exclusive; read-only scans may overlap.
    let _lock = if flag { Some(store.lock()?) } else { None };
    let prev = store.latest();
    // A full scan ignores the previous snapshot, so every directory is re-examined.
    let reuse_from = if full { None } else { prev.as_ref() };
    let mut rep = scan::run(sys, cfg, reuse_from, &ScanOpts { flag, verbose });
    // Keep timestamps strictly increasing so every snapshot has its own file.
    if let Some(p) = &prev {
        if rep.snapshot.ts <= p.ts {
            rep.snapshot.ts = p.ts + 1;
        }
    }
    if let Err(e) = store.save(&rep.snapshot) {
        eprintln!("diskledger: warning: {e} (results below are still valid)");
    }
    store.prune_at(cfg.keep_snapshots, rep.snapshot.ts);
    Ok(rep)
}

fn cmd_scan(sys: &dyn System, store: &Store, dry: bool, full: bool, verbose: bool) -> Res<()> {
    let cfg = store.config()?;
    let rep = fresh_scan_with(sys, store, &cfg, !dry, verbose, full)?;
    println!("{}", rep.summary());
    if dry {
        println!("Would flag {} directories:", rep.would_flag.len());
        for p in &rep.would_flag {
            let r = rep.snapshot.rows.get(p);
            println!(
                "  {:>9} {:>9} entries  {}",
                r.map_or("?".into(), |r| human(r.bytes)),
                r.map_or(0, |r| r.entries),
                p
            );
        }
    } else {
        for p in &rep.flagged_new {
            println!("  flagged {p}");
        }
    }
    for e in rep.errors.iter().take(20) {
        eprintln!("  error: {e}");
    }
    if rep.errors.len() > 20 {
        eprintln!("  ... and {} more errors", rep.errors.len() - 20);
    }
    if !dry {
        store.write_last_scan(&format!("{} {}\n", fmt_ts(rep.snapshot.ts), rep.summary()));
    }
    Ok(())
}

/// The snapshot to report on: the latest stored one with --no-scan, else a fresh read-only scan.
fn current(sys: &dyn System, store: &Store, no_scan: bool, json: bool) -> Res<Snapshot> {
    if no_scan {
        return store.latest().ok_or_else(|| "no snapshots yet. Run: diskledger scan".to_string());
    }
    let cfg = store.config()?;
    let rep = fresh_scan_with(sys, store, &cfg, false, false, false)?;
    if !json {
        eprintln!("scan: {}", rep.summary());
    }
    Ok(rep.snapshot)
}

pub fn resolve_since_at(store: &Store, since: Option<&str>, cur_ts: u64, now_ts: u64) -> Res<Snapshot> {
    let all = store.list();
    let before: Vec<u64> = all.iter().copied().filter(|t| *t < cur_ts).collect();
    let ts = match since {
        None => match store.checkpoint("checkin") {
            Some(ts) => ts,
            None => *before.last().ok_or(
                "no earlier snapshot to compare with. Run 'diskledger checkin' now and 'diskledger diff' later.",
            )?,
        },
        Some(s) => {
            if let Some(ts) = store.checkpoint(s) {
                ts
            } else if let Some(d) = parse_duration(s) {
                let want = now_ts.saturating_sub(d);
                // Newest snapshot at least that old, else the oldest we have.
                *before
                    .iter()
                    .rev()
                    .find(|t| **t <= want)
                    .or(before.first())
                    .ok_or("no earlier snapshot to compare with")?
            } else if let Ok(ts) = s.parse::<u64>() {
                ts
            } else {
                return Err(format!("'{s}' is not a checkpoint name, an age like 2h, or a snapshot timestamp"));
            }
        }
    };
    store.load(ts)
}

fn cmd_diff(sys: &dyn System, store: &Store, since: Option<&str>, min: u64, no_scan: bool, json: bool) -> Res<()> {
    let cur = current(sys, store, no_scan, json)?;
    let base = resolve_since_at(store, since, cur.ts, sys.now())?;
    report::diff(&base, &cur, min, json);
    Ok(())
}

fn cmd_checkpoint(sys: &dyn System, store: &Store, no_scan: bool, name: &str) -> Res<()> {
    let cur = current(sys, store, no_scan, false)?;
    if !store.list().contains(&cur.ts) {
        return Err("could not store the snapshot, so no checkpoint was saved".into());
    }
    store.set_checkpoint(name, cur.ts)?;
    println!("Checkpoint '{name}' set at {} (free space {})", fmt_ts(cur.ts), human(cur.free));
    Ok(())
}

fn cmd_top(
    sys: &dyn System,
    store: &Store,
    under: Option<&str>,
    min: u64,
    depth: usize,
    no_scan: bool,
    json: bool,
) -> Res<()> {
    let cur = current(sys, store, no_scan, json)?;
    let under = match under {
        Some(p) => {
            let resolved = if no_scan {
                normalize_path_str(p)
            } else {
                normalize_path_str(&store::canonical(&PathBuf::from(p))?.to_string_lossy())
            };
            Some(resolved)
        }
        None => None,
    };
    if let Some(u) = &under {
        if !cur.rows.contains_key(u) {
            return Err(format!(
                "{u} is not in the snapshot (outside the monitored folders, or smaller than the recording minimum). Use: diskledger size {u}"
            ));
        }
    }
    report::top(&cur, min, depth, under.as_deref(), sys.now(), json);
    Ok(())
}

fn cmd_candidates(sys: &dyn System, store: &Store, min: u64, no_scan: bool, json: bool) -> Res<()> {
    let cur = current(sys, store, no_scan, json)?;
    report::candidates(store, &cur, min, json);
    Ok(())
}

fn cmd_status(store: &Store, json: bool) -> Res<()> {
    let cfg = store.config()?;
    let (free, total) = kstat::volume_space(&cfg.roots[0]).map_err(|e| e.to_string())?;
    let snaps = store.list();
    let latest = store.latest();
    let flagged = latest.as_ref().map_or(0, |s| s.rows.values().filter(|r| r.flagged).count());
    let cps = store.checkpoints();
    if json {
        let roots: Vec<String> = cfg
            .roots
            .iter()
            .map(|r| {
                let k = r.to_string_lossy();
                let row = latest.as_ref().and_then(|s| s.rows.get(k.as_ref()));
                format!(
                    "{{\"path\":{},\"bytes\":{},\"flagged\":{}}}",
                    json_str(&k),
                    row.map_or(0, |r| r.bytes),
                    row.map_or(false, |r| r.flagged)
                )
            })
            .collect();
        let cpj: Vec<String> = cps.iter().map(|(n, ts)| format!("{{\"name\":{},\"ts\":{}}}", json_str(n), ts)).collect();
        println!(
            "{{\"free\":{},\"total\":{},\"snapshots\":{},\"latest_ts\":{},\"flagged_dirs\":{},\"roots\":[{}],\"checkpoints\":[{}]}}",
            free,
            total,
            snaps.len(),
            latest.as_ref().map_or(0, |s| s.ts),
            flagged,
            roots.join(","),
            cpj.join(",")
        );
        return Ok(());
    }
    println!(
        "Free space: {} of {} ({:.0}% used)",
        human(free),
        human(total),
        100.0 * (1.0 - free as f64 / total.max(1) as f64)
    );
    println!("State: {}", store.home.display());
    match &latest {
        Some(s) => println!(
            "Snapshots: {} (latest {}, {} ago); {} directories recorded, {} with an instant kernel record",
            snaps.len(),
            fmt_ts(s.ts),
            fmt_age(now().saturating_sub(s.ts)),
            s.rows.len(),
            flagged
        ),
        None => println!("Snapshots: none yet. Run: diskledger scan"),
    }
    if let Some(l) = store.last_scan() {
        println!("Last flagging scan: {}", l.trim());
    }
    println!("Monitored directories:");
    for r in &cfg.roots {
        let k = r.to_string_lossy();
        match latest.as_ref().and_then(|s| s.rows.get(k.as_ref())) {
            Some(row) => println!(
                "  {:>9}  {}{}",
                human(row.bytes),
                k,
                if row.flagged { " *" } else { "  (not flagged)" }
            ),
            None => println!("  {:>9}  {}  (not scanned yet)", "?", k),
        }
    }
    if !cps.is_empty() {
        println!("Checkpoints:");
        for (n, ts) in cps {
            println!("  {n}  {} ({} ago)", fmt_ts(ts), fmt_age(now().saturating_sub(ts)));
        }
    }
    Ok(())
}

fn cmd_doctor(store: &Store, json: bool) -> Res<()> {
    let roots = store.config().map(|c| c.roots).unwrap_or_default();
    let rep = kstat::run_doctor(&roots);
    let mut out = io::stdout().lock();
    let _ = if json { rep.render_json(&mut out) } else { rep.render_human(&mut out) };
    if rep.ok {
        Ok(())
    } else {
        Err("APFS kernel verification failed".into())
    }
}

/// Size of arbitrary paths, straight from the kernel. Needs no config and no snapshot.
fn cmd_size(paths: &[String], kb: bool, json: bool) -> Res<()> {
    let mut items = Vec::new();
    let mut failed = false;
    if !json && !kb {
        println!("{:>9} {:>9}  PATH   (* = instant kernel record)", "SIZE", "ENTRIES");
    }
    for p in paths {
        let path = PathBuf::from(p);
        let start = std::time::Instant::now();
        let res: Result<(u64, u64, bool), String> = match std::fs::symlink_metadata(&path) {
            Err(e) => Err(e.to_string()),
            Ok(m) if m.is_dir() => kstat::query(&path).map(|s| (s.bytes, s.entries, s.has_record)).map_err(|e| e.to_string()),
            Ok(m) => {
                use std::os::unix::fs::MetadataExt;
                Ok((m.blocks() * 512, 0, false))
            }
        };
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        match res {
            Ok((bytes, entries, instant)) => {
                if json {
                    items.push(format!(
                        "{{\"path\":{},\"bytes\":{},\"entries\":{},\"instant\":{},\"ms\":{:.1}}}",
                        json_str(p), bytes, entries, instant, ms
                    ));
                } else if kb {
                    println!("{}\t{}", bytes.div_ceil(1024), p);
                } else {
                    println!("{:>9} {:>9}  {}{}", human(bytes), entries, p, if instant { " *" } else { "" });
                }
            }
            Err(e) => {
                failed = true;
                if json {
                    items.push(format!("{{\"path\":{},\"error\":{}}}", json_str(p), json_str(&e)));
                } else {
                    eprintln!("diskledger: {p}: {e}");
                }
            }
        }
    }
    if json {
        println!("[{}]", items.join(","));
    }
    if failed {
        Err("some paths could not be measured".into())
    } else {
        Ok(())
    }
}

const AGENT_LABEL: &str = "com.diskledger.scan";

fn cmd_agent(store: &Store, action: AgentAction) -> Res<()> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set")?;
    let plist_path = PathBuf::from(&home).join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist"));
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}");
    match action {
        AgentAction::Print { interval } | AgentAction::Install { interval } => {
            let is_print = matches!(action, AgentAction::Print { .. });
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let env = match std::env::var("DISKLEDGER_HOME") {
                Ok(h) => format!(
                    "  <key>EnvironmentVariables</key>\n  <dict><key>DISKLEDGER_HOME</key><string>{}</string></dict>\n",
                    xml(&h)
                ),
                Err(_) => String::new(),
            };
            let plist = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key><string>{AGENT_LABEL}</string>\n\
  <key>ProgramArguments</key>\n  <array><string>{}</string><string>scan</string></array>\n\
  <key>StartInterval</key><integer>{interval}</integer>\n\
  <key>RunAtLoad</key><true/>\n\
  <key>ProcessType</key><string>Background</string>\n\
  <key>LowPriorityIO</key><true/>\n\
  <key>Nice</key><integer>10</integer>\n\
{env}\
  <key>StandardOutPath</key><string>/dev/null</string>\n\
  <key>StandardErrorPath</key><string>/dev/null</string>\n\
</dict>\n</plist>\n",
                xml(&exe.to_string_lossy())
            );
            if is_print {
                print!("{plist}");
                return Ok(());
            }
            store.config()?; // Refuse to install a job that cannot run.
            std::fs::write(&plist_path, plist).map_err(|e| format!("write {}: {e}", plist_path.display()))?;
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("{target}/{AGENT_LABEL}")])
                .output();
            let out = std::process::Command::new("launchctl")
                .args(["bootstrap", &target])
                .arg(&plist_path)
                .output()
                .map_err(|e| format!("launchctl: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "launchctl bootstrap failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            println!(
                "Installed {}. 'diskledger scan' now runs every {interval}s and at login.",
                plist_path.display()
            );
            println!("It runs {} — reinstall if you move the binary.", exe.display());
            Ok(())
        }
        AgentAction::Uninstall => {
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("{target}/{AGENT_LABEL}")])
                .output();
            match std::fs::remove_file(&plist_path) {
                Ok(()) => println!("Removed {}", plist_path.display()),
                Err(_) => println!("No agent installed at {}", plist_path.display()),
            }
            Ok(())
        }
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
