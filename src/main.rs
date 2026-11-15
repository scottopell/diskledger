mod kstat;
mod report;
mod scan;
mod store;
mod util;

use scan::{ScanOpts, ScanReport};
use std::collections::HashMap;
use std::path::PathBuf;
use store::{Config, Snapshot, Store};
use util::{fmt_age, fmt_ts, human, json_str, now, parse_duration, parse_size, Res};

const HELP: &str = "\
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
  diskledger top [dir]              Largest directories in the latest snapshot
  diskledger candidates             Regenerable-looking directories (target, node_modules, ...)
  diskledger status                 Free space, roots, snapshots, checkpoints
  diskledger agent install|uninstall|print [--interval SECS]
                                    Run 'scan' periodically via launchd (default 900s)
  diskledger --version

OPTIONS
  --json            Machine-readable output (diff, top, candidates, status)
  --min SIZE        Smallest size or change to show, e.g. 100M, 2G
  --depth N         Levels to show in 'top' (default 4)
  --no-scan         Use the latest stored snapshot; do not look at the disk
  -v, --verbose     Print each directory as it is flagged

Only 'scan' flags directories. Flagging cannot be undone and walks the directory once.
State lives in $DISKLEDGER_HOME (default ~/.diskledger).";

struct Args {
    pos: Vec<String>,
    flags: HashMap<String, String>,
}

