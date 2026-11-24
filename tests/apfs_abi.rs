//! Real-kernel macOS APFS ABI verification against Apple's `apfs.util -S`.
//! Runs on ephemeral temp directories (cleaned up on drop) so `cargo test`
//! and `macos-latest` CI catch private `fsctl` struct layout drift immediately.

use diskledger::kstat::{self, CheckStatus};
use proptest::prelude::*;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct EphemeralDir(PathBuf);

impl EphemeralDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "diskledger-apfs-abi-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).expect("create ephemeral dir");
        EphemeralDir(p)
    }
}

impl Drop for EphemeralDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_synced(path: &std::path::Path, bytes: usize) {
    let mut f = fs::File::create(path).expect("create probe file");
    if bytes > 0 {
        f.write_all(&vec![0xa5u8; bytes]).expect("write probe file");
    }
    f.sync_all().expect("sync probe file");
}

#[derive(Debug, Clone)]
enum KernelStep {
    Write { sub: u8, name: u8, blocks_4k: usize },
    Remove { sub: u8, name: u8 },
}

fn kernel_step_strategy() -> impl Strategy<Value = KernelStep> {
    prop_oneof![
        3 => (0u8..3, 0u8..5, 1usize..=4).prop_map(|(sub, name, blocks_4k)| KernelStep::Write { sub, name, blocks_4k }),
        1 => (0u8..3, 0u8..5).prop_map(|(sub, name)| KernelStep::Remove { sub, name }),
    ]
}

#[test]
fn doctor_verifies_real_apfs_abi_and_renders_valid_json() {
    let rep = kstat::run_doctor(&[]);
    let mut hbuf = Vec::new();
    rep.render_human(&mut hbuf).expect("render human");
    let human = String::from_utf8(hbuf).expect("utf8");
    assert!(rep.ok, "doctor failed:\n{human}");
    for expected in ["abi_unflagged", "abi_flagged", "abi_mutated", "volume_space"] {
        let check = rep
            .checks
            .iter()
            .find(|c| c.name == expected)
            .unwrap_or_else(|| panic!("missing check {expected}"));
        assert_eq!(check.status, CheckStatus::Pass, "{expected} did not pass: {}", check.detail);
    }
    let mut jbuf = Vec::new();
    rep.render_json(&mut jbuf).expect("render json");
    let v: serde_json::Value = serde_json::from_slice(&jbuf).expect("valid doctor json");
    assert_eq!(v["ok"], serde_json::Value::Bool(true));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 6, ..ProptestConfig::default() })]

    /// Mini real-kernel state machine: unflagged setup -> verify fsctl == apfs.util -S ->
    /// flag -> verify gen >= 1 -> apply generated mutations -> verify gen advances and
    /// fsctl continues matching apfs.util -S on every single step.
    #[test]
    fn real_apfs_fsctl_matches_apfs_util_across_mutations(
        pre_steps in prop::collection::vec(kernel_step_strategy(), 1..=4),
        post_steps in prop::collection::vec(kernel_step_strategy(), 1..=5),
    ) {
        let dir = EphemeralDir::new("sm");
        for sub in 0..3u8 {
            fs::create_dir_all(dir.0.join(format!("s{sub}"))).unwrap();
        }

        let apply = |step: &KernelStep| -> bool {
            match *step {
                KernelStep::Write { sub, name, blocks_4k } => {
                    let p = dir.0.join(format!("s{sub}/f{name}.bin"));
                    write_synced(&p, blocks_4k * 4096);
                    true
                }
                KernelStep::Remove { sub, name } => {
                    let p = dir.0.join(format!("s{sub}/f{name}.bin"));
                    fs::remove_file(&p).is_ok()
                }
            }
        };

        for s in &pre_steps {
            apply(s);
        }

        // Unflagged: fsctl must match apfs.util -S identically with has_record == false, gen == 0.
        let unflagged_f = kstat::query(&dir.0).expect("fsctl query unflagged");
        let unflagged_u = kstat::query_via_apfs_util(&dir.0).expect("apfs.util -S unflagged");
        prop_assert_eq!(unflagged_f, unflagged_u);
        prop_assert!(!unflagged_f.has_record);
        prop_assert_eq!(unflagged_f.gen, 0);

        // Flag the ephemeral directory.
        kstat::flag(&dir.0).expect("flag ephemeral dir");
        let mut last = kstat::query(&dir.0).expect("fsctl query flagged");
        let flagged_u = kstat::query_via_apfs_util(&dir.0).expect("apfs.util -S flagged");
        prop_assert_eq!(last, flagged_u);
        prop_assert!(last.has_record);
        prop_assert!(last.gen >= 1);
        prop_assert_eq!(last.entries, unflagged_f.entries);
        prop_assert_eq!(last.bytes, unflagged_f.bytes);

        // Apply post-flag mutations; every real mutation must advance `gen` and match apfs.util -S.
        for s in &post_steps {
            let mutated = apply(s);
            let cur_f = kstat::query(&dir.0).expect("fsctl query post-mutation");
            let cur_u = kstat::query_via_apfs_util(&dir.0).expect("apfs.util -S post-mutation");
            prop_assert_eq!(cur_f, cur_u);
            prop_assert!(cur_f.has_record);
            if mutated {
                prop_assert!(
                    cur_f.gen > last.gen,
                    "expected gen to advance after mutation ({:?}): {} -> {}",
                    s,
                    last.gen,
                    cur_f.gen
                );
            } else {
                prop_assert_eq!(cur_f.gen, last.gen);
            }
            last = cur_f;
        }
    }
}
