use std::time::{SystemTime, UNIX_EPOCH};

pub type Res<T> = Result<T, String>;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// "12.3G", "456M", "7K". Binary units.
pub fn human(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b >= K * K * K {
        format!("{:.1}G", b / (K * K * K))
    } else if b >= K * K {
        format!("{:.0}M", b / (K * K))
    } else if b >= K {
        format!("{:.0}K", b / K)
    } else {
        format!("{}B", bytes)
    }
}

pub fn human_delta(d: i64) -> String {
    if d == 0 {
        "0".to_string()
    } else {
        format!("{}{}", if d > 0 { "+" } else { "-" }, human(d.unsigned_abs()))
    }
}

/// "500M", "2G", "1024" (bytes). Rejects negative, NaN, infinity, and u64 overflow.
pub fn parse_size(s: &str) -> Res<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size (use e.g. 500M, 2G)".into());
    }
    if s.starts_with('-') || s.starts_with('+') {
        return Err(format!("bad size '{s}' (must be non-negative, e.g. 500M, 2G)"));
    }
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1u64 << 20),
        Some('G') => (&s[..s.len() - 1], 1u64 << 30),
        Some('T') => (&s[..s.len() - 1], 1u64 << 40),
        _ => (s, 1),
    };
    if num.is_empty() {
        return Err(format!("bad size '{s}' (use e.g. 500M, 2G)"));
    }
    if let Ok(exact) = num.parse::<u64>() {
        return exact.checked_mul(mult).ok_or_else(|| format!("size '{s}' is too large"));
    }
    let v: f64 = num.parse().map_err(|_| format!("bad size '{s}' (use e.g. 500M, 2G)"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!("bad size '{s}' (use e.g. 500M, 2G)"));
    }
    let scaled = v * mult as f64;
    // u64::MAX as f64 rounds up to 2^64; anything >= that overflows u64.
    if !scaled.is_finite() || scaled < 0.0 || scaled >= (u64::MAX as f64) {
        return Err(format!("size '{s}' is too large"));
    }
    Ok(scaled as u64)
}

/// "90m", "2h", "3d" -> seconds. Returns None on invalid syntax or overflow.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult): (&str, u64) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        _ => return None,
    };
    num.parse::<u64>().ok().and_then(|v| v.checked_mul(mult))
}

pub fn fmt_age(secs: u64) -> String {
    if secs >= 2 * 86400 {
        format!("{}d", secs / 86400)
    } else if secs >= 2 * 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 120 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

/// UTC timestamp, "2026-11-15T17:04:05Z".
pub fn fmt_ts(ts: u64) -> String {
    let days = (ts / 86400) as i64;
    let rem = ts % 86400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

pub fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Escape a path for one TSV field.
pub fn tsv_esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

pub fn tsv_unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('t') => o.push('\t'),
                Some('n') => o.push('\n'),
                Some('r') => o.push('\r'),
                Some(other) => o.push(other),
                None => {}
            }
        } else {
            o.push(c);
        }
    }
    o
}

/// Strip trailing slashes unless the path is "/".
pub fn normalize_path_str(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && path.starts_with('/') {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parent of "/a/b/c" is Some("/a/b"); parent of "/a" is Some("/"); parent of "/" is None.
pub fn parent_of(path: &str) -> Option<&str> {
    let i = path.rfind('/')?;
    if i == 0 {
        if path.len() > 1 { Some("/") } else { None }
    } else {
        Some(&path[..i])
    }
}

pub fn base_name(path: &str) -> &str {
    if path == "/" {
        "/"
    } else {
        path.rsplit('/').next().unwrap_or(path)
    }
}

/// True when `path` is a strict descendant of `ancestor`.
pub fn is_under(path: &str, ancestor: &str) -> bool {
    if ancestor == "/" {
        path.starts_with('/') && path.len() > 1
    } else {
        path.starts_with(ancestor) && path.as_bytes().get(ancestor.len()) == Some(&b'/')
    }
}

/// True when `path == ancestor` or `path` is beneath `ancestor`.
pub fn is_same_or_under(path: &str, ancestor: &str) -> bool {
    path == ancestor || is_under(path, ancestor)
}
