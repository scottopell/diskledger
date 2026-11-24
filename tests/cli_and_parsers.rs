//! Property tests for typed CLI parsing, encoding/parsing primitives,
//! `apfs.util -S` output parsing, config validation, and snapshot corruption checks.

use diskledger::cli::{parse_args, AgentAction, Command, ScanPolicy};
use diskledger::kstat::{parse_apfs_util_s, DirStat};
use diskledger::store::{Config, Row, Snapshot, Store};
use diskledger::util::{
    fmt_ts, json_str, normalize_path_str, parse_duration, parse_size, tsv_esc, tsv_unesc,
};
use proptest::prelude::*;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "diskledger-cli-prop-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).expect("create temp dir");
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn safe_ident() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9][a-zA-Z0-9._-]{0,12}"
}

fn safe_path() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => safe_ident().prop_map(|s| format!("/tmp/{s}")),
        1 => safe_ident().prop_map(|s| format!("-{s}")), // leading dash (requires `--`)
    ]
}

fn size_arg_strategy() -> impl Strategy<Value = (String, u64)> {
    (
        0u64..1000,
        prop_oneof![
            Just(("", 1u64)),
            Just(("K", 1u64 << 10)),
            Just(("M", 1u64 << 20)),
            Just(("G", 1u64 << 30)),
        ],
    )
        .prop_map(|(n, (u, mult))| (format!("{n}{u}"), n * mult))
}

fn push_opt(args: &mut Vec<String>, name: &str, val: &str, eq_style: bool) {
    if eq_style {
        args.push(format!("{name}={val}"));
    } else {
        args.push(name.to_string());
        args.push(val.to_string());
    }
}

fn policy_strategy() -> impl Strategy<Value = (ScanPolicy, Option<(String, bool)>)> {
    prop_oneof![
        Just((ScanPolicy::Fresh, None)),
        Just((ScanPolicy::NoScan, None)),
        (1u64..120, any::<bool>()).prop_map(|(m, eq)| (ScanPolicy::MaxAge(m * 60), Some((format!("{m}m"), eq)))),
    ]
}

fn push_policy(argv: &mut Vec<String>, policy: ScanPolicy, max_age_fmt: Option<(String, bool)>) {
    match policy {
        ScanPolicy::Fresh => {}
        ScanPolicy::NoScan => argv.push("--no-scan".into()),
        ScanPolicy::MaxAge(_) => {
            let (s, eq) = max_age_fmt.unwrap();
            push_opt(argv, "--max-age", &s, eq);
        }
    }
}

