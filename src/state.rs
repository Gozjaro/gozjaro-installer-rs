//! Install state + mount layout. Port of `InstallState` in
//! gozjaro-install's `lib/actions.py`.

use crate::disks::DiskInfo;

// Mount layout (fixed, documented, always cleaned up in reverse order).
pub const ROOT_MOUNT: &str = "/mnt/install";
pub const ISO_MOUNT: &str = "/mnt/iso";
pub const SRC_MOUNT: &str = "/mnt/src";

pub const LIVE_LABEL: &str = "GOZJARO_LIVE";
pub const ESP_LABEL: &str = "GOZJARO_EFI";
pub const ROOT_LABEL: &str = "GOZJARO_ROOT";

/// Kernel/initramfs naming. GRUB 2.14's 10_linux discovers kernels via the
/// glob /boot/vmlinuz-* and derives the version as the basename with the
/// leading "vmlinuz-" stripped (sed 's,^[^0-9]*-,,g'), then finds the
/// initramfs as candidate #2 "initrd-${version}.img". The kver we read from
/// the target's /lib/modules is the BARE kernelrelease (e.g. "7.1.8"), so we
/// stage:
///
/// ```text
/// /boot/vmlinuz-<kver>           matches the vmlinuz-* glob (version=<kver>)
/// /boot/initrd-<kver>.img       = initrd-${version}.img, candidate #2
/// ```
/// Bare names ("vmlinuz"/"initrd.img") match NO 10_linux glob — that is the
/// exact bug that produced an empty boot menu on the first installed boot.
/// Idempotent: kver is used verbatim, so passing "7.1.8" never yields a
/// bare "vmlinuz".
pub fn kernel_name(kver: &str) -> String {
    format!("vmlinuz-{kver}")
}

pub fn initrd_name(kver: &str) -> String {
    format!("initrd-{kver}.img")
}

#[derive(Debug, Default, Clone)]
pub struct InstallState {
    pub is_uefi: bool,
    /// Candidate whole disks: [{name, size_bytes, model}]
    pub disks: Vec<DiskInfo>,
    /// Whole disk chosen by the user.
    pub disk: String,
    pub total_sectors: u64,
    pub esp_dev: String,
    pub root_dev: String,
    pub root_uuid: String,
    pub esp_uuid: String,
    pub kver: String,
    pub hostname: String,
    pub tz: String,
    /// Populated by check_env().
    pub src: String,
    pub iso: String,
}

impl InstallState {
    pub fn new() -> Self {
        Self {
            hostname: "gozjaro".into(),
            tz: "UTC".into(),
            ..Default::default()
        }
    }

    /// Plan is drawable: a whole disk with a positive sector count.
    pub fn plan_ok(&self) -> bool {
        !self.disk.is_empty() && self.total_sectors > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let s = InstallState::new();
        assert_eq!(s.hostname, "gozjaro");
        assert_eq!(s.tz, "UTC");
        assert!(!s.plan_ok());
    }

    #[test]
    fn kernel_naming_matches_grub_10_linux_globs() {
        assert_eq!(kernel_name("7.1.8"), "vmlinuz-7.1.8");
        assert_eq!(initrd_name("7.1.8"), "initrd-7.1.8.img");
        // the bare names the old ISO shipped must NOT be produced
        assert_ne!(kernel_name("7.1.8"), "vmlinuz");
        assert_ne!(initrd_name("7.1.8"), "initrd.img");
    }

    #[test]
    fn plan_ok_requires_disk_and_sectors() {
        let mut s = InstallState::new();
        assert!(!s.plan_ok());
        s.disk = "/dev/sda".into();
        assert!(!s.plan_ok());
        s.total_sectors = 41_943_040;
        assert!(s.plan_ok());
    }
}
