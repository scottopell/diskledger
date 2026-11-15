//! APFS kernel directory statistics.
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
//! `apfs.util` on macOS 26.5 and may change in a later release.

use std::ffi::CString;
use std::io;
use std::os::raw::{c_char, c_int, c_uint, c_ulong, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

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

#[derive(Debug, Clone, Copy)]
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

pub fn flag(path: &Path) -> io::Result<()> {
    let out = Command::new(APFS_UTIL).arg("-M").arg(path).output()?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            msg
        };
        Err(io::Error::new(io::ErrorKind::Other, msg))
    }
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
    Ok((s.f_bavail as u64 * bs, s.f_blocks as u64 * bs))
}