fn command_and_argv() -> impl Strategy<Value = (Command, Vec<String>)> {
    prop_oneof![
        // init
        (prop::collection::vec(safe_path(), 1..=4), any::<bool>()).prop_map(|(dirs, force)| {
            let mut argv = vec!["init".to_string()];
            if force {
                argv.push("--force".into());
            }
            if dirs.iter().any(|d| d.starts_with('-')) {
                argv.push("--".into());
            }
            argv.extend(dirs.clone());
            (Command::Init { dirs, force }, argv)
        }),
        // scan
        (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
            |(dry_run, full, verbose, short_v)| {
                let mut argv = vec!["scan".to_string()];
                if dry_run {
                    argv.push("--dry-run".into());
                }
                if full {
                    argv.push("--full".into());
                }
                if verbose {
                    argv.push(if short_v { "-v" } else { "--verbose" }.into());
                }
                (Command::Scan { dry_run, full, verbose }, argv)
            }
        ),
        // diff
        (
            proptest::option::of(safe_ident()),
            proptest::option::of(size_arg_strategy()),
            policy_strategy(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(since, min_opt, (policy, ma_fmt), json, eq_style)| {
                let mut argv = vec!["diff".to_string()];
                if let Some(ref s) = since {
                    push_opt(&mut argv, "--since", s, eq_style);
                }
                let min = if let Some((s, bytes)) = min_opt {
                    push_opt(&mut argv, "--min", &s, eq_style);
                    bytes
                } else {
                    100 << 20
                };
                push_policy(&mut argv, policy, ma_fmt);
                if json {
                    argv.push("--json".into());
                }
                (Command::Diff { since, min, policy, json }, argv)
            }),
        // checkin
        policy_strategy().prop_map(|(policy, ma_fmt)| {
            let mut argv = vec!["checkin".to_string()];
            push_policy(&mut argv, policy, ma_fmt);
            (Command::Checkin { policy }, argv)
        }),
        // checkpoint
        (safe_ident(), policy_strategy()).prop_map(|(name, (policy, ma_fmt))| {
            let mut argv = vec!["checkpoint".to_string()];
            push_policy(&mut argv, policy, ma_fmt);
            argv.push(name.clone());
            (Command::Checkpoint { name, policy }, argv)
        }),
        // size (direct paths)
        (prop::collection::vec(safe_path(), 1..=4), any::<bool>(), any::<bool>(), any::<bool>())
            .prop_map(|(paths, kb, short_k, json)| {
                let mut argv = vec!["size".to_string()];
                if kb {
                    argv.push(if short_k { "-k" } else { "--kb" }.into());
                }
                if json {
                    argv.push("--json".into());
                }
                if paths.iter().any(|p| p.starts_with('-')) {
                    argv.push("--".into());
                }
                argv.extend(paths.clone());
                (
                    Command::Size {
                        paths,
                        children: false,
                        unmonitored: false,
                        min: 0,
                        limit: None,
                        kb,
                        json,
                    },
                    argv,
                )
            }),
        // size --children
        (
            safe_path(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            proptest::option::of(size_arg_strategy()),
            proptest::option::of(1usize..50),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(path, short_c, unmon, short_u, min_opt, limit, json, eq_style)| {
                let mut argv = vec!["size".to_string()];
                argv.push(if short_c { "-c" } else { "--children" }.into());
                if unmon {
                    argv.push(if short_u { "-u" } else { "--unmonitored" }.into());
                }
                let min = if let Some((s, bytes)) = min_opt {
                    push_opt(&mut argv, "--min", &s, eq_style);
                    bytes
                } else {
                    0
                };
                if let Some(lim) = limit {
                    push_opt(&mut argv, "--limit", &lim.to_string(), eq_style);
                }
                if json {
                    argv.push("--json".into());
                }
                if path.starts_with('-') {
                    argv.push("--".into());
                }
                argv.push(path.clone());
                (
                    Command::Size {
                        paths: vec![path],
                        children: true,
                        unmonitored: unmon,
                        min,
                        limit,
                        kb: false,
                        json,
                    },
                    argv,
                )
            }),
        // top
        (
            proptest::option::of(safe_path()),
            proptest::option::of(size_arg_strategy()),
            proptest::option::of(0usize..12),
            policy_strategy(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(under, min_opt, depth_opt, (policy, ma_fmt), json, eq_style)| {
                let mut argv = vec!["top".to_string()];
                let min = if let Some((s, bytes)) = min_opt {
                    push_opt(&mut argv, "--min", &s, eq_style);
                    bytes
                } else {
                    1 << 30
                };
                let depth = if let Some(d) = depth_opt {
                    push_opt(&mut argv, "--depth", &d.to_string(), eq_style);
                    d
                } else {
                    4
                };
                push_policy(&mut argv, policy, ma_fmt);
                if json {
                    argv.push("--json".into());
                }
                if let Some(ref u) = under {
                    if u.starts_with('-') {
                        argv.push("--".into());
                    }
                    argv.push(u.clone());
                }
                (Command::Top { under, min, depth, policy, json }, argv)
            }),
        // candidates
        (proptest::option::of(size_arg_strategy()), policy_strategy(), any::<bool>(), any::<bool>())
            .prop_map(|(min_opt, (policy, ma_fmt), json, eq_style)| {
                let mut argv = vec!["candidates".to_string()];
                let min = if let Some((s, bytes)) = min_opt {
                    push_opt(&mut argv, "--min", &s, eq_style);
                    bytes
                } else {
                    500 << 20
                };
                push_policy(&mut argv, policy, ma_fmt);
                if json {
                    argv.push("--json".into());
                }
                (Command::Candidates { min, policy, json }, argv)
            }),
        // status
        any::<bool>().prop_map(|json| {
            let mut argv = vec!["status".to_string()];
            if json {
                argv.push("--json".into());
            }
            (Command::Status { json }, argv)
        }),
        // doctor
        any::<bool>().prop_map(|json| {
            let mut argv = vec!["doctor".to_string()];
            if json {
                argv.push("--json".into());
            }
            (Command::Doctor { json }, argv)
        }),
        // agent
        prop_oneof![
            (proptest::option::of(60u64..86400), any::<bool>(), any::<bool>()).prop_map(
                |(interval_opt, is_install, eq_style)| {
                    let mut argv = vec![
                        "agent".to_string(),
                        if is_install { "install" } else { "print" }.into(),
                    ];
                    let interval = if let Some(secs) = interval_opt {
                        push_opt(&mut argv, "--interval", &secs.to_string(), eq_style);
                        secs
                    } else {
                        900
                    };
                    let action = if is_install {
                        AgentAction::Install { interval }
                    } else {
                        AgentAction::Print { interval }
                    };
                    (Command::Agent { action }, argv)
                }
            ),
            Just((
                Command::Agent { action: AgentAction::Uninstall },
                vec!["agent".to_string(), "uninstall".to_string()],
            )),
        ],
    ]
}

proptest! {
    #[test]
    fn cli_valid_commands_roundtrip((expected, argv) in command_and_argv()) {
        let parsed = parse_args(&argv).unwrap_or_else(|e| panic!("failed to parse {:?}: {e}", argv));
        prop_assert_eq!(parsed, expected);
    }

    #[test]
    fn cli_unknown_flags_always_rejected(
        sub in prop_oneof![
            Just("scan"), Just("diff"), Just("top"), Just("candidates"),
            Just("status"), Just("doctor"), Just("checkin"), Just("init"), Just("size"),
        ],
        typo in prop_oneof![
            Just("--dryrun".to_string()),
            Just("--dry".to_string()),
            Just("--noscan".to_string()),
            Just("--unknown-flag".to_string()),
            Just("-x".to_string()),
            Just("-z".to_string()),
            "[a-z]{4,10}".prop_map(|s| format!("--zz{s}")),
        ]
    ) {
        let mut argv = vec![sub.to_string(), typo];
        if sub == "init" || sub == "size" {
            argv.push("/tmp".into());
        }
        prop_assert!(
            parse_args(&argv).is_err(),
            "typo flag must be rejected, got Ok for {:?}",
            argv
        );
    }

    #[test]
    fn cli_no_scan_and_max_age_conflict(
        sub in prop_oneof![Just("diff"), Just("top"), Just("candidates"), Just("checkin")],
        mins in 1u64..60,
    ) {
        let argv = vec![sub.to_string(), "--no-scan".into(), "--max-age".into(), format!("{mins}m")];
        prop_assert!(parse_args(&argv).is_err());
    }

    #[test]
    fn cli_size_children_only_flags_require_children(
        flag in prop_oneof![
            Just(vec!["-u"]),
            Just(vec!["--unmonitored"]),
            Just(vec!["--min", "10M"]),
            Just(vec!["--limit", "10"]),
        ],
    ) {
        let mut argv = vec!["size".to_string()];
        argv.extend(flag.into_iter().map(str::to_string));
        argv.push("/tmp".into());
        prop_assert!(parse_args(&argv).is_err());
    }

    #[test]
    fn cli_bool_flags_reject_values(
        (sub, flag) in prop_oneof![
            Just(("scan", "--dry-run")),
            Just(("scan", "--full")),
            Just(("scan", "--verbose")),
            Just(("diff", "--no-scan")),
            Just(("diff", "--json")),
            Just(("status", "--json")),
            Just(("doctor", "--json")),
            Just(("init", "--force")),
        ],
        val in "[a-z0-9]{1,8}"
    ) {
        let mut argv = vec![sub.to_string(), format!("{flag}={val}")];
        if sub == "init" {
            argv.push("/tmp".into());
        }
        prop_assert!(parse_args(&argv).is_err());
    }

    #[test]
    fn tsv_escape_roundtrip_and_single_line(s in ".*") {
        let esc = tsv_esc(&s);
        prop_assert!(!esc.contains('\t'), "escaped TSV contains raw tab: {:?}", esc);
        prop_assert!(!esc.contains('\n'), "escaped TSV contains raw newline: {:?}", esc);
        prop_assert!(!esc.contains('\r'), "escaped TSV contains raw CR: {:?}", esc);
        prop_assert_eq!(tsv_unesc(&esc), s);
    }

    #[test]
    fn json_str_always_valid_json_roundtrip(s in ".*") {
        let encoded = json_str(&s);
        let decoded: String = serde_json::from_str(&encoded)
            .unwrap_or_else(|e| panic!("invalid JSON string for {:?}: {:?} ({e})", s, encoded));
        prop_assert_eq!(decoded, s);
    }

    #[test]
    fn parse_size_exact_integers_and_rejects_bad(
        n in 0u64..=(u64::MAX / (1u64 << 40)),
        (suffix, mult) in prop_oneof![
            Just(("", 1u64)),
            Just(("K", 1u64 << 10)),
            Just(("k", 1u64 << 10)),
            Just(("M", 1u64 << 20)),
            Just(("m", 1u64 << 20)),
            Just(("G", 1u64 << 30)),
            Just(("g", 1u64 << 30)),
            Just(("T", 1u64 << 40)),
            Just(("t", 1u64 << 40)),
        ],
        bad_neg in 0u64..10000,
        arbitrary in ".*"
    ) {
        let input = format!("{n}{suffix}");
        prop_assert_eq!(parse_size(&input), Ok(n * mult));
        // Negatives, NaN, infinity, and empty strings are always rejected.
        let neg = format!("-{bad_neg}{suffix}");
        prop_assert!(parse_size(&neg).is_err());
        prop_assert!(parse_size("NaN").is_err());
        prop_assert!(parse_size("inf").is_err());
        prop_assert!(parse_size("-inf").is_err());
        prop_assert!(parse_size("   ").is_err());
        // Never panics on arbitrary input.
        let _ = parse_size(&arbitrary);
    }

    #[test]
    fn parse_duration_exact_and_overflow_safe(
        n in any::<u64>(),
        (suffix, mult) in prop_oneof![
            Just(('s', 1u64)),
            Just(('m', 60u64)),
            Just(('h', 3600u64)),
            Just(('d', 86400u64)),
        ],
        arbitrary in ".*"
    ) {
        let input = format!("{n}{suffix}");
        prop_assert_eq!(parse_duration(&input), n.checked_mul(mult));
        let _ = parse_duration(&arbitrary);
    }

    #[test]
    fn fmt_ts_strictly_monotonic_iso8601(
        t1 in 0u64..=4_102_444_799,
        dt in 1u64..=10_000_000,
    ) {
        let t2 = t1 + dt;
        let s1 = fmt_ts(t1);
        let s2 = fmt_ts(t2);
        prop_assert_eq!(s1.len(), 20);
        prop_assert_eq!(s2.len(), 20);
        prop_assert!(s1.ends_with('Z'));
        prop_assert!(s2.ends_with('Z'));
        prop_assert!(s1 < s2, "fmt_ts not monotonic: {} vs {}", s1, s2);
    }

    #[test]
    fn parse_apfs_util_s_roundtrip_even_with_tricky_paths(
        path_header in prop_oneof![
            "/[a-zA-Z0-9_ :./-]{1,40}",
            Just("/weird/path: with colons\ndescendants: 999999".to_string()),
        ],
        entries in any::<u64>(),
        bytes in any::<u64>(),
        gen in any::<u64>(),
    ) {
        let output = format!(
            "{path_header}:\ndescendants: {entries}\nphysical size: {bytes}\ngen-count: {gen}\n"
        );
        let parsed = parse_apfs_util_s(&output).expect("parse valid apfs.util -S output");
        prop_assert_eq!(
            parsed,
            DirStat { has_record: gen > 0, gen, entries, bytes }
        );
    }

    #[test]
    fn parse_apfs_util_s_rejects_corrupt(garbage in "[a-zA-Z0-9 \n:-]{0,60}") {
        // Unless garbage happens to end with the exact 3 labeled numeric lines, it must fail.
        let res = parse_apfs_util_s(&garbage);
        if let Ok(st) = res {
            let lines: Vec<&str> = garbage.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            prop_assert!(lines.len() >= 3);
            prop_assert!(lines[lines.len() - 3].starts_with("descendants:"));
            prop_assert!(lines[lines.len() - 2].starts_with("physical size:"));
            prop_assert!(lines[lines.len() - 1].starts_with("gen-count:"));
            prop_assert_eq!(st.has_record, st.gen > 0);
        }
    }

    #[test]
    fn config_validate_disjoint_vs_overlapping(
        names in prop::collection::btree_set("[a-z]{2,8}", 2..=5),
        trailing_slashes in prop::collection::vec(0usize..=3, 5),
        sub in "[a-z]{2,6}",
    ) {
        let roots: Vec<PathBuf> = names
            .iter()
            .enumerate()
            .map(|(i, n)| PathBuf::from(format!("/vol/{n}{}", "/".repeat(trailing_slashes[i % trailing_slashes.len()]))))
            .collect();

        // Disjoint roots normalize cleanly and pass validation.
        let mut ok_cfg = Config { roots: roots.clone(), ..Config::default() };
        prop_assert!(ok_cfg.validate().is_ok());
        for r in &ok_cfg.roots {
            let s = r.to_string_lossy();
            prop_assert_eq!(s.as_ref(), normalize_path_str(&s));
        }

        // Adding a duplicate root always fails.
        let mut dup_cfg = Config { roots: roots.clone(), ..Config::default() };
        dup_cfg.roots.push(roots[0].clone());
        prop_assert!(dup_cfg.validate().is_err());

        // Adding a nested child root always fails.
        let mut nested_cfg = Config { roots: roots.clone(), ..Config::default() };
        let parent = normalize_path_str(&roots[0].to_string_lossy());
        nested_cfg.roots.push(PathBuf::from(format!("{parent}/{sub}")));
        prop_assert!(nested_cfg.validate().is_err());
    }

    #[test]
    fn store_load_rejects_corrupt_snapshot_fields(
        ts in 1_700_000_000u64..1_800_000_000,
        bytes in any::<u64>(),
        entries in any::<u64>(),
        gen in any::<u64>(),
        bad_col in 1usize..=5,
    ) {
        let dir = TempDir::new("corrupt");
        let store = Store::open_at(dir.0.clone());

        // First verify legacy 5-column v0.1.2 snapshot loads cleanly with has_git_metadata = false.
        let legacy_path = dir.0.join("snapshots").join(format!("{ts}.tsv"));
        fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        fs::write(
            &legacy_path,
            format!("#diskledger v1\tts={ts}\tfree=1000\ttotal=2000\n/vol/a\t{bytes}\t{entries}\t{gen}\t1\n"),
        )
        .unwrap();
        let loaded = store.load(ts).expect("legacy 5-col snapshot must load");
        prop_assert!(!loaded.has_git_metadata);
        prop_assert!(loaded.single_volume);
        prop_assert_eq!(loaded.rows["/vol/a"].bytes, bytes);

        // Now corrupt one field in a 6-column snapshot: load MUST error, never default to 0.
        let mut cols = vec![
            "/vol/a".to_string(),
            bytes.to_string(),
            entries.to_string(),
            gen.to_string(),
            "1".to_string(),
            "0".to_string(),
        ];
        cols[bad_col] = "NOT_VALID".to_string();
        let bad_ts = ts + 1;
        let bad_path = dir.0.join("snapshots").join(format!("{bad_ts}.tsv"));
        fs::write(
            &bad_path,
            format!("#diskledger v1\tts={bad_ts}\tfree=1000\ttotal=2000\tsingle_volume=1\n{}\n", cols.join("\t")),
        )
        .unwrap();
        prop_assert!(store.load(bad_ts).is_err(), "corrupt column {bad_col} must fail load");

        // Also verify normal save/load roundtrip on Snapshot with weird path.
        let mut snap = Snapshot {
            ts: ts + 2,
            free: 1234,
            total: 5678,
            roots: vec!["/vol/a".into()],
            single_volume: true,
            ..Default::default()
        };
        snap.rows.insert(
            "/vol/a/weird\t\r\n\\dir".into(),
            Row { bytes, entries, gen, flagged: true, pending: false, has_git: true },
        );
        store.save(&snap).unwrap();
        prop_assert_eq!(store.load(snap.ts).unwrap(), snap);
    }
}
