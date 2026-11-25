# Harden correctness, deterministic state-machine proptests, and the APFS ABI boundary

## Goal

Refactor `diskledger` around explicit I/O, clock, and report boundaries so a deterministic state-machine `proptest` suite can hammer the real `scan`/`store`/`report`/CLI logic against an in-memory APFS simulator — fixing the correctness bugs along the way and guarding Apple's private `fsctl` layout with a live `apfs.util -S` cross-check.

Release binary stays zero-extra-deps (`libc` only); `proptest` and `serde_json` live strictly in `[dev-dependencies]`.

## 1. Architecture Seams (to make state-machine testing natural)

- **`System` trait (`src/kstat.rs` or `src/sys.rs`)**: wrap `query`, `flag`, `volume_space`, directory/symlink `dev` checks, `read_dir` (returning dir/symlink/`.git` presence in one pass), and `now() -> u64`. Production uses `RealSystem`; tests inject `FakeFs` + virtual clock.
- **Pure report types (`src/report.rs`)**: split `diff`, `top`, and `candidates` into pure computation (`compute_diff`, `compute_top`, `compute_candidates`) returning typed structs, plus separate human and JSON renderers (`Write` sink). Under `--no-scan`, none of these functions take a `&dyn System` — disk purity is enforced by the type signature.
- **Typed CLI (`src/main.rs`)**: replace `Args { pos, flags: HashMap<String, String> }` with a pure `parse_args(&[String]) -> Res<Command>` returning an explicit `Command` enum.

## 2. Correctness Fixes

Each fix corresponds to an invariant checked by the property tests:

- **CLI parsing (`src/main.rs`)**:
  - Reject unknown flags (`--dryrun`, `-x`) and flags invalid for the chosen subcommand instead of silently ignoring them (prevents `--dryrun` from triggering a permanent flagging scan).
  - Support `--flag=value` and `--` end-of-options; fix `HELP` to list `size` and `doctor` under `--json`.
- **Snapshot & config integrity (`src/store.rs`, `src/util.rs`)**:
  - Record configured roots in a `#roots\t...` snapshot header line (backward-compatible with existing `#diskledger v1` snapshots).
  - Record `.git` / nested-repo presence on visited directories in an optional 6th TSV column (backward-compatible with existing 5-column snapshots) so `candidates --no-scan` never touches the disk.
  - Escape `\r` in `tsv_esc`/`tsv_unesc` so `str::lines()` cannot corrupt paths.
  - Reject negative, `NaN`, `inf`, and overflowing values in `parse_size` and `parse_duration`.
  - Use unique temp filenames (`.{ts}.{pid}.{seq}.tmp`) in `Store::save` and atomic write+rename in `set_checkpoint` so concurrent read-only scans cannot clobber each other.
  - Fail `Store::load` on malformed numeric fields instead of silently substituting `0` bytes.
  - Normalize trailing slashes and reject duplicate or overlapping roots in `Config` / `init`; reject out-of-range `max_depth`/`max_children`.
- **Scan & reuse invariants (`src/scan.rs`)**:
  - Fix the `reuse_unchanged` orphan bug: when `record_min_bytes` increases, never drop an ancestor while retaining a `pending` descendant (enforce closed-under-ancestors for every non-root row in a snapshot).
  - Check `old.entries == st.entries` alongside `old.gen` and `old.bytes` in `reuse_unchanged`.
- **Report correctness (`src/report.rs`)**:
  - **Removed roots in `diff`**: use `#roots` metadata so a root removed from config is reported as no longer monitored and excluded from `monitored_delta` / `outside_delta`, rather than reported as `[gone]` with phantom outside growth.
  - **Multi-volume safety**: track whether configured roots share one `dev`; avoid computing a bogus single-volume `outside_delta` when roots span volumes.
  - **Opposing deltas in `diff`**: show a parent directory in human mode when any descendant has `|delta| >= min` even if the parent's net delta cancels below `min`, matching `--json`.
  - **Honor `--depth` in `top --json`** to match human output.
  - **`candidates` staleness**: check `o.entries == c.entries` in addition to bytes/gen, and remove the silent `take(200)` history cap when `keep_snapshots > 200`.

## 3. Private APFS ABI Boundary (`src/kstat.rs` + `diskledger doctor`)

