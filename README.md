# gozjaro-installer-rs

Rust rewrite of the python `gozjaro-install` installer backend
(`Gozjaro/gozjaro-install`: `lib/actions.py` + `lib/disks.py`).

## What this is (phase 1)

- **1:1 port of the pure-logic layers** — GPT geometry, sgdisk command
  strings, device-node naming, GRUB 10_linux kernel/initrd naming,
  fstab/hosts rendering, NVRAM entry lookup, kernel pick — unit-tested on
  the host with zero disk writes (the python test suite's assertions are
  carried over).
- **Stateful driver** (`installer::Installer`) mirroring the python
  orchestration: env check → disk discovery → sgdisk partitioning →
  mkfs → mount → rsync root → kernel/config → chroot initramfs build →
  GRUB → reverse-order cleanup. Every command goes through one audited
  `exec_cmd`; `--dry-run` records without executing and returns a
  machine-readable audit trail.
- **Plain CLI front-end** (`gozjaro-install disks | plan | run`). The
  curses TUI is python-only for now; the ratatui front-end is phase 2 and
  will drive the same `Installer` API.

```sh
gozjaro-install disks                          # list candidate disks
gozjaro-install plan --disk /dev/sda --sectors 41943040   # pure: print the plan
gozjaro-install run --disk /dev/sda --tz Asia/Shanghai --dry-run   # no side effects
gozjaro-install run --disk /dev/sda --tz Asia/Shanghai --yes      # ROOT + WRITES DISKS
```

Real (`non-dry`) runs require root and an explicit `--yes`.

## Deliberate deviations from the python backend

1. `--dry-run` is a **true no-touch** run: python's dry mode still touched
   the filesystem (mkdir / write / symlink on the target tree and the
   partition-wait loop); here all host-side effects are gated on
   `!dry_run`, and the partition "did not appear" error cannot fire in
   dry mode.
2. In dry mode, empty `blkid` UUID output logs a warning instead of
   aborting (a real run still hard-fails on it, as before).
3. `check_env`'s tool list drops `python3` (obviously) — everything else
   is identical.
4. NVRAM lookup regex fixed: efibootmgr glues the current-entry `*`
   directly onto the entry name (`Boot0001*`); the python original's
   `\s+` required whitespace right after the hex digits and could never
   match real output. Rust pattern: `(Boot[0-9A-F]+)\*?\s+Gozjaro`.

## Layout

- `src/disks.rs` — geometry + sgdisk/mkfs command strings + lsblk parser
  (port of `lib/disks.py`, tests ported from `test/test_disks.py`)
- `src/state.rs` — `InstallState`, mount/label constants, kernel naming
- `src/plan.rs` — fstab/hosts/grub/NVRAM/kver pure functions
- `src/installer.rs` — the stateful driver + audited executor
- `src/main.rs` — CLI

## Cross-compiling for the live target

The binary targets `x86_64-unknown-linux-musl` (static, runs in the live
chroot without host-libc surprises):

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

Stage 77 (bootstrap) swaps the python `gozjaro-install` for this binary
once it passes the live-host acceptance test (dry-run on real hardware +
one real install on a scratch disk).