impl Args {
    fn parse(raw: Vec<String>) -> Res<Args> {
        const VALUED: &[&str] = &["--since", "--min", "--depth", "--interval"];
        let mut a = Args { pos: Vec::new(), flags: HashMap::new() };
        let mut it = raw.into_iter();
        while let Some(x) = it.next() {
            if VALUED.contains(&x.as_str()) {
                let v = it.next().ok_or(format!("{x} needs a value"))?;
                a.flags.insert(x, v);
            } else if x == "-v" {
                a.flags.insert("--verbose".into(), String::new());
            } else if x.starts_with("--") {
                a.flags.insert(x, String::new());
            } else {
                a.pos.push(x);
            }
        }
        Ok(a)
    }
    fn has(&self, f: &str) -> bool {
        self.flags.contains_key(f)
    }
    fn get(&self, f: &str) -> Option<&str> {
        self.flags.get(f).map(|s| s.as_str())
    }
    fn size(&self, f: &str, default: u64) -> Res<u64> {
        self.get(f).map_or(Ok(default), parse_size)
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(raw) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("diskledger: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run(mut raw: Vec<String>) -> Res<()> {
    if raw.is_empty() || raw[0] == "help" || raw[0] == "--help" || raw[0] == "-h" {
        println!("{HELP}");
        return Ok(());
    }
    if raw[0] == "--version" || raw[0] == "-V" || raw[0] == "version" {
        println!("diskledger {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let cmd = raw.remove(0);
    let args = Args::parse(raw)?;
    let store = Store::open()?;
    match cmd.as_str() {
        "init" => cmd_init(&store, &args),
        "scan" => cmd_scan(&store, &args),
        "diff" => cmd_diff(&store, &args),
        "checkin" => cmd_checkpoint(&store, &args, "checkin"),
        "checkpoint" => {
            let name = args.pos.first().ok_or("usage: diskledger checkpoint <name>")?.clone();
            cmd_checkpoint(&store, &args, &name)
        }
        "top" => cmd_top(&store, &args),
        "candidates" => cmd_candidates(&store, &args),
        "status" => cmd_status(&store, &args),
        "agent" => cmd_agent(&store, &args),
        other => Err(format!("unknown command '{other}'. Try: diskledger help")),
    }
}

fn cmd_init(store: &Store, args: &Args) -> Res<()> {
    if args.pos.is_empty() {
        return Err("usage: diskledger init <dir>...".into());
    }
    if store.config_path().exists() && !args.has("--force") {
        return Err(format!("{} already exists. Edit it, or pass --force to replace it.", store.config_path().display()));
    }
    let mut cfg = Config::default();
    for p in &args.pos {
        let c = store::canonical(&PathBuf::from(p))?;
        if !c.is_dir() {
            return Err(format!("{}: not a directory", c.display()));
        }
        // Fail early on volumes that do not support the call.
        kstat::query(&c).map_err(|e| format!("{}: kernel dir-stats unavailable ({e}). Is this an APFS volume?", c.display()))?;
        cfg.roots.push(c);
    }
    store.write_config(&cfg)?;
    println!("Wrote {}", store.config_path().display());
    println!("Nothing has been flagged yet. Preview with: diskledger scan --dry-run");
    Ok(())
}

/// Scan the disk, store the snapshot, and return it. A failed save is reported and ignored.
fn fresh_scan(store: &Store, cfg: &Config, flag: bool, verbose: bool, full: bool) -> Res<ScanReport> {
    // Flagging scans are exclusive; read-only scans may overlap.
    let _lock = if flag { Some(store.lock()?) } else { None };
    let prev = store.latest();
    // A full scan ignores the previous snapshot, so every directory is re-examined.
    let reuse_from = if full { None } else { prev.as_ref() };
    let mut rep = scan::run(cfg, reuse_from, &ScanOpts { flag, verbose });
    // Keep timestamps strictly increasing so every snapshot has its own file.
    if let Some(p) = &prev {
        if rep.snapshot.ts <= p.ts {
            rep.snapshot.ts = p.ts + 1;
        }
    }
    if let Err(e) = store.save(&rep.snapshot) {
        eprintln!("diskledger: warning: {e} (results below are still valid)");
    }
    store.prune(cfg.keep_snapshots);
    Ok(rep)
}

fn cmd_scan(store: &Store, args: &Args) -> Res<()> {
    let cfg = store.config()?;
    let dry = args.has("--dry-run");
    let rep = fresh_scan(store, &cfg, !dry, args.has("--verbose"), args.has("--full"))?;
    println!("{}", rep.summary());
    if dry {
        println!("Would flag {} directories:", rep.would_flag.len());
        for p in &rep.would_flag {
            let r = rep.snapshot.rows.get(p);
            println!("  {:>9} {:>9} entries  {}", r.map_or("?".into(), |r| human(r.bytes)), r.map_or(0, |r| r.entries), p);
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
fn current(store: &Store, args: &Args) -> Res<Snapshot> {
    if args.has("--no-scan") {
        return store.latest().ok_or_else(|| "no snapshots yet. Run: diskledger scan".to_string());
    }
    let cfg = store.config()?;
    let rep = fresh_scan(store, &cfg, false, false, false)?;
    if !args.has("--json") {
        eprintln!("scan: {}", rep.summary());
    }
    Ok(rep.snapshot)
}

fn resolve_since(store: &Store, since: Option<&str>, cur_ts: u64) -> Res<Snapshot> {
    let all = store.list();
    let before: Vec<u64> = all.iter().copied().filter(|t| *t < cur_ts).collect();
    let ts = match since {
        None => match store.checkpoint("checkin") {
            Some(ts) => ts,
            None => *before.last().ok_or("no earlier snapshot to compare with. Run 'diskledger checkin' now and 'diskledger diff' later.")?,
        },
        Some(s) => {
            if let Some(ts) = store.checkpoint(s) {
                ts
            } else if let Some(d) = parse_duration(s) {
                let want = now().saturating_sub(d);
                // Newest snapshot at least that old, else the oldest we have.
                *before.iter().rev().find(|t| **t <= want).or(before.first()).ok_or("no earlier snapshot to compare with")?
            } else if let Ok(ts) = s.parse::<u64>() {
                ts
            } else {
                return Err(format!("'{s}' is not a checkpoint name, an age like 2h, or a snapshot timestamp"));
            }
        }
    };
    store.load(ts)
}

fn cmd_diff(store: &Store, args: &Args) -> Res<()> {
    let min = args.size("--min", 100 << 20)?;
    let cur = current(store, args)?;
    let base = resolve_since(store, args.get("--since"), cur.ts)?;
    report::diff(&base, &cur, min, args.has("--json"));
    Ok(())
}

fn cmd_checkpoint(store: &Store, args: &Args, name: &str) -> Res<()> {
    let cur = current(store, args)?;
    if !store.list().contains(&cur.ts) {
        return Err("could not store the snapshot, so no checkpoint was saved".into());
    }
    store.set_checkpoint(name, cur.ts)?;
    println!("Checkpoint '{name}' set at {} (free space {})", fmt_ts(cur.ts), human(cur.free));
    Ok(())
}

fn cmd_top(store: &Store, args: &Args) -> Res<()> {
    let min = args.size("--min", 1 << 30)?;
    let depth: usize = args.get("--depth").map_or(Ok(4), |d| d.parse().map_err(|_| "--depth needs a number".to_string()))?;
    let cur = current(store, args)?;
    let under = match args.pos.first() {
        Some(p) => Some(store::canonical(&PathBuf::from(p))?.to_string_lossy().into_owned()),
        None => None,
    };
    report::top(&cur, min, depth, under.as_deref(), args.has("--json"));
    Ok(())
}

fn cmd_candidates(store: &Store, args: &Args) -> Res<()> {
    let min = args.size("--min", 500 << 20)?;
    let cur = current(store, args)?;
    report::candidates(store, &cur, min, args.has("--json"));
    Ok(())
}

fn cmd_status(store: &Store, args: &Args) -> Res<()> {
    let cfg = store.config()?;
    let (free, total) = kstat::volume_space(&cfg.roots[0]).map_err(|e| e.to_string())?;
    let snaps = store.list();
    let latest = store.latest();
    let flagged = latest.as_ref().map_or(0, |s| s.rows.values().filter(|r| r.flagged).count());
    let cps = store.checkpoints();
    if args.has("--json") {
        let roots: Vec<String> = cfg
            .roots
            .iter()
            .map(|r| {
                let k = r.to_string_lossy();
                let row = latest.as_ref().and_then(|s| s.rows.get(k.as_ref()));
                format!("{{\"path\":{},\"bytes\":{},\"flagged\":{}}}", json_str(&k), row.map_or(0, |r| r.bytes), row.map_or(false, |r| r.flagged))
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
    println!("Free space: {} of {} ({:.0}% used)", human(free), human(total), 100.0 * (1.0 - free as f64 / total.max(1) as f64));
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
            Some(row) => println!("  {:>9}  {}{}", human(row.bytes), k, if row.flagged { " *" } else { "  (not flagged)" }),
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

const AGENT_LABEL: &str = "com.diskledger.scan";

fn cmd_agent(store: &Store, args: &Args) -> Res<()> {
    let action = args.pos.first().map(|s| s.as_str()).unwrap_or("");
    let home = std::env::var("HOME").map_err(|_| "HOME not set")?;
    let plist_path = PathBuf::from(&home).join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist"));
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}");
    match action {
        "print" | "install" => {
            let interval: u64 = args.get("--interval").map_or(Ok(900), |s| s.parse().map_err(|_| "--interval needs seconds".to_string()))?;
            if interval < 60 {
                return Err("--interval must be at least 60 seconds".into());
            }
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
            if action == "print" {
                print!("{plist}");
                return Ok(());
            }
            store.config()?; // Refuse to install a job that cannot run.
            std::fs::write(&plist_path, plist).map_err(|e| format!("write {}: {e}", plist_path.display()))?;
            let _ = std::process::Command::new("launchctl").args(["bootout", &format!("{target}/{AGENT_LABEL}")]).output();
            let out = std::process::Command::new("launchctl")
                .args(["bootstrap", &target])
                .arg(&plist_path)
                .output()
                .map_err(|e| format!("launchctl: {e}"))?;
            if !out.status.success() {
                return Err(format!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            println!("Installed {}. 'diskledger scan' now runs every {interval}s and at login.", plist_path.display());
            println!("It runs {} — reinstall if you move the binary.", exe.display());
            Ok(())
        }
        "uninstall" => {
            let _ = std::process::Command::new("launchctl").args(["bootout", &format!("{target}/{AGENT_LABEL}")]).output();
            match std::fs::remove_file(&plist_path) {
                Ok(()) => println!("Removed {}", plist_path.display()),
                Err(_) => println!("No agent installed at {}", plist_path.display()),
            }
            Ok(())
        }
        _ => Err("usage: diskledger agent install|uninstall|print [--interval SECS]".into()),
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
