# diskledger

Tracks directory sizes over time on macOS/APFS so you can ask what grew,
what shrank, and what could be freed, without re-running `du`.

It reads sizes from the kernel's APFS directory statistics. A directory that
has been flagged ("maintain-dir-stats") answers instantly; any other
directory still gets a correct answer, because the kernel walks it. The
periodic `scan` flags large directories so that answers get faster over time.

## Quick start

Install a release build (Apple silicon):

```bash
gh release download --repo scottopell/diskledger --pattern '*aarch64-apple-darwin.tar.gz' --output - | tar -xz --strip-components 1 -C ~/.local/bin '*/diskledger'
```

Or build from source with `cargo install --path .`. Then:

```bash
diskledger init ~/dev ~/.codex-workspaces
diskledger scan --dry-run      # see what would be flagged; changes nothing
diskledger scan                # record sizes and flag
diskledger checkin             # mark a baseline
# ...later...
diskledger diff                # what changed since the last checkin
```

## Commands

| Command | What it does | Changes the disk? |
|---|---|---|
| `init <dir>...` | Write the config with these monitored directories | Config only |
| `scan` | Record sizes; flag large directories | Flags directories |
| `scan --dry-run` | Same, but only list what would be flagged | No |
| `scan --full` | Re-examine every directory, ignoring the previous snapshot | Flags directories |
| `diff [--since X]` | Changes since a checkpoint, an age (`2h`, `3d`) or a timestamp | Snapshot only |
| `checkin` | Baseline for the next plain `diff` | Snapshot only |
| `checkpoint <name>` | Named baseline | Snapshot only |
| `size <path>...` | Size of any path right now; `-k` prints exactly what `du -sk` prints | No |
| `top [dir]` | Largest directories | Snapshot only |
| `candidates` | Directories named like build output, with how long unchanged | Snapshot only |
| `status` | Free space, roots, snapshots, checkpoints | No |
| `agent install` | Run `scan` every 15 minutes through launchd | Installs a LaunchAgent |

Add `--json` to `diff`, `size`, `top`, `candidates` and `status` for agents.

`size` needs no config and works on any path, monitored or not. On a flagged
directory it answers from the kernel record; elsewhere the kernel walks the
subtree, which is still several times faster than `du`.

`diff` ends with how much of the free-space change the monitored folders
account for and how much happened elsewhere on the volume (`outside_delta`
in JSON). A large "elsewhere" figure means something outside the monitored
folders grew or shrank.
Add `--no-scan` to report from the latest stored snapshot without touching the disk.

## How flagging works

- Only `scan` flags. `diff`, `checkin`, `top` and `candidates` never do.
- A monitored root is always flagged. Below it, a directory is flagged when
  the part of it not already covered by a flagged child holds at least
  `flag_min_entries` entries (default 5,000). After changing that setting,
  run `diskledger scan --full` once so unchanged directories are re-examined.
- Flagging runs bottom-up, so each file is walked once.
- An unchanged flagged directory is skipped entirely on later scans, using
  the kernel's change counter.

## Things to know

- **Flags cannot be removed.** There is no supported way to unflag a directory.
- **Moving a large unflagged directory into or out of a flagged area is slow
  the first time**, because the kernel counts it during the move.
- **Sizes are "space used", like `du`.** Cloned files count in full and a
  hardlinked file counts in only one place.
- **Very recent writes can lag** until they reach the disk.
- **The kernel interface is private to Apple** and could change in a macOS
  update. The struct layout in `src/kstat.rs` was read from `apfs.util` on
  macOS 26.5.
- `candidates` matches by directory name. It does not check whether a
  process is using the directory or who owns it.

## State

Everything is under `~/.diskledger` (override with `DISKLEDGER_HOME`):
`config`, `snapshots/*.tsv`, `checkpoints/*`. The newest `keep_snapshots`
(default 100, about a day at 15-minute scans) are kept in full; older ones
thin to one per day for 30 days. A snapshot that a checkpoint points at is
always kept. If a snapshot cannot be written, for example on a full disk, the
command still prints its result.

## Releases

Releases are cut by running the `release` workflow by hand. It picks the next
version from the existing tags (patch by default), builds the binary, and
publishes it. Tick "prerelease" for a release candidate (`vX.Y.Z-rc.N`).
The workflow writes the chosen version into `Cargo.toml` and commits it before
tagging, so `diskledger --version` always matches the tag.

To install a specific release from source:

```bash
cargo install --git https://github.com/scottopell/diskledger --tag v0.1.0 --locked
```

## License

MIT. See [LICENSE](LICENSE).
