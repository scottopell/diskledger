//! APFS kernel directory statistics and ABI verification.
//!
//! `query` issues the same fsctl that `apfs.util -S` uses. If the directory
//! has its own dir-stats record the kernel answers from that record (tens of
//! microseconds). If not, the kernel walks the subtree and returns the same
//! numbers, just slowly. Either way the answer is correct.
//!
//! `flag` shells out to Apple's `apfs.util -M`, which gives the directory its
//! own record. Flagging walks the subtree once and cannot be undone.
//!
//! This interface is private to Apple. The struct layout below was read from
//! `apfs.util` on macOS 26.5 and may change in a later release. Use
//! `diskledger doctor` (or `run_doctor`) to verify `fsctl` matches `apfs.util -S`.

use crate::util::json_str;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Write};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

extern "C" {
    fn fsctl(path: *const c_char, request: c_ulong, data: *mut c_void, options: c_uint) -> c_int;
    fn getattrlist(
        path: *const c_char,
        attrlist: *mut c_void,
        attrbuf: *mut c_void,
        attrbufsize: libc::size_t,
        options: c_uint,
    ) -> c_int;
    fn fs_snapshot_list(
        dirfd: c_int,
        attrlist: *mut c_void,
        attrbuf: *mut c_void,
        bufsize: libc::size_t,
        flags: u32,
    ) -> c_int;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFURLVolumeAvailableCapacityKey: *const c_void;
    static kCFURLVolumeAvailableCapacityForImportantUsageKey: *const c_void;
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: *const c_void,
        buffer: *const u8,
        buf_len: c_long,
        is_directory: u8,
    ) -> *const c_void;
    fn CFURLCopyResourcePropertyForKey(
        url: *const c_void,
        key: *const c_void,
        property_value_type_ref_ptr: *mut *const c_void,
        error: *mut *const c_void,
    ) -> u8;
    fn CFNumberGetValue(number: *const c_void, the_type: c_long, value_ptr: *mut c_void) -> u8;
    fn CFRelease(cf: *const c_void);
}

/// _IOWR('J', 0x71, 272-byte struct)
const APFS_DIR_STATS_IOC: c_ulong = 0xc110_4a71;
const BUF_WORDS: usize = 34; // 272 bytes

// Offsets into the returned struct, in 8-byte words.
const W_RECORD_ID: usize = 3; // 0x18: non-zero when the directory has its own record
const W_GEN: usize = 6; // 0x30: recursive generation count
const W_ENTRIES: usize = 7; // 0x38: descendant count
const W_BYTES: usize = 8; // 0x40: allocated bytes of all descendants

pub const APFS_UTIL: &str = "/System/Library/Filesystems/apfs.fs/Contents/Resources/apfs.util";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirStat {
    /// True when the kernel maintains a record for this directory.
    pub has_record: bool,
    /// Bumped whenever anything beneath changes. Zero without a record.
    pub gen: u64,
    pub entries: u64,
    pub bytes: u64,
}

pub fn query(path: &Path) -> io::Result<DirStat> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut buf = [0u64; BUF_WORDS];
    // All-zero input selects the read operation, exactly as `apfs.util -S` does.
    let rc = unsafe { fsctl(c.as_ptr(), APFS_DIR_STATS_IOC, buf.as_mut_ptr() as *mut c_void, 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(DirStat {
        has_record: buf[W_RECORD_ID] != 0,
        gen: buf[W_GEN],
        entries: buf[W_ENTRIES],
        bytes: buf[W_BYTES],
    })
}

/// Canonicalize `path` before passing to `apfs.util` so a relative path starting
/// with `-` cannot be misinterpreted as an option flag.
fn safe_apfs_arg(path: &Path) -> io::Result<PathBuf> {
    let p = fs::canonicalize(path)?;
    if !p.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "canonical path is not absolute"));
    }
    Ok(p)
}

pub fn flag(path: &Path) -> io::Result<()> {
    let abs = safe_apfs_arg(path)?;
    let out = Command::new(APFS_UTIL).arg("-M").arg(&abs).output()?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            msg
        };
        Err(io::Error::other(msg))
    }
}

