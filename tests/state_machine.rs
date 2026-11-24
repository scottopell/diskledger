//! Deterministic state-machine property tests driving diskledger's real
//! scan, store, prune, checkpoint, diff, top, and candidates logic against an
//! in-memory APFS filesystem simulator (`FakeFs`) and virtual clock.

use diskledger::cli::{
    add_roots_with, current_with, fresh_scan_with, remove_roots_with, resolve_since_at, ScanPolicy,
};
use diskledger::kstat::{DirStat, VolumeInfo};
use diskledger::report::{
    compute_candidates, compute_diff, compute_size_children, compute_top, DiffStatus,
    MonitoredState, SizeChildrenOpts,
};
use diskledger::scan::{self, ScanOpts};
use diskledger::store::{Config, Snapshot, Store};
use diskledger::sys::{DirListing, DirectEntry, System};
use diskledger::util::{is_same_or_under, is_under, normalize_path_str, parent_of};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

struct TempHome(PathBuf);

impl TempHome {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "diskledger-proptest-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).expect("create temp home");
        TempHome(p)
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, Default)]
struct OpCounts {
    queries: u64,
    walks: u64,
    walked_entries: u64,
    flags: u64,
    list_dirs: u64,
    has_git: u64,
    volume_space: u64,
    dev_checks: u64,
}

impl OpCounts {
    fn total(&self) -> u64 {
        self.queries + self.flags + self.list_dirs + self.has_git + self.volume_space + self.dev_checks
    }
}

#[derive(Debug, Clone)]
struct FakeDir {
    dev: u64,
    flagged: bool,
    gen: u64,
    unreadable: bool,
}

#[derive(Debug, Default)]
struct FakeState {
    dirs: BTreeMap<String, FakeDir>,
    files: BTreeMap<String, u64>,
    symlinks: BTreeMap<String, String>,
}

/// Deterministic in-memory APFS simulator modeling monotonic dir-stats flags,
/// recursive generation counters, symlinks, cross-device mounts, unreadable dirs,
/// outside-volume free-space shifts, and a virtual Unix clock.
struct FakeFs {
    state: Mutex<FakeState>,
    outside_used: AtomicU64,
    other_volumes_used: AtomicU64,
    purgeable: AtomicU64,
    apfs_snapshots: AtomicU64,
    volume_total: u64,
    clock: AtomicU64,
    ops: Mutex<OpCounts>,
}

impl FakeFs {
    fn new(start_ts: u64) -> Self {
        let mut dirs = BTreeMap::new();
        dirs.insert("/".into(), FakeDir { dev: 1, flagged: false, gen: 0, unreadable: false });
        FakeFs {
            state: Mutex::new(FakeState {
                dirs,
                files: BTreeMap::new(),
                symlinks: BTreeMap::new(),
            }),
            outside_used: AtomicU64::new(10 << 30),
            other_volumes_used: AtomicU64::new(5 << 30),
            purgeable: AtomicU64::new(1 << 30),
            apfs_snapshots: AtomicU64::new(0),
            volume_total: 200 << 30,
            clock: AtomicU64::new(start_ts),
            ops: Mutex::new(OpCounts::default()),
        }
    }

    fn ops_total(&self) -> u64 {
        self.ops.lock().unwrap().total()
    }

    fn ops_snapshot(&self) -> OpCounts {
        self.ops.lock().unwrap().clone()
    }

    fn advance_time(&self, dt: u64) {
        let cur = self.clock.load(Ordering::Relaxed);
        self.clock.store(cur.saturating_add(dt), Ordering::Relaxed);
    }

    fn bump_ancestors_locked(dirs: &mut BTreeMap<String, FakeDir>, path: &str, dev: u64) {
        let mut cur = parent_of(path);
        while let Some(anc) = cur {
            if let Some(d) = dirs.get_mut(anc) {
                if d.dev != dev {
                    break;
                }
                if d.flagged {
                    d.gen += 1;
                }
            }
            cur = parent_of(anc);
        }
    }

    fn mkdir(&self, path: &str, dev: u64) {
        let p = normalize_path_str(path);
        let mut st = self.state.lock().unwrap();
        if st.dirs.contains_key(&p) {
            return;
        }
        st.dirs.insert(
            p.clone(),
            FakeDir { dev, flagged: false, gen: 0, unreadable: false },
        );
        Self::bump_ancestors_locked(&mut st.dirs, &p, dev);
    }

    fn set_unreadable(&self, path: &str, unreadable: bool) {
        if let Some(d) = self.state.lock().unwrap().dirs.get_mut(path) {
            d.unreadable = unreadable;
        }
    }

    fn write_file(&self, path: &str, bytes: u64) {
        let p = normalize_path_str(path);
        let Some(par) = parent_of(&p) else { return };
        let mut st = self.state.lock().unwrap();
        let Some(dev) = st.dirs.get(par).map(|d| d.dev) else { return };
        if st.dirs.contains_key(&p) || st.symlinks.contains_key(&p) {
            return;
        }
        st.files.insert(p.clone(), bytes);
        Self::bump_ancestors_locked(&mut st.dirs, &p, dev);
    }

    fn delete_file(&self, path: &str) {
        let p = normalize_path_str(path);
        let mut st = self.state.lock().unwrap();
        if st.files.remove(&p).is_some() {
            if let Some(par) = parent_of(&p) {
                if let Some(dev) = st.dirs.get(par).map(|d| d.dev) {
                    Self::bump_ancestors_locked(&mut st.dirs, &p, dev);
                }
            }
        }
    }

    fn delete_dir_tree(&self, path: &str) {
        let p = normalize_path_str(path);
        let mut st = self.state.lock().unwrap();
        let Some(dev) = st.dirs.get(&p).map(|d| d.dev) else { return };
        st.dirs.retain(|k, _| !is_same_or_under(k, &p));
        st.files.retain(|k, _| !is_under(k, &p));
        st.symlinks.retain(|k, _| !is_under(k, &p));
        Self::bump_ancestors_locked(&mut st.dirs, &p, dev);
    }

    fn add_symlink(&self, path: &str, target: &str) {
        let p = normalize_path_str(path);
        let Some(par) = parent_of(&p) else { return };
        let mut st = self.state.lock().unwrap();
        let Some(dev) = st.dirs.get(par).map(|d| d.dev) else { return };
        if st.dirs.contains_key(&p) || st.files.contains_key(&p) {
            return;
        }
        st.symlinks.insert(p.clone(), target.to_string());
        Self::bump_ancestors_locked(&mut st.dirs, &p, dev);
    }

    fn adjust_outside(&self, delta: i64) {
        let cur = self.outside_used.load(Ordering::Relaxed);
        let next = if delta >= 0 {
            cur.saturating_add(delta as u64).min(50 << 30)
        } else {
            cur.saturating_sub(delta.unsigned_abs())
        };
        self.outside_used.store(next, Ordering::Relaxed);
    }

    fn adjust_apfs_container(&self, other_vol_delta: i64, purgeable_delta: i64, snap_delta: i8) {
        let ov = self.other_volumes_used.load(Ordering::Relaxed);
        let next_ov = if other_vol_delta >= 0 {
            ov.saturating_add(other_vol_delta as u64).min(40 << 30)
        } else {
            ov.saturating_sub(other_vol_delta.unsigned_abs())
        };
        self.other_volumes_used.store(next_ov, Ordering::Relaxed);

        let pu = self.purgeable.load(Ordering::Relaxed);
        let next_pu = if purgeable_delta >= 0 {
            pu.saturating_add(purgeable_delta as u64).min(20 << 30)
        } else {
            pu.saturating_sub(purgeable_delta.unsigned_abs())
        };
        self.purgeable.store(next_pu, Ordering::Relaxed);

        let sn = self.apfs_snapshots.load(Ordering::Relaxed);
        let next_sn = if snap_delta >= 0 {
            sn.saturating_add(snap_delta as u64).min(10)
        } else {
            sn.saturating_sub(snap_delta.unsigned_abs() as u64)
        };
        self.apfs_snapshots.store(next_sn, Ordering::Relaxed);
    }

