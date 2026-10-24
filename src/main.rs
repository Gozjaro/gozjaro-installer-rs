//! gozjaro-install (rust) — CLI front-end.
//!
//! Phase 1 replaces the python backend (lib/actions.py + lib/disks.py) with
//! a plain CLI: no curses. The curses/ratatui front-end is phase 2; the
//! destructive path already requires an explicit `--yes` + root, which the
//! TUI will call through the same `Installer` API.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use gozjaro_installer::disks;
use gozjaro_installer::installer::Installer;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "gozjaro-install",
    version,
    about = "GozJaro disk installer (rust)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List candidate whole disks (lsblk, >= 1 GiB).
    Disks,
    /// Print the exact sgdisk/mkfs command plan for a disk (pure, safe).
    Plan {
        /// Whole-disk device, e.g. /dev/sda or /dev/nvme0n1.
        #[arg(long)]
        disk: String,
        /// Total sectors (blockdev --getsz units, 512-byte).
        #[arg(long)]
        sectors: u64,
    },
    /// Run the install. Without --dry-run this WRITES DISKS and requires
    /// root + an explicit --yes.
    Run {
        #[arg(long)]
        disk: String,
        #[arg(long, default_value = "gozjaro")]
        hostname: String,
        #[arg(long, default_value = "UTC")]
        tz: String,
        /// Record commands without executing.
        #[arg(long)]
        dry_run: bool,
        /// Reboot after a successful install.
        #[arg(long)]
        reboot: bool,
        /// Mandatory for real (non-dry) runs: I typed this on purpose.
        #[arg(long)]
        yes: bool,
    },
}

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Disks => {
            let mut inst = Installer::new(false);
            let found = inst.list_disks().context("lsblk failed")?;
            if found.is_empty() {
                eprintln!("no candidate disks (need >= 1 GiB whole disks)");
                return Ok(());
            }
            for d in found {
                println!(
                    "{}  {:>10.1} GiB  {}",
                    d.name,
                    d.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    d.model
                );
            }
        }
        Cmd::Plan { disk, sectors } => {
            let lay = disks::build_layout(&disk, sectors)
                .with_context(|| format!("cannot plan a GPT layout on {disk}"))?;
            for cmd in disks::all_partition_commands(&lay) {
                println!("{cmd}");
            }
            for cmd in disks::format_specs(&lay, "GOZJARO_EFI", "GOZJARO_ROOT") {
                println!("{cmd}");
            }
        }
        Cmd::Run {
            disk,
            hostname,
            tz,
            dry_run,
            reboot,
            yes,
        } => {
            if !dry_run {
                let uid = unsafe { libc::getuid() };
                if uid != 0 {
                    anyhow::bail!("real runs require root (uid {uid})");
                }
                if !yes {
                    anyhow::bail!(
                        "refusing to write {disk} without --yes \
                         (this destroys the disk; add --dry-run to preview)"
                    );
                }
            }
            let mut inst = Installer::new(dry_run);
            inst.check_env().context("environment check")?;
            let _ = inst.list_disks(); // fill state.disks for the audit log
            let total = inst
                .disk_total_sectors(&disk)
                .with_context(|| format!("cannot size {disk}"))?;
            inst.state.disk = disk.clone();
            inst.state.total_sectors = total;
            inst.run_partitioning()
                .with_context(|| format!("partitioning {disk}"))?;
            inst.run_post_partition(&hostname, &tz, reboot)
                .with_context(|| "post-partition steps")?;
            if dry_run {
                println!("dry-run audit trail:\n{}", inst.audit_trail());
            }
            for line in &inst.log {
                eprintln!("[{}] {}", line.kind, line.text);
            }
        }
    }
    Ok(())
}