/// Parse the stdout of `apfs.util -S <dir>`.
///
/// Format:
/// ```text
/// /path/to/dir:
/// descendants: 97
/// physical size: 405504
/// gen-count: 0
/// ```
/// Inspects the last 3 non-empty lines so paths containing `:` or newlines cannot confuse parsing.
pub fn parse_apfs_util_s(output: &str) -> io::Result<DirStat> {
    let lines: Vec<&str> = output.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected apfs.util -S output ({} lines)", lines.len()),
        ));
    }
    let tail = &lines[lines.len() - 3..];
    let entries = parse_kv(tail[0], "descendants:")?;
    let bytes = parse_kv(tail[1], "physical size:")?;
    let gen = parse_kv(tail[2], "gen-count:")?;
    Ok(DirStat {
        has_record: gen > 0,
        gen,
        entries,
        bytes,
    })
}

fn parse_kv(line: &str, prefix: &str) -> io::Result<u64> {
    let rest = line.strip_prefix(prefix).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("expected '{prefix}', got '{line}'"))
    })?;
    rest.trim().parse::<u64>().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, format!("bad integer in '{line}'"))
    })
}

pub fn query_via_apfs_util(path: &Path) -> io::Result<DirStat> {
    let abs = safe_apfs_arg(path)?;
    let out = Command::new(APFS_UTIL).arg("-S").arg(&abs).output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            msg
        };
        return Err(io::Error::other(msg));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_apfs_util_s(&text)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Free bytes immediately available to this user (`statfs.f_bavail * f_bsize`).
    pub free: u64,
    /// Total APFS container/volume capacity (`statfs.f_blocks * f_bsize`).
    pub total: u64,
    /// Purgeable bytes macOS can reclaim on demand (`max(important_avail - avail, 0)`).
    pub purgeable: Option<u64>,
    /// Bytes used by this APFS volume alone (`getattrlist ATTR_VOL_SPACEUSED`).
    /// `(total - free) - volume_used` is space used by sibling volumes in the same
    /// APFS container (e.g. VM swapfile, System, Preboot, Recovery).
    pub volume_used: Option<u64>,
    /// Local APFS snapshot names on this volume (`fs_snapshot_list`).
    pub snapshots: Vec<String>,
}

impl VolumeInfo {
    /// Bytes used by sibling volumes in the same APFS container (e.g. VM swap, System).
    pub fn other_volumes_used(&self) -> Option<u64> {
        let vu = self.volume_used?;
        Some(self.total.saturating_sub(self.free).saturating_sub(vu))
    }
}

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_VOL_INFO: u32 = 0x8000_0000;
const ATTR_VOL_SPACEUSED: u32 = 0x0080_0000;

fn query_volume_used(mount_c: &CStr) -> Option<u64> {
    #[repr(C, packed(4))]
    struct VolUsedBuf {
        length: u32,
        space_used: u64,
    }
    let mut alist = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: ATTR_VOL_INFO | ATTR_VOL_SPACEUSED,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buf = VolUsedBuf { length: 0, space_used: 0 };
    let rc = unsafe {
        getattrlist(
            mount_c.as_ptr(),
            &mut alist as *mut AttrList as *mut c_void,
            &mut buf as *mut VolUsedBuf as *mut c_void,
            std::mem::size_of::<VolUsedBuf>(),
            0,
        )
    };
    if rc == 0 && buf.length as usize >= std::mem::size_of::<VolUsedBuf>() {
        Some(buf.space_used)
    } else {
        None
    }
}

fn query_purgeable_bytes(path: &Path) -> Option<u64> {
    const K_CF_NUMBER_SINT64_TYPE: c_long = 4;
    let bytes = path.as_os_str().as_bytes();
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            bytes.as_ptr(),
            bytes.len() as c_long,
            1,
        );
        if url.is_null() {
            return None;
        }
        let get_i64 = |key: *const c_void| -> Option<i64> {
            let mut val: *const c_void = std::ptr::null();
            let mut err: *const c_void = std::ptr::null();
            let ok = CFURLCopyResourcePropertyForKey(url, key, &mut val, &mut err);
            if !err.is_null() {
                CFRelease(err);
            }
            if ok == 0 || val.is_null() {
                if !val.is_null() {
                    CFRelease(val);
                }
                return None;
            }
            let mut out: i64 = 0;
            let got = CFNumberGetValue(val, K_CF_NUMBER_SINT64_TYPE, &mut out as *mut i64 as *mut c_void);
            CFRelease(val);
            if got != 0 { Some(out) } else { None }
        };
        let avail = get_i64(kCFURLVolumeAvailableCapacityKey);
        let important = get_i64(kCFURLVolumeAvailableCapacityForImportantUsageKey);
        CFRelease(url);
        match (avail, important) {
            (Some(a), Some(i)) if i >= a => Some((i - a) as u64),
            (Some(_), Some(_)) => Some(0),
            _ => None,
        }
    }
}