    /// Ground-truth same-volume stats for `path`, matching APFS semantics.
    fn ground_truth_stats(&self, path: &str) -> Option<DirStat> {
        let p = normalize_path_str(path);
        let st = self.state.lock().unwrap();
        let root_dir = st.dirs.get(&p)?;
        let dev = root_dir.dev;

        let mut reachable = BTreeSet::new();
        reachable.insert(p.clone());
        for (dpath, dinfo) in st.dirs.iter() {
            if is_under(dpath, &p) && dinfo.dev == dev {
                let mut ok = true;
                let mut anc = parent_of(dpath);
                while let Some(a) = anc {
                    if a == p {
                        break;
                    }
                    if st.dirs.get(a).map_or(true, |info| info.dev != dev) {
                        ok = false;
                        break;
                    }
                    anc = parent_of(a);
                }
                if ok {
                    reachable.insert(dpath.clone());
                }
            }
        }

        let mut entries: u64 = (reachable.len() - 1) as u64;
        let mut bytes: u64 = 0;
        for (fpath, fbytes) in st.files.iter() {
            if let Some(par) = parent_of(fpath) {
                if reachable.contains(par) {
                    entries += 1;
                    bytes += *fbytes;
                }
            }
        }
        // Real APFS dir-stats `descendants` counts regular files and directories, not symlinks.
        Some(DirStat {
            has_record: root_dir.flagged,
            gen: if root_dir.flagged { root_dir.gen } else { 0 },
            entries,
            bytes,
        })
    }

    fn ground_truth_bytes(&self, path: &str) -> u64 {
        self.ground_truth_stats(path).map_or(0, |s| s.bytes)
    }

    fn all_flagged(&self) -> BTreeSet<String> {
        self.state
            .lock()
            .unwrap()
            .dirs
            .iter()
            .filter(|(_, d)| d.flagged)
            .map(|(k, _)| k.clone())
            .collect()
    }

    fn dir_has_direct_git(&self, dir: &str) -> bool {
        let git_path = if dir == "/" { "/.git".to_string() } else { format!("{dir}/.git") };
        let st = self.state.lock().unwrap();
        st.files.contains_key(&git_path) || st.dirs.contains_key(&git_path)
    }

    fn vol1_used(&self) -> u64 {
        let st = self.state.lock().unwrap();
        let file_sum: u64 = st
            .files
            .iter()
            .filter(|(p, _)| parent_of(p).and_then(|par| st.dirs.get(par)).map_or(false, |d| d.dev == 1))
            .map(|(_, b)| *b)
            .sum();
        file_sum.saturating_add(self.outside_used.load(Ordering::Relaxed))
    }

    fn vol1_container_used(&self) -> u64 {
        self.vol1_used()
            .saturating_add(self.other_volumes_used.load(Ordering::Relaxed))
    }
}

impl System for FakeFs {
    fn query(&self, path: &Path) -> io::Result<DirStat> {
        let p = normalize_path_str(&path.to_string_lossy());
        let st = self
            .ground_truth_stats(&p)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such directory"))?;
        let mut ops = self.ops.lock().unwrap();
        ops.queries += 1;
        if !st.has_record {
            ops.walks += 1;
            ops.walked_entries += st.entries;
        }
        Ok(st)
    }

    fn flag(&self, path: &Path) -> io::Result<()> {
        let p = normalize_path_str(&path.to_string_lossy());
        self.ops.lock().unwrap().flags += 1;
        let mut st = self.state.lock().unwrap();
        let d = st
            .dirs
            .get_mut(&p)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such directory"))?;
        d.flagged = true;
        if d.gen == 0 {
            d.gen = 1;
        }
        Ok(())
    }

    fn volume_space(&self, path: &Path) -> io::Result<(u64, u64)> {
        let p = normalize_path_str(&path.to_string_lossy());
        self.ops.lock().unwrap().volume_space += 1;
        let dev = self
            .state
            .lock()
            .unwrap()
            .dirs
            .get(&p)
            .map(|d| d.dev)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such path"))?;
        if dev == 1 {
            let used = self.vol1_container_used();
            Ok((self.volume_total.saturating_sub(used), self.volume_total))
        } else {
            Ok((50 << 30, 100 << 30))
        }
    }

    fn volume_info(&self, path: &Path) -> io::Result<VolumeInfo> {
        let p = normalize_path_str(&path.to_string_lossy());
        self.ops.lock().unwrap().volume_space += 1;
        let dev = self
            .state
            .lock()
            .unwrap()
            .dirs
            .get(&p)
            .map(|d| d.dev)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such path"))?;
        if dev == 1 {
            let vol_used = self.vol1_used();
            let container_used = vol_used.saturating_add(self.other_volumes_used.load(Ordering::Relaxed));
            let free = self.volume_total.saturating_sub(container_used);
            let sn = self.apfs_snapshots.load(Ordering::Relaxed);
            let snapshots = (0..sn).map(|i| format!("com.apple.TimeMachine.snap-{i}")).collect();
            Ok(VolumeInfo {
                free,
                total: self.volume_total,
                purgeable: Some(self.purgeable.load(Ordering::Relaxed)),
                volume_used: Some(vol_used),
                snapshots,
            })
        } else {
            Ok(VolumeInfo {
                free: 50 << 30,
                total: 100 << 30,
                purgeable: Some(0),
                volume_used: Some(50 << 30),
                snapshots: Vec::new(),
            })
        }
    }

    fn dir_dev(&self, path: &Path) -> io::Result<Option<u64>> {
        let p = normalize_path_str(&path.to_string_lossy());
        self.ops.lock().unwrap().dev_checks += 1;
        let st = self.state.lock().unwrap();
        if let Some(d) = st.dirs.get(&p) {
            Ok(Some(d.dev))
        } else if st.files.contains_key(&p) {
            Ok(None)
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "not found"))
        }
    }

    fn symlink_dev(&self, path: &Path) -> io::Result<u64> {
        let p = normalize_path_str(&path.to_string_lossy());
        self.ops.lock().unwrap().dev_checks += 1;
        let st = self.state.lock().unwrap();
        if let Some(d) = st.dirs.get(&p) {
            Ok(d.dev)
        } else if let Some(par) = parent_of(&p) {
            st.dirs
                .get(par)
                .map(|d| d.dev)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "not found"))
        }
    }

    fn list_dir(&self, path: &Path, max_children: usize) -> io::Result<DirListing> {
        let p = normalize_path_str(&path.to_string_lossy());
        self.ops.lock().unwrap().list_dirs += 1;
        let st = self.state.lock().unwrap();
        let d = st
            .dirs
            .get(&p)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such directory"))?;
        if d.unreadable {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"));
        }
        let git_path = if p == "/" { "/.git".to_string() } else { format!("{p}/.git") };
        let has_git = st.files.contains_key(&git_path) || st.dirs.contains_key(&git_path);
        let has_direct_files = st.files.keys().any(|k| parent_of(k) == Some(p.as_str()));
        let mut subdirs = Vec::new();
        for k in st.dirs.keys() {
            if parent_of(k) == Some(p.as_str()) {
                subdirs.push(PathBuf::from(k));
                if subdirs.len() > max_children {
                    return Ok(DirListing {
                        subdirs: Vec::new(),
                        too_many_children: true,
                        has_git,
                        has_direct_files,
                    });
                }
            }
        }
        Ok(DirListing { subdirs, too_many_children: false, has_git, has_direct_files })
    }

    fn list_entries(&self, path: &Path) -> io::Result<Vec<DirectEntry>> {
        let p = normalize_path_str(&path.to_string_lossy());
        self.ops.lock().unwrap().list_dirs += 1;
        let st = self.state.lock().unwrap();
        let d = st
            .dirs
            .get(&p)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such directory"))?;
        if d.unreadable {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"));
        }
        let parent_dev = d.dev;
        let mut out = Vec::new();
        for (k, info) in &st.dirs {
            if parent_of(k) == Some(p.as_str()) {
                out.push(DirectEntry {
                    path: PathBuf::from(k),
                    is_dir: true,
                    is_symlink: false,
                    dev: info.dev,
                    file_bytes: 0,
                });
            }
        }
        for (k, &bytes) in &st.files {
            if parent_of(k) == Some(p.as_str()) {
                out.push(DirectEntry {
                    path: PathBuf::from(k),
                    is_dir: false,
                    is_symlink: false,
                    dev: parent_dev,
                    file_bytes: bytes,
                });
            }
        }
        for k in st.symlinks.keys() {
            if parent_of(k) == Some(p.as_str()) {
                out.push(DirectEntry {
                    path: PathBuf::from(k),
                    is_dir: false,
                    is_symlink: true,
                    dev: parent_dev,
                    file_bytes: 0,
                });
            }
        }
        Ok(out)
    }

    fn has_git(&self, path: &Path) -> bool {
        let p = normalize_path_str(&path.to_string_lossy());
        self.ops.lock().unwrap().has_git += 1;
        self.dir_has_direct_git(&p)
    }

    fn now(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone)]
enum DirLabel {
    Normal(u8),
    Regenerable(usize),
    Weird(usize),
}

impl DirLabel {
    fn as_name(&self) -> String {
        const REGEN: &[&str] = &["target", "build", "node_modules", ".cache", "dist"];
        const WEIRD: &[&str] = &[
            "dir with spaces",
            "tab\tname",
            "quote\"name",
            "slash\\name",
            "cr\rname",
            "unicodé-🦀",
        ];
        match self {
            DirLabel::Normal(n) => format!("d{n}"),
            DirLabel::Regenerable(i) => REGEN[i % REGEN.len()].to_string(),
            DirLabel::Weird(i) => WEIRD[i % WEIRD.len()].to_string(),
        }
    }
}