- Add an `apfs.util -S` output parser (`descendants`, `physical size`, `gen-count`).
- Harden `kstat::flag` against paths starting with `-` (ensure canonical/absolute path) and use `saturating_mul` in `volume_space`.
- Add **`diskledger doctor [--json]`**:
  - Creates a temporary directory in `TMPDIR` (removed on drop), populates a small subtree, and verifies `kstat::query` against `apfs.util -S` across three states: (1) unflagged (`has_record == false`, `gen == 0`), (2) after `kstat::flag` (`has_record == true`, `gen > 0`), and (3) after mutating a file inside it (`gen` increases, `bytes`/`entries` update).
  - Verifies `volume_space` (`free <= total`, `total > 0`).
  - If a config exists, spot-checks each configured root read-only (`kstat::query` vs `apfs.util -S`, no flagging).
  - Exits non-zero with a clear mismatch report if a macOS update shifts the `fsctl` struct layout.

## 4. Proptest Suite (Deterministic State-Machine Focus)

No hand-rolled single-case unit tests (migrate the 3 small tests in `src/util.rs` into properties too).

1. **Deterministic APFS Ledger State Machine (`proptest`)**:
   - In-memory `FakeFs` faithfully models APFS dir-stats: monotonic flagging, recursive `gen` counter bumps on descendant changes, accurate `entries`/`bytes`, symlinks (never followed), cross-device mounts (never crossed), unreadable dirs, `.git` markers, outside-volume free-space changes, and operation counters (`query`/`walk`/`flag`/`read_dir`).
   - Runs generated sequences of transitions (`CreateFile`, `ResizeFile`, `DeleteFile`, `CreateDir`, `DeleteDir`, `AddSymlink`, `AddCrossDevMount`, `OutsideVolumeDelta`, `AdvanceTime`, `AddRoot`, `RemoveRoot`, `TuneConfig`, `Scan { flag, full }`, `Checkpoint`, `Checkin`, `Prune`) against `(FakeFs, real Store in TempDir, VirtualClock)`.
   - Checks universal invariants across every sequence:
     - **Accounting conservation**: `monitored_delta + outside_delta == -free_delta` and `monitored_delta` equals ground-truth changes inside baselined roots.
     - **Root add/remove isolation**: config changes never fabricate `[gone]`/`[new]` deltas in `monitored_delta` or corrupt `outside_delta`.
     - **Incremental == full scan parity**: incremental scan matches `scan --full` rows and flag decisions when thresholds are unchanged, while reusing untouched flagged subtrees with zero walks.
     - **Flagging monotonicity & dry-run parity**: `scan --dry-run`'s `would_flag` equals `scan`'s `flagged_new`; read-only commands never invoke `flag()`.
     - **Tree closure**: every non-root row in a snapshot has its parent in the same snapshot.
     - **`--no-scan` purity**: `diff`/`top`/`candidates` with `--no-scan` make zero `FakeFs` calls.
     - **Human/JSON parity & schema validity**: every `--json` output parses via `serde_json` and matches the structured report (including `top --depth` and `diff` opposing-delta children).
     - **Store/prune guarantees**: strictly increasing timestamps even with `dt == 0`, lossless TSV roundtrip, checkpoint-pinned snapshots never pruned, daily retention within 30 days.
     - **Candidates safety**: directories holding `.git` (directly or in a recorded/direct-child subtree) never appear as candidates.
2. **CLI & Parser Properties (`proptest`)**:
   - Generated valid `Command` invocations roundtrip cleanly (`--flag val`, `--flag=val`, `--`); unknown or misplaced flags always error.
   - `tsv_esc`/`tsv_unesc`, `json_str`, `parse_size`, `parse_duration`, and `apfs.util -S` parser properties over arbitrary Unicode/edge inputs.
3. **Real-Kernel APFS ABI State-Machine Test (`cargo test` on macOS / CI)**:
   - Runs the `doctor` unflagged → flag → mutate → delete verification against a real APFS temp dir so `cargo test --release --locked` on `macos-latest` CI catches ABI drift automatically.

## Verification

- `cargo test --release --locked` passes all proptests and the live APFS ABI test.
- `cargo build --release --locked` succeeds with zero new runtime dependencies.
- `diskledger doctor` and `diskledger doctor --json` pass against the local APFS volume and existing `~/.diskledger` snapshots load cleanly without migration.