fn query_apfs_snapshots(mount_c: &CStr) -> Vec<String> {
    let fd = unsafe { libc::open(mount_c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if fd < 0 {
        return Vec::new();
    }
    struct FdGuard(c_int);
    impl Drop for FdGuard {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }
    let _guard = FdGuard(fd);

    let mut alist = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut names = Vec::new();
    loop {
        let n = unsafe {
            fs_snapshot_list(
                fd,
                &mut alist as *mut AttrList as *mut c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                0,
            )
        };
        if n <= 0 {
            break;
        }
        let mut off = 0usize;
        for _ in 0..n {
            if off + 32 > buf.len() {
                break;
            }
            let rec_len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            if rec_len < 32 || off + rec_len > buf.len() {
                break;
            }
            // Layout: u32 length (0..4), attribute_set_t (4..24), attrreference_t { i32 offset, u32 len } (24..32)
            let data_off = i32::from_ne_bytes(buf[off + 24..off + 28].try_into().unwrap());
            let data_len = u32::from_ne_bytes(buf[off + 28..off + 32].try_into().unwrap()) as usize;
            if data_off >= 0 {
                let start = off + 24 + data_off as usize;
                let end = start.saturating_add(data_len).min(off + rec_len);
                if start <= end {
                    let slice = &buf[start..end];
                    let trimmed = slice.split(|&b| b == 0).next().unwrap_or(slice);
                    if !trimmed.is_empty() {
                        names.push(String::from_utf8_lossy(trimmed).into_owned());
                    }
                }
            }
            off += rec_len;
        }
    }
    names.sort();
    names
}

/// Full APFS volume space breakdown (free, total, purgeable, per-volume used, local snapshots).
pub fn volume_info(path: &Path) -> io::Result<VolumeInfo> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let bs = s.f_bsize as u64;
    let free = (s.f_bavail as u64).saturating_mul(bs);
    let total = (s.f_blocks as u64).saturating_mul(bs);
    let mount_c = unsafe { CStr::from_ptr(s.f_mntonname.as_ptr()) };
    let volume_used = query_volume_used(mount_c);
    let purgeable = query_purgeable_bytes(path);
    let snapshots = query_apfs_snapshots(mount_c);
    Ok(VolumeInfo { free, total, purgeable, volume_used, snapshots })
}

/// (free bytes available to this user, total bytes) for the volume holding `path`.
pub fn volume_space(path: &Path) -> io::Result<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let bs = s.f_bsize as u64;
    Ok((
        (s.f_bavail as u64).saturating_mul(bs),
        (s.f_blocks as u64).saturating_mul(bs),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Skip,
    Fail,
}

impl CheckStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckStatus::Pass => "pass",
            CheckStatus::Skip => "skip",
            CheckStatus::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn render_human(&self, w: &mut impl Write) -> io::Result<()> {
        for c in &self.checks {
            writeln!(w, "[{:>4}] {}: {}", c.status.as_str(), c.name, c.detail)?;
        }
        if self.ok {
            writeln!(w, "APFS kernel interface verified.")?;
        } else {
            writeln!(w, "One or more APFS kernel checks failed.")?;
        }
        Ok(())
    }

    pub fn render_json(&self, w: &mut impl Write) -> io::Result<()> {
        let items: Vec<String> = self
            .checks
            .iter()
            .map(|c| {
                format!(
                    "{{\"name\":{},\"status\":{},\"detail\":{}}}",
                    json_str(&c.name),
                    json_str(c.status.as_str()),
                    json_str(&c.detail)
                )
            })
            .collect();
        writeln!(w, "{{\"ok\":{},\"checks\":[{}]}}", self.ok, items.join(","))
    }
}

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_probe_file(path: &Path, len: usize) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(&vec![0x5au8; len])?;
    f.sync_all()
}