#[derive(Debug, Clone)]
enum FileLabel {
    Normal(u8),
    GitMarker,
}

impl FileLabel {
    fn as_name(&self) -> String {
        match self {
            FileLabel::Normal(n) => format!("f{n}.bin"),
            FileLabel::GitMarker => ".git".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
enum Transition {
    WriteFile { dir_idx: usize, name: FileLabel, bytes: u64 },
    DeleteFile { file_idx: usize },
    CreateDir { parent_idx: usize, name: DirLabel },
    DeleteDir { dir_idx: usize },
    DeleteRootOnDisk { root_idx: usize },
    ToggleUnreadable { dir_idx: usize },
    AddSymlinkCycle { parent_idx: usize, target_idx: usize },
    AddCrossDevMount { parent_idx: usize, bytes: u64 },
    OutsideVolumeDelta { delta: i64 },
    ApfsContainerShift { other_vol_delta: i64, purgeable_delta: i64, snap_delta: i8 },
    AdvanceTime { dt: u64 },
    AddRoot { same_volume: bool },
    RemoveRoot { root_idx: usize },
    TuneConfig { record_min_bytes: u64, flag_min_entries: u64, max_depth: u32, keep_snapshots: usize },
    Scan { flag: bool, full: bool },
    Checkpoint { name: String },
    VerifyReports { min: u64, depth: usize },
    Prune,
}

fn dir_label_strategy() -> impl Strategy<Value = DirLabel> {
    prop_oneof![
        4 => (0u8..8).prop_map(DirLabel::Normal),
        3 => (0usize..5).prop_map(DirLabel::Regenerable),
        1 => (0usize..6).prop_map(DirLabel::Weird),
    ]
}

fn file_label_strategy() -> impl Strategy<Value = FileLabel> {
    prop_oneof![
        6 => (0u8..12).prop_map(FileLabel::Normal),
        1 => Just(FileLabel::GitMarker),
    ]
}

fn transition_strategy() -> impl Strategy<Value = Transition> {
    prop_oneof![
        5 => (any::<usize>(), file_label_strategy(), prop_oneof![Just(0u64), 1024u64..(8u64 << 20)])
            .prop_map(|(dir_idx, name, bytes)| Transition::WriteFile { dir_idx, name, bytes }),
        2 => any::<usize>().prop_map(|file_idx| Transition::DeleteFile { file_idx }),
        4 => (any::<usize>(), dir_label_strategy())
            .prop_map(|(parent_idx, name)| Transition::CreateDir { parent_idx, name }),
        2 => any::<usize>().prop_map(|dir_idx| Transition::DeleteDir { dir_idx }),
        1 => any::<usize>().prop_map(|root_idx| Transition::DeleteRootOnDisk { root_idx }),
        1 => any::<usize>().prop_map(|dir_idx| Transition::ToggleUnreadable { dir_idx }),
        1 => (any::<usize>(), any::<usize>())
            .prop_map(|(parent_idx, target_idx)| Transition::AddSymlinkCycle { parent_idx, target_idx }),
        1 => (any::<usize>(), 1u64..(4u64 << 20))
            .prop_map(|(parent_idx, bytes)| Transition::AddCrossDevMount { parent_idx, bytes }),
        2 => (-(4i64 << 20)..=(4i64 << 20))
            .prop_map(|delta| Transition::OutsideVolumeDelta { delta }),
        1 => (-(200i64 << 20)..=(200i64 << 20), -(200i64 << 20)..=(200i64 << 20), -1i8..=2i8)
            .prop_map(|(other_vol_delta, purgeable_delta, snap_delta)| {
                Transition::ApfsContainerShift { other_vol_delta, purgeable_delta, snap_delta }
            }),
        2 => prop_oneof![
            Just(0u64),
            1u64..3600,
            Just(86400u64),
            Just(86400u64 * 35),
        ].prop_map(|dt| Transition::AdvanceTime { dt }),
        1 => any::<bool>().prop_map(|same_volume| Transition::AddRoot { same_volume }),
        1 => any::<usize>().prop_map(|root_idx| Transition::RemoveRoot { root_idx }),
        1 => (
            prop_oneof![Just(0u64), Just(512u64 << 10), Just(2u64 << 20), Just(4u64 << 20)],
            prop_oneof![Just(2u64), Just(4u64), Just(8u64)],
            1u32..=6,
            1usize..=6,
        ).prop_map(|(record_min_bytes, flag_min_entries, max_depth, keep_snapshots)| {
            Transition::TuneConfig { record_min_bytes, flag_min_entries, max_depth, keep_snapshots }
        }),
        4 => (any::<bool>(), any::<bool>())
            .prop_map(|(flag, full)| Transition::Scan { flag, full }),
        2 => prop_oneof![Just("checkin".to_string()), Just("cp-a".to_string()), Just("cp-b".to_string())]
            .prop_map(|name| Transition::Checkpoint { name }),
        3 => (prop_oneof![Just(0u64), Just(256u64 << 10), Just(2u64 << 20)], 0usize..=5)
            .prop_map(|(min, depth)| Transition::VerifyReports { min, depth }),
        1 => Just(Transition::Prune),
    ]
}

fn assert_tree_closed(snap: &Snapshot) {
    let root_set: BTreeSet<&str> = snap.roots.iter().map(|s| s.as_str()).collect();
    for path in snap.rows.keys() {
        if root_set.contains(path.as_str()) {
            continue;
        }
        let par = parent_of(path).expect("non-root path must have a parent");
        assert!(
            snap.rows.contains_key(par),
            "tree closure violated: '{}' recorded without parent '{}'",
            path,
            par
        );
    }
}

fn assert_json_valid(bytes: &[u8], context: &str) -> serde_json::Value {
    let text = std::str::from_utf8(bytes).unwrap_or_else(|e| panic!("{context}: invalid utf-8: {e}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{context}: invalid JSON: {e}\n{text}"))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn apfs_ledger_state_machine(transitions in prop::collection::vec(transition_strategy(), 5..45)) {
        let home = TempHome::new("sm");
        let store = Store::open_at(home.0.clone());
        let fs = FakeFs::new(1_795_000_000);

        fs.mkdir("/vol", 1);
        fs.mkdir("/vol/r0", 1);
        fs.mkdir("/vol/r1", 1);

        let mut cfg = Config {
            roots: vec![PathBuf::from("/vol/r0"), PathBuf::from("/vol/r1")],
            flag_min_entries: 4,
            record_min_bytes: 1 << 20,
            max_depth: 5,
            max_children: 64,
            keep_snapshots: 5,
        };
        store.write_config(&cfg).expect("write initial config");

        let mut next_root_id = 2usize;
        let mut prev_flagged = fs.all_flagged();
        let mut thresholds_dirty = true;
        // Record ground-truth root byte totals at each saved snapshot timestamp for exact diff checks.
        let mut ground_truth_by_ts: BTreeMap<u64, BTreeMap<String, u64>> = BTreeMap::new();
        let mut last_scan_was_clean_for_fs = false;

        for tr in transitions {
            match tr {
                Transition::WriteFile { dir_idx, name, bytes } => {
                    let candidates: Vec<String> = fs
                        .state
                        .lock()
                        .unwrap()
                        .dirs
                        .iter()
                        .filter(|(k, d)| *k != "/" && *k != "/vol" && d.dev == 1)
                        .map(|(k, _)| k.clone())
                        .collect();
                    if !candidates.is_empty() {
                        let dir = &candidates[dir_idx % candidates.len()];
                        let fpath = format!("{dir}/{}", name.as_name());
                        fs.write_file(&fpath, bytes);
                        last_scan_was_clean_for_fs = false;
                    }
                }
                Transition::DeleteFile { file_idx } => {
                    let files: Vec<String> = fs.state.lock().unwrap().files.keys().cloned().collect();
                    if !files.is_empty() {
                        fs.delete_file(&files[file_idx % files.len()]);
                        last_scan_was_clean_for_fs = false;
                    }
                }
                Transition::CreateDir { parent_idx, name } => {
                    let parents: Vec<String> = fs
                        .state
                        .lock()
                        .unwrap()
                        .dirs
                        .iter()
                        .filter(|(k, d)| *k != "/" && *k != "/vol" && d.dev == 1)
                        .map(|(k, _)| k.clone())
                        .collect();
                    if !parents.is_empty() {
                        let par = &parents[parent_idx % parents.len()];
                        let child = format!("{par}/{}", name.as_name());
                        let is_file = fs.state.lock().unwrap().files.contains_key(&child);
                        if !is_file {
                            fs.mkdir(&child, 1);
                            last_scan_was_clean_for_fs = false;
                        }
                    }
                }
                Transition::DeleteDir { dir_idx } => {
                    let roots: BTreeSet<String> = cfg
                        .roots
                        .iter()
                        .map(|r| normalize_path_str(&r.to_string_lossy()))
                        .collect();
                    let removable: Vec<String> = fs
                        .state
                        .lock()
                        .unwrap()
                        .dirs
                        .keys()
                        .filter(|k| *k != "/" && *k != "/vol" && !roots.contains(*k))
                        .cloned()
                        .collect();
                    if !removable.is_empty() {
                        let victim = &removable[dir_idx % removable.len()];
                        fs.delete_dir_tree(victim);
                        prev_flagged.retain(|p| !is_same_or_under(p, victim));
                        last_scan_was_clean_for_fs = false;
                    }
                }
                Transition::DeleteRootOnDisk { root_idx } => {
                    // Delete a configured root from disk while leaving it in config (or recreate if gone),
                    // ensuring real [gone] differs cleanly from config root removal.
                    if cfg.roots.len() >= 2 {
                        let r = normalize_path_str(&cfg.roots[root_idx % cfg.roots.len()].to_string_lossy());
                        let (exists, existing_vol1, is_vol1) = {
                            let st = fs.state.lock().unwrap();
                            let exists = st.dirs.contains_key(&r);
                            let existing_vol1 = cfg
                                .roots
                                .iter()
                                .filter(|p| {
                                    let k = normalize_path_str(&p.to_string_lossy());
                                    st.dirs.get(&k).map_or(false, |d| d.dev == 1)
                                })
                                .count();
                            let is_vol1 = st.dirs.get(&r).map_or(false, |d| d.dev == 1);
                            (exists, existing_vol1, is_vol1)
                        };
                        if exists {
                            if !is_vol1 || existing_vol1 >= 2 {
                                fs.delete_dir_tree(&r);
                                prev_flagged.retain(|p| !is_same_or_under(p, &r));
                                last_scan_was_clean_for_fs = false;
                            }
                        } else {
                            fs.mkdir(&r, 1);
                            last_scan_was_clean_for_fs = false;
                        }
                    }
                }
                Transition::ToggleUnreadable { dir_idx } => {
                    let roots: BTreeSet<String> = cfg
                        .roots
                        .iter()
                        .map(|r| normalize_path_str(&r.to_string_lossy()))
                        .collect();
                    let dirs: Vec<(String, bool)> = fs
                        .state
                        .lock()
                        .unwrap()
                        .dirs
                        .iter()
                        .filter(|(k, _)| *k != "/" && *k != "/vol" && !roots.contains(*k))
                        .map(|(k, d)| (k.clone(), d.unreadable))
                        .collect();
                    if !dirs.is_empty() {
                        let (p, cur) = &dirs[dir_idx % dirs.len()];
                        fs.set_unreadable(p, !cur);
                        // Changing readability doesn't bump APFS gen, so treat as external state change.
                        thresholds_dirty = true;
                        last_scan_was_clean_for_fs = false;
                    }
                }
                Transition::AddSymlinkCycle { parent_idx, target_idx } => {
                    let dirs: Vec<String> = fs
                        .state
                        .lock()
                        .unwrap()
                        .dirs
                        .keys()
                        .filter(|k| *k != "/" && *k != "/vol")
                        .cloned()
                        .collect();
                    if !dirs.is_empty() {
                        let par = &dirs[parent_idx % dirs.len()];
                        let target = &dirs[target_idx % dirs.len()];
                        fs.add_symlink(&format!("{par}/loop_link"), target);
                        last_scan_was_clean_for_fs = false;
                    }
                }
                Transition::AddCrossDevMount { parent_idx, bytes } => {
                    let dirs: Vec<String> = fs
                        .state
                        .lock()
                        .unwrap()
                        .dirs
                        .iter()
                        .filter(|(k, d)| *k != "/" && *k != "/vol" && d.dev == 1)
                        .map(|(k, _)| k.clone())
                        .collect();
                    if !dirs.is_empty() {
                        let par = &dirs[parent_idx % dirs.len()];
                        let mnt = format!("{par}/ext_mount");
                        let occupied = {
                            let st = fs.state.lock().unwrap();
                            st.dirs.contains_key(&mnt) || st.files.contains_key(&mnt)
                        };
                        if !occupied {
                            fs.mkdir(&mnt, 2);
                            fs.write_file(&format!("{mnt}/huge.bin"), bytes);
                            last_scan_was_clean_for_fs = false;
                        }
                    }
                }
                Transition::OutsideVolumeDelta { delta } => {
                    fs.adjust_outside(delta);
                }
                Transition::ApfsContainerShift { other_vol_delta, purgeable_delta, snap_delta } => {
                    fs.adjust_apfs_container(other_vol_delta, purgeable_delta, snap_delta);
                }
                Transition::AdvanceTime { dt } => {
                    fs.advance_time(dt);
                }
                Transition::AddRoot { same_volume } => {
                    if cfg.roots.len() < 5 {
                        let rpath = format!("/vol/r{next_root_id}");
                        next_root_id += 1;
                        fs.mkdir(&rpath, if same_volume { 1 } else { 2 });
                        cfg.roots.push(PathBuf::from(&rpath));
                        store.write_config(&cfg).expect("write updated config");
                        thresholds_dirty = true;
                        last_scan_was_clean_for_fs = false;
                    }
                }
                Transition::RemoveRoot { root_idx } => {
                    if cfg.roots.len() >= 2 {
                        cfg.roots.remove(root_idx % cfg.roots.len());
                        store.write_config(&cfg).expect("write updated config");
                        thresholds_dirty = true;
                        last_scan_was_clean_for_fs = false;
                    }
                }
                Transition::TuneConfig { record_min_bytes, flag_min_entries, max_depth, keep_snapshots } => {
                    cfg.record_min_bytes = record_min_bytes;
                    cfg.flag_min_entries = flag_min_entries;
                    cfg.max_depth = max_depth;
                    cfg.keep_snapshots = keep_snapshots;
                    store.write_config(&cfg).expect("write tuned config");
                    thresholds_dirty = true;
                }
                Transition::Scan { flag, full } => {
                    let ops_before = fs.ops_snapshot();
                    let prev_latest = store.latest();

                    // Dry-run parity oracle: before a flagging scan, run a shadow dry-run
                    // and verify it predicts the exact set `scan` actually flags (when full
                    // or thresholds unchanged).
                    let predicted_flags: Option<BTreeSet<String>> = if flag && (full || !thresholds_dirty) {
                        let reuse = if full { None } else { prev_latest.as_ref() };
                        let dry = scan::run(&fs, &cfg, reuse, &ScanOpts { flag: false, verbose: false });
                        prop_assert_eq!(
                            fs.ops_snapshot().flags,
                            ops_before.flags,
                            "dry-run scan must NEVER call flag()"
                        );
                        Some(dry.would_flag.into_iter().collect())
                    } else {
                        None
                    };

                    // Incremental == Full parity oracle (also tests 1-thread vs parallel parity).
                    let full_oracle = if !flag && !full && !thresholds_dirty {
                        Some(scan::run_with_threads(&fs, &cfg, None, &ScanOpts { flag: false, verbose: false }, 1))
                    } else {
                        None
                    };

                    // Parallel vs single-threaded parity on every read-only scan.
                    let seq_oracle = if !flag {
                        let reuse = if full { None } else { prev_latest.as_ref() };
                        Some(scan::run_with_threads(&fs, &cfg, reuse, &ScanOpts { flag: false, verbose: false }, 1))
                    } else {
                        None
                    };

                    let ops_pre_real = fs.ops_snapshot();
                    let rep = fresh_scan_with(&fs, &store, &cfg, flag, false, full)
                        .expect("fresh_scan_with");
                    let ops_post_real = fs.ops_snapshot();

                    // Read-only scan must never call flag().
                    if !flag {
                        prop_assert_eq!(ops_post_real.flags, ops_pre_real.flags);
                    }

                    // Parallel scan must match single-threaded scan identically.
                    if let Some(seq) = seq_oracle {
                        prop_assert_eq!(&rep.snapshot.rows, &seq.snapshot.rows, "parallel vs 1-thread rows diverged");
                        prop_assert_eq!(&rep.snapshot.extra_git_repos, &seq.snapshot.extra_git_repos);
                        prop_assert_eq!(&rep.would_flag, &seq.would_flag, "parallel vs 1-thread would_flag diverged");
                        prop_assert_eq!(&rep.errors, &seq.errors, "parallel vs 1-thread errors diverged");
                    }

                    // Flagging is strictly monotonic for surviving directories (never unflagged).
                    let now_flagged = fs.all_flagged();
                    let surviving_prev: BTreeSet<String> = {
                        let st = fs.state.lock().unwrap();
                        prev_flagged
                            .iter()
                            .filter(|p| st.dirs.contains_key(*p))
                            .cloned()
                            .collect()
                    };
                    prop_assert!(
                        surviving_prev.is_subset(&now_flagged),
                        "flags must never disappear from surviving directories"
                    );
                    prev_flagged = now_flagged;

                    // Dry-run prediction matches actual flagged_new set.
                    if let Some(pred) = predicted_flags {
                        let actual: BTreeSet<String> = rep.flagged_new.iter().cloned().collect();
                        prop_assert_eq!(actual, pred, "dry-run would_flag != actual flagged_new");
                    }

                    // Incremental == full equivalence when thresholds unchanged.
                    if let Some(oracle) = full_oracle {
                        prop_assert_eq!(
                            &rep.snapshot.rows,
                            &oracle.snapshot.rows,
                            "incremental scan rows diverged from full scan"
                        );
                        prop_assert_eq!(
                            &rep.snapshot.extra_git_repos,
                            &oracle.snapshot.extra_git_repos,
                            "incremental extra_git_repos diverged from full scan"
                        );
                        let inc_would: BTreeSet<&String> = rep.would_flag.iter().collect();
                        let full_would: BTreeSet<&String> = oracle.would_flag.iter().collect();
                        prop_assert_eq!(
                            inc_would,
                            full_would,
                            "incremental would_flag diverged from full scan"
                        );
                    }

                    // Tree closure invariant ALWAYS holds, even right after TuneConfig!
                    assert_tree_closed(&rep.snapshot);

                    // Strictly increasing timestamp & lossless TSV roundtrip.
                    if let Some(prev) = &prev_latest {
                        prop_assert!(rep.snapshot.ts > prev.ts, "timestamps must strictly increase");
                    }
                    let reloaded = store.load(rep.snapshot.ts).expect("reload saved snapshot");
                    prop_assert_eq!(&reloaded, &rep.snapshot, "snapshot TSV roundtrip mismatch");

                    // Record ground-truth root byte sizes at this snapshot.
                    let mut root_truth = BTreeMap::new();
                    for r in &rep.snapshot.roots {
                        root_truth.insert(r.clone(), fs.ground_truth_bytes(r));
                    }
                    ground_truth_by_ts.insert(rep.snapshot.ts, root_truth);

                    // If we just ran a full scan (or kept thresholds clean), future incremental
                    // scans match full scans.
                    if full || prev_latest.is_none() {
                        thresholds_dirty = false;
                    }
                    // Verify reuse: an immediate second read-only scan with no changes should
                    // reuse every already-flagged root without walking it.
                    if !thresholds_dirty && !rep.snapshot.rows.is_empty() {
                        let second = scan::run(
                            &fs,
                            &cfg,
                            Some(&rep.snapshot),
                            &ScanOpts { flag: false, verbose: false },
                        );
                        let all_roots_flagged = cfg.roots.iter().all(|r| {
                            let k = normalize_path_str(&r.to_string_lossy());
                            rep.snapshot.rows.get(&k).map_or(false, |row| row.flagged)
                        });
                        if all_roots_flagged {
                            prop_assert_eq!(second.walked, 0, "all-flagged unchanged tree must incur 0 walks");
                            prop_assert_eq!(second.reused, rep.snapshot.rows.len() as u64);
                        }
                    }
                    last_scan_was_clean_for_fs = !thresholds_dirty;
                }
                Transition::Checkpoint { name } => {
                    if let Some(latest) = store.latest() {
                        store.set_checkpoint(&name, latest.ts).expect("set checkpoint");
                        prop_assert_eq!(store.checkpoint(&name), Some(latest.ts));
                    }
                }
                Transition::VerifyReports { min, depth } => {
                    let snaps = store.list();
                    if snaps.is_empty() {
                        continue;
                    }
                    let cur = store.latest().expect("latest snapshot");

                    // --no-scan purity: ZERO calls to FakeFs allowed during report computation & rendering!
                    let ops_before = fs.ops_total();

                    // 1. Diff against oldest and against previous (if available), plus resolve_since_at.
                    for &base_ts in snaps.iter().filter(|t| **t < cur.ts).take(2) {
                        let base = store.load(base_ts).expect("load base snapshot");
                        let diff = compute_diff(&base, &cur, min);

                        // Accounting conservation (both 2-way and 3-way APFS container breakdown).
                        if base.single_volume && cur.single_volume {
                            let out = diff.outside_delta.expect("single_volume diff must have outside_delta");
                            prop_assert_eq!(
                                diff.monitored_delta + out,
                                -diff.free_delta,
                                "accounting conservation violated"
                            );
                            if let (Some(unmon), Some(other)) =
                                (diff.unmonitored_volume_delta, diff.other_volumes_delta)
                            {
                                prop_assert_eq!(
                                    diff.monitored_delta + unmon + other,
                                    -diff.free_delta,
                                    "3-way APFS container conservation violated"
                                );
                                prop_assert_eq!(unmon + other, out);
                            }
                        } else {
                            prop_assert_eq!(
                                diff.outside_delta,
                                None,
                                "multi-volume diff must omit outside_delta"
                            );
                            prop_assert_eq!(diff.unmonitored_volume_delta, None);
                            prop_assert_eq!(diff.other_volumes_delta, None);
                        }

                        // Exact ground-truth monitored_delta over roots present in BOTH configs.
                        let base_roots: BTreeSet<&str> = base.roots.iter().map(|s| s.as_str()).collect();
                        let cur_roots: BTreeSet<&str> = cur.roots.iter().map(|s| s.as_str()).collect();
                        let mut expected_monitored: i64 = 0;
                        for r in base_roots.intersection(&cur_roots) {
                            let b = ground_truth_by_ts[&base.ts][*r] as i64;
                            let c = ground_truth_by_ts[&cur.ts][*r] as i64;
                            expected_monitored += c - b;
                        }
                        prop_assert_eq!(
                            diff.monitored_delta,
                            expected_monitored,
                            "monitored_delta != exact sum of covered root changes"
                        );

                        // Removed roots must be in no_longer_monitored and NEVER reported as [gone].
                        for r in base_roots.difference(&cur_roots) {
                            prop_assert!(
                                diff.no_longer_monitored.iter().any(|(p, _)| p == r),
                                "removed root '{r}' missing from no_longer_monitored"
                            );
                            prop_assert!(
                                !diff.rows.contains_key(*r),
                                "removed root '{r}' leaked into covered diff rows"
                            );
                        }
                        // Added roots must be in not_in_baseline and NEVER reported as [new].
                        for r in cur_roots.difference(&base_roots) {
                            prop_assert!(
                                diff.not_in_baseline.iter().any(|(p, _)| p == r),
                                "added root '{r}' missing from not_in_baseline"
                            );
                            prop_assert!(
                                !diff.rows.contains_key(*r),
                                "added root '{r}' leaked into covered diff rows"
                            );
                        }

                        // JSON validity + human/JSON parity.
                        let mut jbuf = Vec::new();
                        diff.render_json(&mut jbuf).expect("diff json");
                        let jval = assert_json_valid(&jbuf, "diff");
                        prop_assert_eq!(
                            jval["changes"].as_array().unwrap().len(),
                            diff.changes.len()
                        );

                        let mut hbuf = Vec::new();
                        diff.render_human(&mut hbuf).expect("diff human");
                        let human_text = String::from_utf8(hbuf).expect("utf8");
                        // Every significant change in JSON must appear in human output (base name or full path).
                        for row in &diff.changes {
                            let leaf = diskledger::util::base_name(&row.path);
                            prop_assert!(
                                human_text.contains(leaf),
                                "significant path '{}' missing from human diff output:\n{}",
                                row.path,
                                human_text
                            );
                        }
                    }

                    // Also exercise resolve_since_at with duration and timestamp when an earlier snapshot exists.
                    if snaps.len() >= 2 {
                        let resolved = resolve_since_at(&store, Some("1h"), cur.ts, fs.now())
                            .expect("resolve_since_at 1h");
                        prop_assert!(resolved.ts < cur.ts);
                    }

                    // 2. Top: JSON validity and --depth honored identically in human & JSON.
                    let top = compute_top(&cur, min, depth, None);
                    for n in &top.dirs {
                        prop_assert!(n.depth <= depth, "top --json exceeded --depth");
                        prop_assert!(n.bytes >= min);
                    }
                    let mut top_j = Vec::new();
                    top.render_json(&mut top_j).expect("top json");
                    let top_jv = assert_json_valid(&top_j, "top");
                    prop_assert_eq!(top_jv["dirs"].as_array().unwrap().len(), top.dirs.len());
                    let mut top_h = Vec::new();
                    top.render_human(&mut top_h, fs.now()).expect("top human");

                    // Also exercise compute_top scoped under an existing recorded directory.
                    if let Some(sub) = cur.rows.keys().next() {
                        let sub_top = compute_top(&cur, min, depth, Some(sub));
                        for n in &sub_top.dirs {
                            prop_assert!(is_same_or_under(&n.path, sub));
                            prop_assert!(n.depth <= depth);
                        }
                    }

                    // 3. Candidates: JSON validity, no nested candidates, and no git-holding candidate.
                    let cand = compute_candidates(&store, &cur, min);
                    let mut cand_j = Vec::new();
                    cand.render_json(&mut cand_j).expect("candidates json");
                    let cand_jv = assert_json_valid(&cand_j, "candidates");
                    prop_assert_eq!(
                        cand_jv["candidates"].as_array().unwrap().len(),
                        cand.candidates.len()
                    );
                    let mut cand_h = Vec::new();
                    cand.render_human(&mut cand_h).expect("candidates human");

                    for i in 0..cand.candidates.len() {
                        let ci = &cand.candidates[i];
                        prop_assert!(ci.unchanged_since <= cur.ts);
                        prop_assert_eq!(ci.unchanged_secs, cur.ts - ci.unchanged_since);
                        for j in 0..cand.candidates.len() {
                            if i != j {
                                prop_assert!(
                                    !is_under(&ci.path, &cand.candidates[j].path),
                                    "candidate '{}' nested inside candidate '{}'",
                                    ci.path,
                                    cand.candidates[j].path
                                );
                            }
                        }
                        // If cur reflects current filesystem cleanly, verify no recorded or direct-child
                        // git repo was misclassified as a candidate!
                        if last_scan_was_clean_for_fs {
                            prop_assert!(
                                !fs.dir_has_direct_git(&ci.path),
                                "candidate '{}' directly holds .git!",
                                ci.path
                            );
                            for (k, r) in cur.subtree(&ci.path) {
                                prop_assert!(
                                    !r.has_git,
                                    "candidate '{}' has recorded git repo '{}'!",
                                    ci.path,
                                    k
                                );
                            }
                            // Also verify no direct child on FakeFs holds .git (even if < record_min_bytes)
                            // unless ci.path was unreadable or at max_depth.
                            let ci_depth = cfg
                                .roots
                                .iter()
                                .find_map(|r| {
                                    let rk = normalize_path_str(&r.to_string_lossy());
                                    if ci.path == rk {
                                        Some(0u32)
                                    } else if is_under(&ci.path, &rk) {
                                        Some(ci.path[rk.len()..].matches('/').count() as u32)
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(cfg.max_depth);
                            let (ci_unreadable, child_dirs) = {
                                let st = fs.state.lock().unwrap();
                                let unr = st.dirs.get(&ci.path).map_or(true, |d| d.unreadable);
                                let kids: Vec<String> = st
                                    .dirs
                                    .iter()
                                    .filter(|(k, d)| d.dev == 1 && parent_of(k) == Some(ci.path.as_str()))
                                    .map(|(k, _)| k.clone())
                                    .collect();
                                (unr, kids)
                            };
                            let ci_bytes = cur.rows.get(&ci.path).map_or(0, |r| r.bytes);
                            if !ci_unreadable && ci_depth < cfg.max_depth && ci_bytes >= cfg.record_min_bytes {
                                if child_dirs.len() <= cfg.max_children {
                                    for cdir in child_dirs {
                                        if diskledger::report::REGENERABLE.contains(&diskledger::util::base_name(&ci.path)) {
                                            prop_assert!(
                                                !fs.dir_has_direct_git(&cdir),
                                                "candidate '{}' has direct-child git repo '{}'!",
                                                ci.path,
                                                cdir
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Strict --no-scan purity check: ZERO filesystem operations occurred.
                    prop_assert_eq!(
                        fs.ops_total(),
                        ops_before,
                        "report computation/rendering under --no-scan must not touch System"
                    );
                }
                Transition::Prune => {
                    let before = store.list();
                    let cps = store.checkpoints();
                    let now_ts = fs.now();
                    store.prune_at(cfg.keep_snapshots, now_ts);
                    let after = store.list();
                    let after_set: BTreeSet<u64> = after.iter().copied().collect();

                    // Every checkpoint-pinned snapshot must survive pruning.
                    for (name, ts) in &cps {
                        if before.contains(ts) {
                            prop_assert!(
                                after_set.contains(ts),
                                "prune deleted checkpoint '{}' snapshot {}",
                                name,
                                ts
                            );
                            store.load(*ts).expect("pinned snapshot must remain readable");
                        }
                    }

                    // Newest `keep_snapshots` must survive.
                    let keep = cfg.keep_snapshots.min(before.len());
                    for ts in &before[before.len() - keep..] {
                        prop_assert!(after_set.contains(ts), "prune deleted recent snapshot {ts}");
                    }

                    // Among older non-pinned snapshots: at most 1 per UTC day within 30 days,
                    // and 0 older than 30 days.
                    let pinned: BTreeSet<u64> = cps.into_iter().map(|(_, ts)| ts).collect();
                    let cutoff = now_ts.saturating_sub(30 * 86400);
                    let mut seen_days = BTreeSet::new();
                    for &ts in &before[..before.len() - keep] {
                        if after_set.contains(&ts) && !pinned.contains(&ts) {
                            prop_assert!(ts >= cutoff, "non-pinned snapshot older than 30d survived prune");
                            prop_assert!(
                                seen_days.insert(ts / 86400),
                                "more than one non-pinned daily snapshot survived for day {}",
                                ts / 86400
                            );
                        }
                    }
                }
            }
        }
    }

    /// Dedicated property for cancelling sibling changes (+delta and -delta under the same parent):
    /// human diff must still display both significant children even when the parent's net delta is 0.
    #[test]
    fn diff_shows_cancelling_opposing_children(delta_bytes in (2u64 << 20)..(20u64 << 20)) {
        let home = TempHome::new("cancel");
        let store = Store::open_at(home.0.clone());
        let fs = FakeFs::new(1_795_000_000);
        fs.mkdir("/root", 1);
        fs.mkdir("/root/a", 1);
        fs.mkdir("/root/b", 1);
        fs.write_file("/root/a/data.bin", delta_bytes);
        fs.write_file("/root/b/data.bin", 1 << 20);

        let cfg = Config {
            roots: vec![PathBuf::from("/root")],
            flag_min_entries: 100,
            record_min_bytes: 512 << 10,
            max_depth: 6,
            max_children: 64,
            keep_snapshots: 10,
        };
        let s1 = fresh_scan_with(&fs, &store, &cfg, false, false, true).unwrap().snapshot;

        // Swap sizes: /root/a shrinks by (delta - 1M), /root/b grows by (delta - 1M) -> net root delta = 0!
        fs.write_file("/root/a/data.bin", 1 << 20);
        fs.write_file("/root/b/data.bin", delta_bytes);
        fs.advance_time(60);
        let s2 = fresh_scan_with(&fs, &store, &cfg, false, false, true).unwrap().snapshot;

        let min = 1 << 20;
        let diff = compute_diff(&s1, &s2, min);
        prop_assert_eq!(diff.monitored_delta, 0);
        prop_assert_eq!(diff.outside_delta, Some(0));
        prop_assert!(diff.changes.iter().any(|r| r.path == "/root/a"));
        prop_assert!(diff.changes.iter().any(|r| r.path == "/root/b"));
        prop_assert!(diff.changes.iter().all(|r| r.status == DiffStatus::Changed));

        let mut hbuf = Vec::new();
        diff.render_human(&mut hbuf).unwrap();
        let text = String::from_utf8(hbuf).unwrap();
        prop_assert!(text.contains("/root"), "parent root missing:\n{text}");
        prop_assert!(text.contains("  a"), "shrinking child 'a' missing:\n{text}");
        prop_assert!(text.contains("  b"), "growing child 'b' missing:\n{text}");
        prop_assert!(!text.contains("nothing changed"), "falsely claimed nothing changed:\n{text}");
    }

    /// Structural walk-bound invariant: when a dirty flagged parent's remaining
    /// uncovered bytes (< record_min_bytes) and entries (< flag_min_entries) are
    /// exhausted by its flagged children, scan must incur ZERO kernel walks on
    /// any number of unflagged sibling directories, while still preserving exact
    /// snapshot parity with a full scan (including `.git` detection under regen dirs).
    #[test]
    fn budget_pruning_skips_all_unflagged_siblings(
        n_siblings in 5usize..40,
        flagged_bytes in (100u64 << 20)..(500u64 << 20),
        sibling_bytes in 1u64..(128u64 << 10),
        parent_is_regen in any::<bool>(),
        git_sibling_idx in 0usize..40,
    ) {
        let fs = FakeFs::new(1_795_000_000);
        let root = if parent_is_regen { "/vol/build" } else { "/vol/proj" };
        fs.mkdir("/vol", 1);
        fs.mkdir(root, 1);
        let big = format!("{root}/big_flagged");
        fs.mkdir(&big, 1);
        fs.write_file(&format!("{big}/blob.bin"), flagged_bytes);
        // Flag both big child and root.
        fs.flag(Path::new(&big)).unwrap();
        fs.flag(Path::new(root)).unwrap();

        let git_idx = git_sibling_idx % n_siblings;
        for i in 0..n_siblings {
            let sib = format!("{root}/small_{i:02}");
            fs.mkdir(&sib, 1);
            fs.write_file(&format!("{sib}/tiny.txt"), sibling_bytes);
            if i == git_idx {
                fs.write_file(&format!("{sib}/.git"), 64);
            }
        }

        let cfg = Config {
            roots: vec![PathBuf::from(root)],
            flag_min_entries: 5_000,
            record_min_bytes: 64 << 20,
            max_depth: 6,
            max_children: 200,
            keep_snapshots: 5,
        };

        // Initial snapshot records `root` and `big_flagged` as flagged.
        let prev = scan::run(&fs, &cfg, None, &ScanOpts { flag: false, verbose: false }).snapshot;
        // Mutate inside `big_flagged` so `root` and `big_flagged` both become dirty!
        fs.write_file(&format!("{big}/delta.bin"), 4096);

        let before = fs.ops_snapshot();
        let rep = scan::run(&fs, &cfg, Some(&prev), &ScanOpts { flag: false, verbose: false });
        let after = fs.ops_snapshot();

        prop_assert_eq!(
            after.walks - before.walks,
            0,
            "dirty flagged parent with <record_min_bytes and <flag_min_entries remaining must do 0 walks on {} unflagged siblings",
            n_siblings
        );
        prop_assert_eq!(after.walked_entries - before.walked_entries, 0);
        prop_assert_eq!(
            after.queries - before.queries,
            2,
            "should only query root and big_flagged"
        );

        // Must still match a full scan on rows and extra_git_repos.
        let full = scan::run(&fs, &cfg, None, &ScanOpts { flag: false, verbose: false });
        prop_assert_eq!(rep.snapshot.rows, full.snapshot.rows);
        prop_assert_eq!(&rep.snapshot.extra_git_repos, &full.snapshot.extra_git_repos);
        if parent_is_regen {
            let expected_git = format!("{root}/small_{git_idx:02}");
            prop_assert!(rep.snapshot.extra_git_repos.contains(&expected_git));
        }
    }

    /// Structural walk-bound invariant: a chain of nested unflagged zero-direct-file
    /// container directories sitting above a flagged subtree must incur ZERO kernel
    /// walks on an incremental read-only scan, while matching a full scan identically.
    #[test]
    fn bottom_up_container_chain_avoids_redundant_parent_walks(
        chain_len in 1usize..=5,
        leaf_bytes in (80u64 << 20)..(200u64 << 20),
        delta_bytes in 1u64..(4u64 << 20),
    ) {
        let fs = FakeFs::new(1_795_000_000);
        fs.mkdir("/vol", 1);
        fs.mkdir("/vol/root", 1);
        let mut cur = "/vol/root".to_string();
        for i in 0..chain_len {
            cur = format!("{cur}/c{i}");
            fs.mkdir(&cur, 1);
        }
        let leaf = format!("{cur}/leaf_flagged");
        fs.mkdir(&leaf, 1);
        fs.write_file(&format!("{leaf}/blob.bin"), leaf_bytes);
        fs.flag(Path::new(&leaf)).unwrap();
        fs.flag(Path::new("/vol/root")).unwrap();

        let cfg = Config {
            roots: vec![PathBuf::from("/vol/root")],
            flag_min_entries: 5_000,
            record_min_bytes: 64 << 20,
            max_depth: 10,
            max_children: 64,
            keep_snapshots: 5,
        };

        let prev = scan::run(&fs, &cfg, None, &ScanOpts { flag: false, verbose: false }).snapshot;
        // Mutate inside the flagged leaf so `/vol/root` and `leaf` are dirty.
        fs.write_file(&format!("{leaf}/delta.bin"), delta_bytes);

        let before = fs.ops_snapshot();
        let rep = scan::run(&fs, &cfg, Some(&prev), &ScanOpts { flag: false, verbose: false });
        let after = fs.ops_snapshot();

        prop_assert_eq!(
            after.walks - before.walks,
            0,
            "chain of {} unflagged zero-file container dirs must incur 0 kernel walks",
            chain_len
        );
        prop_assert_eq!(after.walked_entries - before.walked_entries, 0);

        let full = scan::run(&fs, &cfg, None, &ScanOpts { flag: false, verbose: false });
        prop_assert_eq!(rep.snapshot.rows, full.snapshot.rows);
        prop_assert_eq!(rep.would_flag, full.would_flag);
    }

    /// Structural walk-bound invariant: when a flagged parent has many LARGE
    /// (>= record_min_bytes) unflagged siblings (so remaining-budget pruning cannot
    /// skip them) and mutations happen strictly inside a flagged descendant subtree,
    /// frontier gen-delta accounting must incur ZERO kernel walks on all unflagged
    /// siblings and intermediate containers, while matching a full scan identically.
    #[test]
    fn frontier_gen_accounting_skips_large_unflagged_siblings(
        n_large_siblings in 3usize..20,
        sibling_bytes in (80u64 << 20)..(200u64 << 20),
        delta_bytes in 1u64..(10u64 << 20),
        flag_mode in any::<bool>(),
    ) {
        let fs = FakeFs::new(1_795_000_000);
        fs.mkdir("/vol", 1);
        fs.mkdir("/vol/root", 1);
        fs.mkdir("/vol/root/mid", 1);
        fs.mkdir("/vol/root/mid/active_flagged", 1);
        fs.write_file("/vol/root/mid/active_flagged/base.bin", 100 << 20);
        fs.flag(Path::new("/vol/root/mid/active_flagged")).unwrap();
        fs.flag(Path::new("/vol/root")).unwrap();

        for i in 0..n_large_siblings {
            let sib = format!("/vol/root/large_unflagged_{i:02}");
            fs.mkdir(&sib, 1);
            fs.write_file(&format!("{sib}/big.bin"), sibling_bytes);
        }

        let cfg = Config {
            roots: vec![PathBuf::from("/vol/root")],
            flag_min_entries: 5_000,
            record_min_bytes: 64 << 20,
            max_depth: 6,
            max_children: 100,
            keep_snapshots: 5,
        };

        let prev = scan::run(&fs, &cfg, None, &ScanOpts { flag: false, verbose: false }).snapshot;
        // Mutate strictly inside the flagged descendant!
        fs.write_file("/vol/root/mid/active_flagged/edit.bin", delta_bytes);

        let before = fs.ops_snapshot();
        let rep = scan::run(&fs, &cfg, Some(&prev), &ScanOpts { flag: flag_mode, verbose: false });
        let after = fs.ops_snapshot();

        prop_assert_eq!(
            after.walks - before.walks,
            0,
            "mutations inside flagged descendant must incur 0 kernel walks on {} large unflagged siblings",
            n_large_siblings
        );
        prop_assert_eq!(after.walked_entries - before.walked_entries, 0);

        let full = scan::run(&fs, &cfg, None, &ScanOpts { flag: false, verbose: false });
        prop_assert_eq!(rep.snapshot.rows, full.snapshot.rows);
        prop_assert_eq!(rep.would_flag, full.would_flag);
    }

    /// `--max-age` reuses the latest snapshot with zero filesystem calls when
    /// `now - latest.ts <= max_age`, and triggers a fresh read-only scan once older.
    #[test]
    fn max_age_reuses_fresh_snapshot_with_zero_fs_ops(
        max_age in 10u64..3600,
        within_frac in 0u64..=100,
        over_delta in 1u64..3600,
    ) {
        let home = TempHome::new("maxage");
        let store = Store::open_at(home.0.clone());
        let fs = FakeFs::new(1_795_000_000);
        fs.mkdir("/vol", 1);
        fs.mkdir("/vol/r", 1);
        fs.write_file("/vol/r/f.bin", 80 << 20);
        let cfg = Config {
            roots: vec![PathBuf::from("/vol/r")],
            flag_min_entries: 5_000,
            record_min_bytes: 64 << 20,
            max_depth: 6,
            max_children: 64,
            keep_snapshots: 5,
        };
        store.write_config(&cfg).unwrap();

        let s1 = fresh_scan_with(&fs, &store, &cfg, false, false, false).unwrap().snapshot;

        // Advance time within `max_age` -> MUST reuse s1 with ZERO fs calls.
        let dt_in = (max_age * within_frac) / 100;
        fs.advance_time(dt_in);
        let ops_before = fs.ops_total();
        let reused = current_with(&fs, &store, ScanPolicy::MaxAge(max_age), true).unwrap();
        prop_assert_eq!(fs.ops_total(), ops_before, "--max-age hit must touch 0 fs ops");
        prop_assert_eq!(reused.ts, s1.ts);

        // Advance past `max_age` -> MUST run a fresh read-only scan.
        fs.advance_time(max_age - dt_in + over_delta);
        let ops_before2 = fs.ops_total();
        let fresh = current_with(&fs, &store, ScanPolicy::MaxAge(max_age), true).unwrap();
        prop_assert!(fs.ops_total() > ops_before2, "--max-age miss must run a fresh scan");
        prop_assert!(fresh.ts > s1.ts);
    }

    /// `size --children` / `--unmonitored` invariants:
    /// - Strictly sorted descending by shown bytes
    /// - Monitored roots are skipped with ZERO walks under `--unmonitored`
    /// - Partial parents (like `~/Library` containing `~/Library/Caches`) subtract
    ///   monitored descendants accurately
    /// - `sum(entries) + omitted_bytes == total_bytes`, never crosses volumes, never calls flag()
    #[test]
    fn size_children_sorting_partial_subtraction_and_zero_walk_skip(
        mon_bytes in (10u64 << 20)..(100u64 << 20),
        partial_mon_bytes in (10u64 << 20)..(80u64 << 20),
        partial_unmon_bytes in (5u64 << 20)..(80u64 << 20),
        unmon_sizes in prop::collection::vec(0u64..(60u64 << 20), 1..=8),
        file_bytes in 0u64..(20u64 << 20),
        min_bytes in prop_oneof![Just(0u64), Just(10u64 << 20), Just(30u64 << 20)],
        limit in prop_oneof![Just(None), (1usize..=5).prop_map(Some)],
    ) {
        let fs = FakeFs::new(1_795_000_000);
        fs.mkdir("/home", 1);
        // 1. Fully monitored root (intentionally unflagged so any query would increment walks!)
        fs.mkdir("/home/dev", 1);
        fs.write_file("/home/dev/blob.bin", mon_bytes);
        // 2. Partial parent: /home/Library has monitored /home/Library/Caches + unmonitored AppSupport
        fs.mkdir("/home/Library", 1);
        fs.mkdir("/home/Library/Caches", 1);
        fs.write_file("/home/Library/Caches/c.bin", partial_mon_bytes);
        fs.flag(Path::new("/home/Library/Caches")).unwrap();
        fs.mkdir("/home/Library/AppSupport", 1);
        fs.write_file("/home/Library/AppSupport/a.bin", partial_unmon_bytes);
        // 3. Pure unmonitored sibling dirs
        for (i, &sz) in unmon_sizes.iter().enumerate() {
            let d = format!("/home/unmon_{i}");
            fs.mkdir(&d, 1);
            fs.write_file(&format!("{d}/f.bin"), sz);
        }
        // 4. Direct file + cross-dev mount (must be ignored)
        fs.write_file("/home/direct.bin", file_bytes);
        fs.mkdir("/home/external_vol", 2);
        fs.write_file("/home/external_vol/huge.bin", 999 << 20);

        let roots = vec!["/home/dev".to_string(), "/home/Library/Caches".to_string()];

        // Full children mode
        let ops0 = fs.ops_snapshot();
        let all = compute_size_children(
            &fs,
            Path::new("/home"),
            &roots,
            &SizeChildrenOpts { min_bytes, limit, unmonitored_only: false },
        )
        .unwrap();
        let ops1 = fs.ops_snapshot();
        prop_assert_eq!(ops1.flags, ops0.flags, "size -c must never flag");
        prop_assert!(
            all.entries.iter().all(|e| e.path != "/home/external_vol"),
            "size -c must not cross volumes"
        );
        let kept_sum: u64 = all.entries.iter().map(|e| e.bytes).sum();
        prop_assert_eq!(kept_sum + all.omitted_bytes, all.total_bytes);
        for w in all.entries.windows(2) {
            prop_assert!(w[0].bytes >= w[1].bytes, "not sorted descending");
        }
        let mut jbuf = Vec::new();
        all.render_json(&mut jbuf).unwrap();
        assert_json_valid(&jbuf, "size -c json");
        let mut hbuf = Vec::new();
        all.render_human(&mut hbuf).unwrap();

        // Unmonitored-only mode: MUST skip /home/dev without walking it, and report
        // exact unmonitored_bytes for /home/Library!
        let unmon_all = compute_size_children(
            &fs,
            Path::new("/home"),
            &roots,
            &SizeChildrenOpts { min_bytes: 0, limit: None, unmonitored_only: true },
        )
        .unwrap();
        prop_assert!(unmon_all.entries.iter().all(|e| e.path != "/home/dev"));
        let lib = unmon_all
            .entries
            .iter()
            .find(|e| e.path == "/home/Library")
            .expect("/home/Library must appear");
        prop_assert_eq!(lib.monitored, MonitoredState::Partial);
        prop_assert_eq!(lib.bytes, partial_mon_bytes + partial_unmon_bytes);
        prop_assert_eq!(lib.unmonitored_bytes, partial_unmon_bytes);

        let expected_unmon_total: u64 =
            partial_unmon_bytes + unmon_sizes.iter().sum::<u64>() + file_bytes;
        prop_assert_eq!(unmon_all.total_bytes, expected_unmon_total);

        // Verify covered dir `/home/dev` is skipped with 0 walks when every other dir is flagged.
        fs.flag(Path::new("/home/Library")).unwrap();
        for i in 0..unmon_sizes.len() {
            fs.flag(Path::new(&format!("/home/unmon_{i}"))).unwrap();
        }
        let pre = fs.ops_snapshot();
        let _ = compute_size_children(
            &fs,
            Path::new("/home"),
            &roots,
            &SizeChildrenOpts { min_bytes, limit, unmonitored_only: true },
        )
        .unwrap();
        let post = fs.ops_snapshot();
        prop_assert_eq!(
            post.walks - pre.walks,
            0,
            "size -c -u must not walk covered root /home/dev"
        );
    }

    /// `root add` / `root remove` invariants:
    /// - Adding valid disjoint roots persists atomically and never calls `flag()`
    /// - Adding duplicates, nested overlaps, or non-directories fails leaving config untouched
    /// - Removing existing or even already-deleted roots works cleanly; removing the last root fails
    #[test]
    fn root_add_remove_invariants(
        extra_names in prop::collection::btree_set("[a-z]{2,8}", 2..=5),
        delete_before_remove in any::<bool>(),
    ) {
        let home = TempHome::new("roots");
        let store = Store::open_at(home.0.clone());
        let fs = FakeFs::new(1_795_000_000);
        fs.mkdir("/vol", 1);
        fs.mkdir("/vol/base", 1);
        let init_cfg = Config {
            roots: vec![PathBuf::from("/vol/base")],
            ..Config::default()
        };
        store.write_config(&init_cfg).unwrap();

        let paths: Vec<PathBuf> = extra_names
            .iter()
            .map(|n| {
                let p = format!("/vol/{n}");
                fs.mkdir(&p, 1);
                PathBuf::from(p)
            })
            .collect();

        let ops0 = fs.ops_snapshot();
        let added = add_roots_with(&fs, &store, &paths).unwrap();
        let ops1 = fs.ops_snapshot();
        prop_assert_eq!(ops1.flags, ops0.flags, "root add must NEVER flag");
        prop_assert_eq!(added.len(), paths.len());

        // Duplicate root rejected without mutating config.
        let before_dup = store.config().unwrap();
        prop_assert!(add_roots_with(&fs, &store, &[paths[0].clone()]).is_err());
        prop_assert_eq!(&store.config().unwrap(), &before_dup);

        // Overlapping nested child rejected without mutating config.
        let nested = PathBuf::from(format!("{}/child", paths[0].display()));
        fs.mkdir(&nested.to_string_lossy(), 1);
        prop_assert!(add_roots_with(&fs, &store, &[nested]).is_err());
        prop_assert_eq!(&store.config().unwrap(), &before_dup);

        // Removing all added roots succeeds even if one was already deleted from disk.
        if delete_before_remove {
            fs.delete_dir_tree(&paths[0].to_string_lossy());
        }
        let rm_args: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let removed = remove_roots_with(&store, &rm_args).unwrap();
        prop_assert_eq!(removed.len(), paths.len());
        prop_assert_eq!(store.config().unwrap().roots, vec![PathBuf::from("/vol/base")]);

        // Removing the last remaining root must error and preserve config.
        prop_assert!(remove_roots_with(&store, &["/vol/base".to_string()]).is_err());
        prop_assert_eq!(store.config().unwrap().roots, vec![PathBuf::from("/vol/base")]);
    }
}
