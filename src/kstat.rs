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
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::raw::{c_char, c_int, c_uint, c_ulong, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

extern "C" {
    fn fsctl(path: *const c_char, request: c_ulong, data: *mut c_void, options: c_uint) -> c_int;
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

    // 4. Volume space sanity
    match volume_space(&probe_path) {
        Ok((free, total)) if total > 0 && free <= total => checks.push(DoctorCheck {
            name: "volume_space".into(),
            status: CheckStatus::Pass,
            detail: format!("free={free} <= total={total}"),
        }),
        Ok((free, total)) => checks.push(DoctorCheck {
            name: "volume_space".into(),
            status: CheckStatus::Fail,
            detail: format!("invalid volume space: free={free}, total={total}"),
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