fn compare_stats(fsctl_st: DirStat, util_st: DirStat) -> Result<(), String> {
    if fsctl_st.entries != util_st.entries
        || fsctl_st.bytes != util_st.bytes
        || fsctl_st.gen != util_st.gen
        || fsctl_st.has_record != util_st.has_record
    {
        Err(format!(
            "fsctl {:?} != apfs.util -S {:?}",
            fsctl_st, util_st
        ))
    } else {
        Ok(())
    }
}

/// Cross-check the private `fsctl` struct layout against Apple's `apfs.util -S`
/// using an ephemeral temp directory (unflagged -> flagged -> mutated) plus
/// read-only spot checks on any configured roots.
pub fn run_doctor(roots: &[PathBuf]) -> DoctorReport {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut checks = Vec::new();
    let probe_path = std::env::temp_dir().join(format!(
        "diskledger-doctor-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _guard = TempDirGuard(probe_path.clone());

    let setup = fs::create_dir_all(probe_path.join("sub"))
        .and_then(|_| write_probe_file(&probe_path.join("sub/probe1.bin"), 8192));
    if let Err(e) = setup {
        checks.push(DoctorCheck {
            name: "probe_setup".into(),
            status: CheckStatus::Fail,
            detail: format!("could not create temp probe dir: {e}"),
        });
        return DoctorReport { ok: false, checks };
    }

    // 1. Unflagged state
    let unflagged_res = (|| -> Result<DirStat, String> {
        let f = query(&probe_path).map_err(|e| format!("fsctl query: {e}"))?;
        let u = query_via_apfs_util(&probe_path).map_err(|e| format!("apfs.util -S: {e}"))?;
        compare_stats(f, u)?;
        if f.has_record || f.gen != 0 {
            return Err(format!("expected unflagged dir (has_record=false, gen=0), got {f:?}"));
        }
        if f.entries < 2 || f.bytes == 0 {
            return Err(format!("expected >=2 entries and >0 bytes in probe dir, got {f:?}"));
        }
        Ok(f)
    })();
    match &unflagged_res {
        Ok(st) => checks.push(DoctorCheck {
            name: "abi_unflagged".into(),
            status: CheckStatus::Pass,
            detail: format!("fsctl matches apfs.util -S (entries={}, bytes={}, gen=0)", st.entries, st.bytes),
        }),
        Err(msg) => checks.push(DoctorCheck {
            name: "abi_unflagged".into(),
            status: CheckStatus::Fail,
            detail: msg.clone(),
        }),
    }

    // 2. Flagged state
    let flagged_res = (|| -> Result<DirStat, String> {
        flag(&probe_path).map_err(|e| format!("apfs.util -M: {e}"))?;
        let f = query(&probe_path).map_err(|e| format!("fsctl query: {e}"))?;
        let u = query_via_apfs_util(&probe_path).map_err(|e| format!("apfs.util -S: {e}"))?;
        compare_stats(f, u)?;
        if !f.has_record || f.gen == 0 {
            return Err(format!("expected flagged dir (has_record=true, gen>0), got {f:?}"));
        }
        Ok(f)
    })();
    match &flagged_res {
        Ok(st) => checks.push(DoctorCheck {
            name: "abi_flagged".into(),
            status: CheckStatus::Pass,
            detail: format!("fsctl matches apfs.util -S after flag (entries={}, bytes={}, gen={})", st.entries, st.bytes, st.gen),
        }),
        Err(msg) => checks.push(DoctorCheck {
            name: "abi_flagged".into(),
            status: CheckStatus::Fail,
            detail: msg.clone(),
        }),
    }

    // 3. Mutated state (verify gen increments and bytes/entries update)
    if let Ok(before) = flagged_res {
        let mut_res = (|| -> Result<DirStat, String> {
            write_probe_file(&probe_path.join("sub/probe2.bin"), 8192)
                .map_err(|e| format!("write probe2: {e}"))?;
            let f = query(&probe_path).map_err(|e| format!("fsctl query: {e}"))?;
            let u = query_via_apfs_util(&probe_path).map_err(|e| format!("apfs.util -S: {e}"))?;
            compare_stats(f, u)?;
            if f.entries != before.entries + 1 {
                return Err(format!("expected entries {}, got {}", before.entries + 1, f.entries));
            }
            if f.bytes <= before.bytes {
                return Err(format!("expected bytes > {}, got {}", before.bytes, f.bytes));
            }
            if f.gen <= before.gen {
                return Err(format!("expected gen > {}, got {}", before.gen, f.gen));
            }
            Ok(f)
        })();
        match mut_res {
            Ok(st) => checks.push(DoctorCheck {
                name: "abi_mutated".into(),
                status: CheckStatus::Pass,
                detail: format!(
                    "gen advanced {} -> {}, entries {} -> {}, bytes {} -> {} (matches apfs.util -S)",
                    before.gen, st.gen, before.entries, st.entries, before.bytes, st.bytes
                ),
            }),
            Err(msg) => checks.push(DoctorCheck {
                name: "abi_mutated".into(),
                status: CheckStatus::Fail,
                detail: msg,
            }),
        }
    }

    // 4. Volume space & APFS breakdown sanity
    match volume_info(&probe_path) {
        Ok(vi) if vi.total > 0 && vi.free <= vi.total => {
            let purge_ok = vi.purgeable.map_or(true, |p| vi.free.saturating_add(p) <= vi.total);
            let vol_ok = vi
                .volume_used
                .map_or(true, |vu| vu <= vi.total.saturating_sub(vi.free) + (1 << 30));
            if purge_ok && vol_ok {
                checks.push(DoctorCheck {
                    name: "volume_space".into(),
                    status: CheckStatus::Pass,
                    detail: format!(
                        "free={} <= total={}, purgeable={:?}, vol_used={:?}, apfs_snapshots={}",
                        vi.free,
                        vi.total,
                        vi.purgeable,
                        vi.volume_used,
                        vi.snapshots.len()
                    ),
                });
            } else {
                checks.push(DoctorCheck {
                    name: "volume_space".into(),
                    status: CheckStatus::Fail,
                    detail: format!("inconsistent volume info: {vi:?}"),
                });
            }
        }
        Ok(vi) => checks.push(DoctorCheck {
            name: "volume_space".into(),
            status: CheckStatus::Fail,
            detail: format!("invalid volume space: free={}, total={}", vi.free, vi.total),
        }),
        Err(e) => checks.push(DoctorCheck {
            name: "volume_space".into(),
            status: CheckStatus::Fail,
            detail: format!("statfs failed: {e}"),
        }),
    }

    // 5. Read-only spot checks on configured roots (retry if actively mutating).
    for root in roots {
        let name = format!("root:{}", root.display());
        if !root.is_dir() {
            checks.push(DoctorCheck {
                name,
                status: CheckStatus::Fail,
                detail: "not a directory".into(),
            });
            continue;
        }
        let mut settled = None;
        let mut last_err = None;
        for _ in 0..3 {
            let before = match query(root) {
                Ok(s) => s,
                Err(e) => {
                    last_err = Some(format!("fsctl query: {e}"));
                    break;
                }
            };
            let util = match query_via_apfs_util(root) {
                Ok(s) => s,
                Err(e) => {
                    last_err = Some(format!("apfs.util -S: {e}"));
                    break;
                }
            };
            let after = match query(root) {
                Ok(s) => s,
                Err(e) => {
                    last_err = Some(format!("fsctl query: {e}"));
                    break;
                }
            };
            if before == after {
                settled = Some((before, util));
                break;
            }
        }
        if let Some(e) = last_err {
            checks.push(DoctorCheck { name, status: CheckStatus::Fail, detail: e });
        } else if let Some((f, u)) = settled {
            match compare_stats(f, u) {
                Ok(()) => checks.push(DoctorCheck {
                    name,
                    status: CheckStatus::Pass,
                    detail: format!(
                        "matches apfs.util -S (entries={}, bytes={}, gen={}, flagged={})",
                        f.entries, f.bytes, f.gen, f.has_record
                    ),
                }),
                Err(msg) => checks.push(DoctorCheck {
                    name,
                    status: CheckStatus::Fail,
                    detail: msg,
                }),
            }
        } else {
            checks.push(DoctorCheck {
                name,
                status: CheckStatus::Skip,
                detail: "directory changed during spot check; skipped".into(),
            });
        }
    }

    let ok = checks.iter().all(|c| c.status != CheckStatus::Fail);
    DoctorReport { ok, checks }
}
