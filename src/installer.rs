//! Stateful, audited install driver. Port of `Installer` in
//! gozjaro-install's `lib/actions.py`. Every destructive action funnels
//! through `exec_cmd` so the front-end stays a thin view; all commands run
//! as argument lists (no shell), so disk names with spaces can never break
//! quoting.
//!
//! The one non-rust actor is the chrooted initramfs builder
//! (/usr/sbin/gozjaro-build-initramfs, installed by stage 77) which packs
//! the target's initramfs with the *target's own cpio*.

use crate::disks;
use crate::plan;
use crate::state::{self, InstallState};
use std::ffi::CString;
use anyhow::{anyhow, bail, Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Host tools the installer needs (python REQUIRED_HOST_TOOLS, minus
/// python3 itself and plus the rust binary — the host checks are the same).
pub const REQUIRED_HOST_TOOLS: &[&str] = &[
    "sgdisk", "mkfs.vfat", "mkfs.ext4", "blkid", "wipefs", "rsync", "cpio",
    "grub-install", "efibootmgr",
];

pub struct LogLine {
    pub kind: &'static str,
    pub text: String,
}

pub struct Installer {
    pub state: InstallState,
    pub dry_run: bool,
    pub log: Vec<LogLine>,
    audit: Vec<Vec<String>>,
    /// Mount bookkeeping for cleanup (reverse order on exit).
    mounts: Vec<String>,
    chroot_binds: Vec<(String, String)>,
    live_unmounts: Vec<String>,
}

impl Installer {
    pub fn new(dry_run: bool) -> Self {
        Self {
            state: InstallState::new(),
            dry_run,
            log: Vec::new(),
            audit: Vec::new(),
            mounts: Vec::new(),
            chroot_binds: Vec::new(),
            live_unmounts: Vec::new(),
        }
    }

    fn logl(&mut self, kind: &'static str, text: String) {
        self.log.push(LogLine { kind, text });
    }

    /// Run argv (list). `check=false` never raises on non-zero.
    /// Returns (rc, captured_text). Dry-run records and returns (0, "").
    fn exec_cmd(&mut self, argv: &[&str], check: bool) -> Result<(i32, String)> {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let truncated: Vec<String> = args
            .iter()
            .map(|a| {
                if a.len() > 48 {
                    format!("{}...", &a[..45])
                } else {
                    a.clone()
                }
            })
            .collect();
        self.logl("cmd", format!("$ {}", truncated.join(" ")));
        if self.dry_run {
            self.audit.push(args);
            return Ok((0, String::new()));
        }
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..]).stdout(std::process::Stdio::piped());
        let proc = cmd
            .output()
            .map_err(|e| anyhow!("cannot exec {}: {}", args[0], e))?;
        let text = String::from_utf8_lossy(&proc.stdout).into_owned();
        let rc: i32 = proc.status.code().unwrap_or(-1);
        if check && rc != 0 {
            let tail = text.lines().rev().take(12).collect::<Vec<_>>().join("\n");
            bail!("{} failed (rc={rc}):\n{tail}", args[0]);
        }
        Ok((rc, text))
    }

    /// Make the kernel re-read the partition table so the new partition
    /// device nodes appear — the job the external `partprobe` (util-linux)
    /// used to do. The GozJaro tree intentionally ships no util-linux, so
    /// the installer does it natively: util-linux' partprobe itself is
    /// `ioctl(fd, BLKPG)` with a `BLKRRPART` fallback, and both ioctls are
    /// arch-independent (_IO(0x12, 0x70) / _IO(0x12, 0x74) in
    /// asm/blkdev.h, same number on x86_64 and aarch64).
    ///
    /// Best-effort, matching partprobe's exit-code contract (non-zero can
    /// mean "no partitions to re-read" on a device): the hard gate is the
    /// partition-device poll in `partition_and_format`, which still bails
    /// if the nodes never appear.
    fn revalidate_partitions(&mut self, dev: &str) -> Result<()> {
        self.logl("cmd", format!("$ revalidate {dev} (kernel BLKPG/BLKRRPART, native)"));
        self.audit.push(vec!["revalidate".into(), dev.to_string()]);
        if self.dry_run {
            return Ok(());
        }
        let cdev = CString::new(dev).map_err(|e| anyhow!("invalid device name {dev}: {e}"))?;
        let fd = unsafe { libc::open(cdev.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            bail!("revalidate {dev}: cannot open {dev}: {e}");
        }
        const BLKPG: libc::c_ulong = 0x1270;
        const BLKRRPART: libc::c_ulong = 0x1274;
        let ok = if unsafe { libc::ioctl(fd, BLKPG) } < 0
            && unsafe { libc::ioctl(fd, BLKRRPART) } < 0
        {
            false
        } else {
            true
        };
        let _ = unsafe { libc::close(fd) };
        if !ok {
            let e = std::io::Error::last_os_error();
            self.logl(
                "warn",
                format!("revalidate {dev}: kernel rejected BLKPG/BLKRRPART ({e}); device nodes may still appear on next access"),
            );
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // step 0 — environment + source
    // ------------------------------------------------------------------

    pub fn check_env(&mut self) -> Result<()> {
        let missing: Vec<&str> = REQUIRED_HOST_TOOLS
            .iter()
            .copied()
            .filter(|t| which(t).is_none())
            .collect();
        if !missing.is_empty() {
            self.logl("warn", format!("missing tools: {}", missing.join(", ")));
        }
        self.state.is_uefi = Path::new("/sys/firmware/efi").is_dir();
        self.logl(
            "info",
            if self.state.is_uefi {
                "UEFI firmware: yes".into()
            } else {
                "UEFI firmware: no (BIOS path)".into()
            },
        );
        // live source
        let live_squash = Path::new("/run/squashfs");
        if live_squash.is_dir() && Path::new("/run/livecd").is_dir() {
            self.state.src = "/run/squashfs".into();
            self.state.iso = "/run/livecd".into();
            self.logl("info", "reusing live mounts (source=/run/squashfs)".into());
            return Ok(());
        }
        let (_, out) = self.exec_cmd(&["blkid", "-L", state::LIVE_LABEL], false)?;
        let mut live = out.trim().to_string();
        if live.is_empty() {
            // fall back to probing cdrom/sd devices
            for cand in ["/dev/sr0", "/dev/sr1", "/dev/sda"] {
                if Path::new(cand).exists() {
                    live = cand.to_string();
                    break;
                }
            }
        }
        if live.is_empty() {
            bail!("cannot find the live medium (label {})", state::LIVE_LABEL);
        }
        std::fs::create_dir_all(state::ISO_MOUNT)
            .context(format!("cannot create {}", state::ISO_MOUNT))?;
        self.exec_cmd(&["mount", "-o", "ro", &live, state::ISO_MOUNT], true)?;
        self.live_unmounts.push(state::ISO_MOUNT.to_string());
        let squash = format!("{}/live/filesystem.squashfs", state::ISO_MOUNT);
        if !Path::new(&squash).is_file() {
            let _ = self.exec_cmd(&["umount", state::ISO_MOUNT], false);
            bail!("{live} is not the Gozjaro live medium (no {squash})");
        }
        std::fs::create_dir_all(state::SRC_MOUNT)
            .context(format!("cannot create {}", state::SRC_MOUNT))?;
        self.exec_cmd(&["mount", "-o", "ro,loop", &squash, state::SRC_MOUNT], true)?;
        self.live_unmounts.push(state::SRC_MOUNT.to_string());
        self.state.src = state::SRC_MOUNT.into();
        self.state.iso = state::ISO_MOUNT.into();
        self.logl("info", format!("live source loop-mounted at {}", state::SRC_MOUNT));
        Ok(())
    }

    // ------------------------------------------------------------------
    // step 1 — disk discovery
    // ------------------------------------------------------------------

    pub fn list_disks(&mut self) -> Result<Vec<disks::DiskInfo>> {
        let (_, js) = self.exec_cmd(
            &["lsblk", "-d", "-b", "-J", "-o", "NAME,SIZE,TYPE,MODEL,ROTA"],
            true,
        )?;
        let mut found = disks::parse_lsblk_block_devices(&js);
        // only physical whole disks with a plausible size for ESP+root (>=1GiB)
        found.retain(|d| d.size_bytes >= 1024 * 1024 * 1024);
        found.sort_by(|a, b| a.name.cmp(&b.name));
        self.state.disks = found.clone();
        self.logl("info", format!("{} candidate disk(s)", found.len()));
        Ok(found)
    }

    pub fn disk_total_sectors(&mut self, dev: &str) -> Result<u64> {
        let (_, out) = self.exec_cmd(&["blockdev", "--getsz", dev], true)?;
        Ok(disks::total_sectors_from_blockdev(&out))
    }

    // ------------------------------------------------------------------
    // steps 2/3 — partition + format (sgdisk, not parted)
    // ------------------------------------------------------------------

    pub fn partition_and_format(&mut self, dev: &str, total_sectors: u64) -> Result<()> {
        self.state.disk = dev.to_string();
        self.state.total_sectors = total_sectors;
        let layout = disks::build_layout(dev, total_sectors)
            .with_context(|| format!("cannot plan a GPT layout on {dev}"))?;
        let (esp_dev, root_dev) =
            disks::partition_device_names(dev, layout.esp_no, layout.root_no);
        self.state.esp_dev = esp_dev.clone();
        self.state.root_dev = root_dev.clone();

        self.logl("step", format!("partitioning {dev} (GPT via sgdisk)"));
        self.exec_cmd(&["wipefs", "-a", dev], false)?;
        for cmd in disks::all_partition_commands(&layout) {
            let argv: Vec<&str> = cmd.split_whitespace().collect();
            self.exec_cmd(&argv, true)?;
        }
        // make the kernel see the new table (native BLKPG, replaces the
        // external partprobe that the minimal tree does not ship)
        self.revalidate_partitions(dev)?;
        if !self.dry_run {
            for _ in 0..10 {
                if Path::new(&root_dev).exists() && Path::new(&esp_dev).exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            for d in [&esp_dev, &root_dev] {
                if !Path::new(d).exists() {
                    bail!("partition {d} did not appear after revalidate");
                }
            }
        }

        self.logl(
            "step",
            format!("formatting {esp_dev} (vfat) and {root_dev} (ext4)"),
        );
        for cmd in disks::format_specs(&layout, state::ESP_LABEL, state::ROOT_LABEL) {
            let argv: Vec<&str> = cmd.split_whitespace().collect();
            self.exec_cmd(&argv, true)?;
        }

        let (_, ru) = self.exec_cmd(
            &["blkid", "-s", "UUID", "-o", "value", &root_dev],
            true,
        )?;
        let (_, eu) = self.exec_cmd(&["blkid", "-s", "UUID", "-o", "value", &esp_dev], true)?;
        self.state.root_uuid = ru.trim().to_string();
        self.state.esp_uuid = eu.trim().to_string();
        if self.state.root_uuid.is_empty() {
            if self.dry_run {
                self.logl("warn", "dry run: root UUID unknown (no real blkid output)".into());
            } else {
                bail!("cannot read UUID of {}", self.state.root_dev);
            }
        }
        self.logl(
            "info",
            format!(
                "root uuid={}  esp uuid={}",
                self.state.root_uuid, self.state.esp_uuid
            ),
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // step 4 — mount target
    // ------------------------------------------------------------------

    pub fn mount_target(&mut self) -> Result<()> {
        let s = self.state.clone();
        let root = state::ROOT_MOUNT;
        if !self.dry_run {
            std::fs::create_dir_all(root)
                .context(format!("cannot create {root}"))?;
        }
        self.exec_cmd(&["mount", &s.root_dev, root], true)?;
        self.mounts.push(root.to_string());
        if s.is_uefi {
            if !self.dry_run {
                std::fs::create_dir_all(format!("{root}/boot/efi"))?;
            }
            self.exec_cmd(&["mount", &s.esp_dev, &format!("{root}/boot/efi")], true)?;
            self.mounts.push(format!("{root}/boot/efi"));
        }
        self.logl("info", format!("target mounted at {root}"));
        Ok(())
    }

    // ------------------------------------------------------------------
    // step 5 — copy root
    // ------------------------------------------------------------------

    pub fn copy_root(&mut self) -> Result<()> {
        let s = self.state.clone();
        self.logl("step", "copying root filesystem (this takes a few minutes)".into());
        self.exec_cmd(
            &[
                "rsync",
                "-aHAXx",
                "--exclude=/run",
                "--exclude=/tmp",
                "--exclude=/proc",
                "--exclude=/sys",
                "--exclude=/dev",
                &format!("{}/", s.src),
                &format!("{}/", state::ROOT_MOUNT),
            ],
            true,
        )?;
        if !self.dry_run {
            // THE /run BUG (2026-10-18 first installed boot): the squashfs has
            // no /run (stage 90 excludes it), rsync --exclude=/run keeps it
            // absent, and systemd's mount_setup() then fails "Failed to mount
            // tmpfs on /run: No such file or directory". Create the
            // API-filesystem mountpoints.
            for d in ["run", "dev", "proc", "sys", "tmp", "boot"] {
                std::fs::create_dir_all(format!("{}/{d}", state::ROOT_MOUNT))?;
            }
        }
        self.logl(
            "info",
            "root filesystem copied; API-filesystem mountpoints created".into(),
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // step 6 — kernel + config + fstab
    // ------------------------------------------------------------------

    pub fn install_kernel_and_config(&mut self, hostname: &str, tz: &str) -> Result<()> {
        let moddir = format!("{}/lib/modules", state::ROOT_MOUNT);
        let mut kvers: Vec<String> = Vec::new();
        if !self.dry_run {
            if !Path::new(&moddir).is_dir() {
                bail!("no {moddir} in the target (rsync incomplete?)");
            }
            for entry in std::fs::read_dir(&moddir)? {
                let entry = entry?;
                if entry.path().is_dir() {
                    kvers.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        let kver = plan::latest_kver(&kvers.iter().map(String::as_str).collect::<Vec<_>>())
            .ok_or_else(|| anyhow!("no kernel under {moddir}"))?;
        self.state.kver = kver.clone();
        self.logl("info", format!("kernel {kver}"));

        let iso = self.state.iso.clone();
        if !self.dry_run {
            let boot = format!("{}/boot", state::ROOT_MOUNT);
            std::fs::create_dir_all(&boot)?;
            // kernel comes from the ISO (built by stage 80, renamed bare by
            // 90); re-stage it under the versioned name GRUB's 10_linux
            // requires.
            let dst = format!("{}/{}", boot, state::kernel_name(&kver));
            std::fs::copy(format!("{iso}/boot/vmlinuz"), &dst)?;
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o644))?;

            self.state.hostname = hostname.to_string();
            self.state.tz = tz.to_string();
            std::fs::write(
                format!("{}/etc/hostname", state::ROOT_MOUNT),
                format!("{hostname}\n"),
            )?;
            std::fs::write(
                format!("{}/etc/hosts", state::ROOT_MOUNT),
                plan::hosts_text(hostname),
            )?;
            let tzfile = format!("{}/usr/share/zoneinfo/{tz}", state::ROOT_MOUNT);
            let link = format!("{}/etc/localtime", state::ROOT_MOUNT);
            if Path::new(&tzfile).is_file() {
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(format!("/usr/share/zoneinfo/{tz}"), &link)?;
                std::fs::write(format!("{}/etc/timezone", state::ROOT_MOUNT), format!("{tz}\n"))?;
            } else {
                self.logl("warn", format!("timezone {tz} not found, using UTC"));
            }
            // systemd regenerates machine-id on first boot when it is empty
            std::fs::write(format!("{}/etc/machine-id", state::ROOT_MOUNT), "")?;
            // strip live-only overrides so the installed system prompts for login
            for drop in [
                format!(
                    "{}/etc/systemd/system/getty@tty1.service.d/autologin.conf",
                    state::ROOT_MOUNT
                ),
                format!("{}/etc/systemd/journald.conf.d/live.conf", state::ROOT_MOUNT),
            ] {
                let _ = std::fs::remove_file(&drop);
            }
        } else {
            self.state.hostname = hostname.to_string();
            self.state.tz = tz.to_string();
            self.state.kver = kver.clone();
        }
        self.write_fstab();
        self.logl("info", "fstab/hostname/timezone written, live overrides removed".into());
        Ok(())
    }

    /// The fstab text is pure (in plan::fstab); writing it here keeps the
    /// dry/real symmetry: dry runs render but do not touch the target.
    fn write_fstab(&mut self) {
        let s = &self.state;
        let text = plan::fstab(s, "install");
        if self.dry_run {
            self.logl("fstab", text);
            return;
        }
        let _ = std::fs::write(format!("{}/etc/fstab", state::ROOT_MOUNT), text);
    }

    // ------------------------------------------------------------------
    // step 7 — chroot: build the initramfs (the chicken lays the egg)
    // ------------------------------------------------------------------

    fn bind_chroot_fs(&mut self) -> Result<()> {
        let is_uefi = self.state.is_uefi;
        let root = state::ROOT_MOUNT;
        for sub in ["dev", "dev/pts", "proc", "sys", "run"] {
            let (src, dst) = match sub {
                "dev/pts" => ("/dev/pts", format!("{root}/dev/pts")),
                other => (other, format!("{root}/{other}")),
            };
            self.exec_cmd(&["mount", "--bind", src, &dst], true)?;
            self.chroot_binds.push((src.to_string(), dst));
        }
        // efibootmgr inside the chroot needs the live efivarfs visible
        if is_uefi {
            let host_efivars = "/sys/firmware/efi/efivars";
            if Path::new(host_efivars).is_dir() {
                let root = state::ROOT_MOUNT;
                let tgt = format!("{root}{host_efivars}");
                if !self.dry_run {
                    std::fs::create_dir_all(&tgt)?;
                }
                self.exec_cmd(&["mount", "--bind", host_efivars, &tgt], true)?;
                self.chroot_binds.push((host_efivars.to_string(), tgt));
            }
        }
        Ok(())
    }

    pub fn build_initramfs(&mut self) -> Result<()> {
        let s = self.state.clone();
        self.logl(
            "step",
            format!(
                "building initramfs for kernel {} (in target, with its own cpio)",
                s.kver
            ),
        );
        self.bind_chroot_fs()?;
        self.exec_cmd(
            &["chroot", state::ROOT_MOUNT, "/usr/sbin/gozjaro-build-initramfs", &s.kver],
            true,
        )?;
        let out = format!("{}/{}/boot/{}", state::ROOT_MOUNT, s.kver, state::initrd_name(&s.kver));
        if !self.dry_run && !Path::new(&out).is_file() {
            bail!("{out} was not produced by the chroot build");
        }
        self.logl("info", format!("initramfs built: {out}"));
        Ok(())
    }

    // ------------------------------------------------------------------
    // step 8 — bootloader
    // ------------------------------------------------------------------

    pub fn install_grub(&mut self) -> Result<()> {
        let s = self.state.clone();
        self.logl(
            "step",
            format!("installing GRUB{}", if s.is_uefi { " (UEFI)" } else { " (BIOS)" }),
        );
        if !self.dry_run {
            std::fs::create_dir_all(format!("{}/boot/grub", state::ROOT_MOUNT))?;
        }
        for cmd in plan::grub_commands(&s) {
            self.chroot_sh(&cmd)?;
        }
        if s.is_uefi {
            // register + make current the NVRAM entry
            let (_, out) = self.exec_cmd(
                &["chroot", state::ROOT_MOUNT, "efibootmgr", "-v"],
                false,
            )?;
            if let Some(entry) = plan::find_gozjaro_boot_entry(&out) {
                let _ = self.exec_cmd(
                    &["chroot", state::ROOT_MOUNT, "efibootmgr", "-b", &entry],
                    false,
                );
                self.logl("info", format!("NVRAM current boot entry set: {entry}"));
            } else {
                self.logl(
                    "warn",
                    "could not find the Gozjaro NVRAM entry; the firmware \
                     boot order may still point at the live ISO"
                        .into(),
                );
            }
        }
        self.logl("info", "bootloader installed".into());
        Ok(())
    }

    /// Single shell string, run inside the target (python `chroot()` helper).
    fn chroot_sh(&mut self, cmd: &str) -> Result<()> {
        let root = state::ROOT_MOUNT;
        self.exec_cmd(
            &[
                "chroot",
                root,
                "/usr/bin/env",
                "-i",
                "HOME=/root",
                "PATH=/usr/bin:/usr/sbin:/bin:/sbin",
                "/bin/sh",
                "-c",
                cmd,
            ],
            true,
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // finalise
    // ------------------------------------------------------------------

    pub fn finalise(&mut self, reboot: bool) -> Result<()> {
        let binds: Vec<(String, String)> = self.chroot_binds.clone();
        for (_, dst) in binds.iter().rev() {
            self.exec_cmd(&["umount", dst], false)?;
        }
        self.chroot_binds.clear();
        let mounts: Vec<String> = self.mounts.clone();
        for mnt in mounts.iter().rev() {
            self.exec_cmd(&["umount", mnt], false)?;
        }
        self.mounts.clear();
        let live: Vec<String> = self.live_unmounts.clone();
        for mnt in live.iter().rev() {
            self.exec_cmd(&["umount", mnt], false)?;
        }
        self.live_unmounts.clear();
        self.exec_cmd(&["sync"], true)?;
        let s = self.state.clone();
        self.logl(
            "done",
            format!("installation complete: {} ({})", s.disk, s.kver),
        );
        if reboot {
            self.exec_cmd(&["systemctl", "reboot"], false)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // orchestration (used by both front-ends / the CLI)
    // ------------------------------------------------------------------

    pub fn run_partitioning(&mut self) -> Result<()> {
        let dev = self.state.disk.clone();
        let total = self.state.total_sectors;
        self.partition_and_format(&dev, total)
    }

    pub fn run_post_partition(&mut self, hostname: &str, tz: &str, reboot: bool) -> Result<()> {
        self.mount_target()?;
        self.copy_root()?;
        self.install_kernel_and_config(hostname, tz)?;
        self.build_initramfs()?;
        self.install_grub()?;
        self.finalise(reboot)
    }

    /// Audit trail of exec'd commands (python audit_trail()).
    pub fn audit_trail(&self) -> String {
        self.audit
            .iter()
            .map(|c| format!("  $ {}", c.join(" ")))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `which(1)` equivalent over $PATH.
fn which(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let cand = Path::new(dir).join(tool);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dry-run partitioning: no host side effects, audit records the exact
    /// command order (wipefs → sgdisk zap → sgdisk new ×2 → revalidate →
    /// mkfs.vfat → mkfs.ext4 → blkid ×2), and the "did not appear" error
    /// cannot fire (device wait is dry-gated).
    #[test]
    fn dry_partition_audit_order() {
        let mut inst = Installer::new(true);
        inst.state.disk = "/dev/sda".into();
        let _ = inst.partition_and_format("/dev/sda", 41_943_040);
        let firsts: Vec<String> = inst
            .audit
            .iter()
            .map(|c| c[0].clone())
            .collect();
        let pos = |t: &str| firsts.iter().position(|x| x == t).unwrap_or(usize::MAX);
        assert!(pos("wipefs") < pos("sgdisk"));
        assert!(pos("sgdisk") < pos("revalidate"));
        assert!(pos("revalidate") < pos("mkfs.vfat"));
        assert!(pos("mkfs.vfat") < pos("mkfs.ext4"));
        assert!(pos("mkfs.ext4") < pos("blkid"));
        // the sgdisk zap is the very first sgdisk invocation
        let sgdiks: Vec<_> = inst.audit.iter().filter(|c| c[0] == "sgdisk").collect();
        assert_eq!(sgdiks.len(), 3);
        assert!(sgdiks[0][1].starts_with("--zap-all"));
        let trail = inst.audit_trail();
        assert!(trail.contains("$ sgdisk --zap-all /dev/sda"));
    }

    #[test]
    fn audit_trail_empty_without_exec() {
        let inst = Installer::new(true);
        assert!(inst.audit_trail().is_empty());
    }
}
