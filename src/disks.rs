//! Pure, testable disk-layout logic. 1:1 port of gozjaro-install's
//! `lib/disks.py` — nothing here touches a running system: it builds
//! sgdisk *command strings* and filesystem specs that the executor runs.
//!
//! Partition plan (GPT, x86_64 UEFI; legacy-BIOS boot via a BIOS Boot
//! partition — grub-install(i386-pc) refuses blocklists, and GPT leaves no
//! embedding gap, so BIOS boot needs an unformatted 2 MiB bios_grub part):
//!
//! ```text
//! p1  biosboot  1 MiB .. 3 MiB     (raw)     GUID 21686148-... (BIOS Boot)
//! p2  ESP       3 MiB .. 515 MiB    FAT32    GUID C12A7328-... (EFI)   LegacyBIOSBootable
//! p3  root      515 MiB .. end      ext4     GUID 0FC63DAF-... (Linux)
//! ```
//!
//! Why sgdisk and not parted: the live image's parted 3.6 segfaults in its
//! geometry-probe/exit path on some virtual SCSI disks; sgdisk is robust,
//! scriptable, and is what LFS ships.

use anyhow::{anyhow, bail, Result};
#[cfg(target_os = "linux")]
use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

/// GPT partition type GUIDs (upper-case, canonical).
///
/// BBP_GUID is the *GRUB* BIOS-Boot type, NOT the "well-known" Microsoft
/// `21686148-6449-4447-...` one. GRUB's `gpt_partition_map_embed`
/// (grub-core/partmap/gpt.c) matches a single constant,
/// `GRUB_GPT_PARTITION_TYPE_BIOS_BOOT` =
/// `21686148-6449-6E6F-744E-656564454649` — the bytes spell "Hah!IdOn
/// TNEEDEFI". Verified empirically against a grub 2.14 install: with the
/// Microsoft GUID (or the Debian biosboot `E3C9E312-...` alias)
/// `grub-install --target=i386-pc` reports "this GPT partition label
/// contains no BIOS Boot Partition; embedding won't be possible" and
/// refuses blocklists. Only the GRUB GUID is found, after which
/// "Installation finished. No error reported."
pub const ESP_GUID: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"; // EFI System
pub const LINUX_GUID: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"; // Linux filesystem
pub const BBP_GUID: &str = "21686148-6449-6E6F-744E-656564454649";  // BIOS Boot (GRUB's type)

pub const SECTOR: u64 = 512;
pub const BBP_START_SECTOR: u64 = 2048; // 1 MiB
pub const BBP_SIZE_MIB: u64 = 2;        // 2 MiB — the i386-pc embedding area
pub const ESP_START_SECTOR: u64 =
    BBP_START_SECTOR + BBP_SIZE_MIB * 1024 * 1024 / SECTOR; // 3 MiB
pub const ESP_SIZE_MIB: u64 = 512;
pub const GPT_TAIL_RESERVE: u64 = 34; // no partition may use the 33-sector
                                       // backup-GPT block (entry array + header)
                                       // at [total-33 .. total-1]; sgdisk
                                       // rejects a partition ending inside it,
                                       // so the last partition sector is total-34

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GptLayout {
    pub disk: String,
    pub bbp_no: u32,
    pub esp_no: u32,
    pub root_no: u32,
    pub bbp_start: u64,
    pub bbp_end: u64,
    pub esp_start: u64,
    pub esp_end: u64,
    pub root_start: u64,
    pub root_end: u64,
    pub bbp_guid: &'static str,
    pub esp_guid: &'static str,
    pub root_guid: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiskInfo {
    pub name: String,
    pub size_bytes: u64,
    pub model: String,
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Inclusive end sector of the BIOS Boot partition (always the first one).
pub fn bbp_end_sector() -> u64 {
    BBP_START_SECTOR + BBP_SIZE_MIB * 1024 * 1024 / SECTOR - 1
}

/// Inclusive end sector of the ESP given its size in MiB.
pub fn esp_end_sector(esp_mib: u64) -> u64 {
    ESP_START_SECTOR + esp_mib * 1024 * 1024 / SECTOR - 1
}

pub fn root_start_sector(esp_mib: u64) -> u64 {
    esp_end_sector(esp_mib) + 1
}

/// Last usable sector for the root partition. The GPT reserves a 33-sector
/// backup block at the tail; sgdisk rejects a partition whose last sector
/// lands in that block, so a partition must end at or before total-34.
pub fn root_end_sector(total_sectors: u64) -> Result<u64> {
    if total_sectors == 0 {
        bail!("total_sectors must be positive");
    }
    let usable = total_sectors
        .checked_sub(GPT_TAIL_RESERVE)
        .ok_or_else(|| anyhow!("disk too small for a GPT layout"))?;
    if usable == 0 {
        bail!("disk too small for a GPT layout");
    }
    Ok(usable)
}

pub fn build_layout(disk: &str, total_sectors: u64) -> Result<GptLayout> {
    let layout = GptLayout {
        disk: disk.to_string(),
        bbp_no: 1,
        esp_no: 2,
        root_no: 3,
        bbp_start: BBP_START_SECTOR,
        bbp_end: bbp_end_sector(),
        esp_start: ESP_START_SECTOR,
        esp_end: esp_end_sector(ESP_SIZE_MIB),
        root_start: root_start_sector(ESP_SIZE_MIB),
        root_end: root_end_sector(total_sectors)?,
        bbp_guid: BBP_GUID,
        esp_guid: ESP_GUID,
        root_guid: LINUX_GUID,
    };
    // Hard invariants: every partition must sit inside the disk and the root
    // must have at least one sector.
    if layout.bbp_end >= total_sectors {
        bail!("disk too small: {total_sectors} sectors cannot hold the {} MiB BIOS Boot partition (ends at sector {})", BBP_SIZE_MIB, layout.bbp_end);
    }
    if layout.esp_end >= total_sectors {
        bail!(
            "disk too small: {total_sectors} sectors cannot hold the \
             512 MiB ESP (ends at sector {})",
            layout.esp_end
        );
    }
    if layout.root_end < layout.root_start {
        bail!(
            "disk too small: no room left for the root partition \
             (need at least sector {}, got total {total_sectors})",
            layout.root_start
        );
    }
    Ok(layout)
}

/// Smallest disk (sectors) that fits the ESP + a min-size root.
pub fn min_disk_sectors(esp_mib: u64, min_root_mib: u64) -> u64 {
    root_start_sector(esp_mib) + min_root_mib * 1024 * 1024 / SECTOR
}

// ---------------------------------------------------------------------------
// sgdisk command generation (pure strings — the executor runs them)
// ---------------------------------------------------------------------------

/// `sgdisk --zap-all` wipes the GPT headers, the partition table and the
/// protective MBR signature. A GPT (and its protective MBR) is re-created
/// automatically when the first `--new` runs on an unpartitioned device, so
/// no separate protective-MBR step is needed (one before any GPT exists
/// would fail).
pub fn zap_commands(disk: &str) -> Vec<String> {
    vec![format!("sgdisk --zap-all {disk}")]
}

/// Create p1 (ESP) and p2 (root). sgdisk `--new=no:start:end` is inclusive
/// on both ends; the GUIDs are spelled explicitly so the commands are
/// self-documenting and independent of sgdisk's keyword table.
///
/// Every property is applied in its OWN sgdisk invocation: the sgdisk in
/// the GozJaro image does not commit when `--new` and `--type`/`--set-boot`
/// ride on the same invocation (it exits 0 and writes nothing), whereas
/// plain `--new` and the single-property `-t`/`-c`/`-b` forms each commit
/// reliably. Verified against the live image's sgdisk build.
pub fn partition_commands(l: &GptLayout) -> Vec<String> {
    let d = l.disk.as_str();
    vec![
        // create all three partitions first
        format!(
            "sgdisk --new={no}:{start}:{end} {d}",
            no = l.bbp_no,
            start = l.bbp_start,
            end = l.bbp_end
        ),
        format!(
            "sgdisk --new={no}:{start}:{end} {d}",
            no = l.esp_no,
            start = l.esp_start,
            end = l.esp_end
        ),
        format!(
            "sgdisk --new={no}:{start}:{end} {d}",
            no = l.root_no,
            start = l.root_start,
            end = l.root_end
        ),
        // then attach the properties, one invocation each
        format!(
            "sgdisk -t {no}:{guid} {d}",
            no = l.bbp_no,
            guid = l.bbp_guid
        ),
        // no space in the name: sgdisk -c takes one shell word
        format!("sgdisk -c {no}:BIOSBoot {d}", no = l.bbp_no),
        format!(
            "sgdisk -t {no}:{guid} {d}",
            no = l.esp_no,
            guid = l.esp_guid
        ),
        format!("sgdisk -c {no}:ESP {d}", no = l.esp_no),
        format!("sgdisk -b {no} {d}", no = l.esp_no), // legacy BIOS bootable
        format!(
            "sgdisk -t {no}:{guid} {d}",
            no = l.root_no,
            guid = l.root_guid
        ),
        format!("sgdisk -c {no}:root {d}", no = l.root_no),
    ]
}

/// FAT32 ESP + ext4 root with the given volume labels; the BIOS Boot
/// partition is left unformatted on purpose (grub's i386-pc embedding area).
/// ESP device node: nvme/mmcblk use p-suffixed minors, sd* don't.
pub fn format_specs(l: &GptLayout, esp_label: &str, root_label: &str) -> Vec<String> {
    let (esp_dev, root_dev) = partition_device_names(&l.disk, l.esp_no, l.root_no);
    vec![
        format!("mkfs.vfat -F32 -n {esp_label} {esp_dev}"),
        format!("mkfs.ext4 -L {root_label} -F {root_dev}"),
    ]
}

/// Map a whole-disk name + partition numbers to device nodes.
/// nvmeXnY and mmcblkX take a `p` between the whole-disk name and the minor
/// (nvme0n1p1); SCSI/SATA/USB (sdX) do not (sda1).
pub fn partition_device_names(disk: &str, esp_no: u32, root_no: u32) -> (String, String) {
    // Matches whole-disk names only: /dev/nvme0n1 -> yes; /dev/nvme0n1p1 -> no.
    let re = regex::Regex::new(r"(nvme[0-9]+n[0-9]+|mmcblk[0-9]+)$").unwrap();
    if re.is_match(disk) {
        (format!("{disk}p{esp_no}"), format!("{disk}p{root_no}"))
    } else {
        (format!("{disk}{esp_no}"), format!("{disk}{root_no}"))
    }
}

/// Full, ordered partitioning sequence (no mkfs).
pub fn all_partition_commands(l: &GptLayout) -> Vec<String> {
    let mut cmds = zap_commands(&l.disk);
    cmds.extend(partition_commands(l));
    cmds
}

// ---------------------------------------------------------------------------
// Parsing `lsblk` / `blockdev` output (executor supplies raw strings)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LsblkDev {
    #[serde(rename = "NAME")]
    name: Option<String>,
    #[serde(rename = "SIZE")]
    size: Option<u64>,
    #[serde(rename = "MODEL")]
    model: Option<String>,
    #[serde(rename = "TYPE")]
    ptype: Option<String>,
}

#[derive(Deserialize)]
struct LsblkOut {
    blockdevices: Vec<LsblkDev>,
}

fn is_candidate(ptype: &str, name: &str) -> bool {
    ptype == "disk" && !name.is_empty() && !name.starts_with("loop")
}

/// Candidate whole disks from `lsblk -d -b -J -o NAME,SIZE,TYPE,MODEL,ROTA`
/// output. JSON is the canonical input because the MODEL column is
/// whitespace-padded in plain-text lsblk; a whitespace fallback is kept for
/// testing / hand-built strings (treats field 4 as a single-word model).
pub fn parse_lsblk_block_devices(raw: &str) -> Vec<DiskInfo> {
    let raw = raw.trim_start();
    if raw.starts_with('{') {
        let data: Result<LsblkOut, serde_json::Error> = serde_json::from_str(raw);
        match data {
            Ok(parsed) => return parsed.blockdevices.into_iter().filter_map(|dev| {
                let name = dev.name.unwrap_or_default();
                let ptype = dev.ptype.unwrap_or_default();
                is_candidate(&ptype, &name).then(|| DiskInfo {
                    name: name.clone(),
                    size_bytes: dev.size.unwrap_or(0),
                    model: dev.model.unwrap_or_default(),
                })
            }).collect(),
            Err(_) => return Vec::new(),
        }
    }
    raw.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                return None;
            }
            let (name, size, ptype) = (parts[0], parts[1], parts[2]);
            if !is_candidate(ptype, name) {
                return None;
            }
            Some(DiskInfo {
                name: name.to_string(),
                size_bytes: size.parse().unwrap_or(0),
                model: parts.get(3).copied().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Parse `/sys/block/<dev>/size` (device size in 512-byte sectors).
pub fn total_sectors_from_sysfs(raw: &str) -> u64 {
    raw.trim().parse().unwrap_or(0)
}

/// `/sys/block` directory for a device node (`/dev/sdX`, `/dev/sdXN`,
/// `/dev/srN`, `/dev/loopN`, `/dev/nvmeXnYpZ` all live there).
pub fn sysfs_block_path(dev: &str) -> std::path::PathBuf {
    let name = std::path::Path::new(dev)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dev.to_string());
    std::path::Path::new("/sys/block").join(name)
}

/// True when `dev` is a partition, not a whole disk. The `start`
/// attribute (first LBA) is only created for partition devices, on
/// every kernel generation we target; whole disks never have it.
/// (Kernel 7.x moved per-partition attrs to /sys/class/block/<n>/, so
/// check there — /sys/block/<n> is unreliable on the new layout.)
pub fn is_partition(dev: &str) -> bool {
    let name = std::path::Path::new(dev)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dev.to_string());
    std::path::Path::new("/sys/class/block")
        .join(&name)
        .join("start")
        .is_file()
}

/// Device size in 512-byte sectors, read straight from sysfs — no shell
/// (the dry-run audit mode makes `exec_cmd` return empty output, which
/// used to zero the sector count and kill GPT planning). Tries
/// /sys/block/<n>/size first, falls back to /sys/class/block/<n>/size
/// (kernel 7.x drops per-partition sysfs dirs under /sys/block).
#[cfg(target_os = "linux")]
pub fn read_device_sectors(dev: &str) -> Result<u64> {
    let name = std::path::Path::new(dev)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dev.to_string());
    let p1 = std::path::Path::new("/sys/block").join(&name).join("size");
    let p2 = std::path::Path::new("/sys/class/block").join(&name).join("size");
    let p = if p1.is_file() {
        p1
    } else if p2.is_file() {
        p2
    } else {
        bail!("{dev}: no sysfs size attribute (device missing or unreadable)")
    };
    let raw = std::fs::read_to_string(p.as_os_str())
        .with_context(|| format!("reading {}", p.display()))?;
    let n = total_sectors_from_sysfs(&raw);
    if n == 0 {
        bail!("{dev}: sysfs sector size is 0 (device missing or unreadable)");
    }
    Ok(n)
}
#[cfg(not(target_os = "linux"))]
pub fn read_device_sectors(_dev: &str) -> Result<u64> {
    bail!("read_device_sectors needs Linux sysfs")
}

// --- isofs (ISO 9660) read-only medium verification ------------------------
//
// The live medium is verified WITHOUT mounting: the Primary Volume
// Descriptor sits at logical sector 16 (absolute offset 32 KiB), 2048
// bytes: type 0x01, magic "CD001", volume label at bytes 40..72.

pub const ISOFS_PVD_OFFSET: u64 = 32_768;
pub const ISOFS_BLOCK: usize = 2048;

/// Volume label from raw PVD bytes; None on bad magic or empty label.
pub fn isofs_label_from_pvd(pvd: &[u8]) -> Option<String> {
    if pvd.len() < ISOFS_BLOCK || pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }
    let label = pvd[40..72]
        .iter()
        .map(|b| *b as char)
        .collect::<String>()
        .trim()
        .to_string();
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

/// Read-only open + seek + read of the isofs volume label from a block
/// device. Never mounts, never writes; a read failure or short device
/// simply yields None (the caller keeps probing).
#[cfg(target_os = "linux")]
pub fn read_isofs_volume_label(dev: &str) -> Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let f = match std::fs::File::open(dev) {
        Ok(f) => f,
        // Probe contract (see the doc comment): an unopenable or missing
        // device simply has no readable label — the caller keeps probing,
        // it is not an error.
        Err(_) => return Ok(None),
    };
    let mut f = f;
    if f.seek(SeekFrom::Start(ISOFS_PVD_OFFSET)).is_err() {
        return Ok(None);
    }
    let mut buf = [0u8; ISOFS_BLOCK];
    if f.read_exact(&mut buf).is_err() {
        return Ok(None); // shorter than a PVD window — not a live medium
    }
    Ok(isofs_label_from_pvd(&buf))
}
#[cfg(not(target_os = "linux"))]
pub fn read_isofs_volume_label(_dev: &str) -> Result<Option<String>> {
    bail!("isofs label probe needs Linux block devices")
}

/// Devices whose /proc/mounts fstype is iso9660 — this catches the live
/// medium even when its mount point is hidden under systemd's tmpfs /run
/// (the mount record persists, the directory does not).
pub fn find_iso9660_devices(mounts_raw: &str) -> Vec<String> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    mounts_raw
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 3 && f[2] == "iso9660" {
                Some(f[0].to_string())
            } else {
                None
            }
        })
        .filter(|d| seen.insert(d.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- geometry (ported from test/test_disks.py) ---

    #[test]
    fn known_21gb_disk_matches_sfdisk_dump() {
        // 21.5 GiB VMware disk, 512-byte sectors, total = 41943040 sectors.
        let lay = build_layout("/dev/sda", 41_943_040).unwrap();
        assert_eq!(lay.bbp_start, 2048);
        assert_eq!(lay.bbp_end, 2048 + 4096 - 1); // 2 MiB
        assert_eq!(lay.esp_start, 6144); // 3 MiB
        assert_eq!(lay.esp_end, 6144 + 1_048_576 - 1); // 512 MiB
        assert_eq!(lay.root_start, 6144 + 1_048_576);
        assert_eq!(lay.root_end, 41_943_040 - 34); // tail reserved (GPT backup block)
    }

    #[test]
    fn layout_too_small_errors() {
        assert!(build_layout("/dev/sda", 100).is_err());
        assert!(root_end_sector(0).is_err());
    }

    #[test]
    fn test_min_disk_sectors() {
        assert_eq!(min_disk_sectors(ESP_SIZE_MIB, 1), root_start_sector(ESP_SIZE_MIB) + 2048);
    }

    // --- sgdisk command strings ---

    #[test]
    fn full_plan_is_ordered_and_self_documenting() {
        let lay = build_layout("/dev/sda", 41_943_040).unwrap();
        let cmds = all_partition_commands(&lay);
        assert_eq!(cmds[0], "sgdisk --zap-all /dev/sda");
        // all three partitions are created before any property is attached
        assert!(cmds[1].contains("--new=1:2048:6143")); // 2 MiB bios boot
        assert!(cmds[2].contains("--new=2:6144:1054719")); // 512 MiB ESP
        assert!(cmds[3].contains("--new=3:1054720:41943006")); // total-34
        // each property is its own sgdisk invocation (the live image's
        // sgdisk build does not commit --new and --type/--set-boot
        // combined in one invocation)
        assert!(cmds[4].contains("-t 1:21686148-6449-6E6F-744E-656564454649"));
        assert!(cmds[5].contains("-c 1:BIOSBoot"));
        assert!(cmds[6].contains("-t 2:C12A7328-F81F-11D2-BA4B-00A0C93EC93B"));
        assert!(cmds[7].contains("-c 2:ESP"));
        assert!(cmds[8].contains("-b 2"));
        assert!(cmds[9].contains("-t 3:0FC63DAF-8483-4772-8E79-3D69D8477DE4"));
        assert!(cmds[10].contains("-c 3:root"));
        // creation commands carry no --type/--set-boot
        assert!(!cmds[1].contains("--type"));
        assert!(!cmds[2].contains("--type"));
        assert!(!cmds[3].contains("--set-boot"));
        // no partition command references a protective-MBR step
        assert!(!cmds.iter().any(|c| c.contains("protective")));
    }

    #[test]
    fn format_specs_use_correct_device_nodes() {
        let lay = build_layout("/dev/nvme0n1", 41_943_040).unwrap();
        let fmt = format_specs(&lay, "GOZJARO_EFI", "GOZJARO_ROOT");
        assert!(fmt[0].contains("/dev/nvme0n1p2")); // ESP is p2 now
        assert!(fmt[1].contains("/dev/nvme0n1p3")); // root is p3
        assert!(fmt[0].starts_with("mkfs.vfat"));
        assert!(fmt[1].starts_with("mkfs.ext4"));
    }

    // --- device naming ---

    #[test]
    fn nvme_mmcblk_take_p_suffix() {
        assert_eq!(
            partition_device_names("/dev/nvme0n1", 1, 2),
            ("/dev/nvme0n1p1".into(), "/dev/nvme0n1p2".into())
        );
        assert_eq!(
            partition_device_names("/dev/mmcblk0", 1, 2),
            ("/dev/mmcblk0p1".into(), "/dev/mmcblk0p2".into())
        );
    }

    #[test]
    fn scsi_sata_do_not() {
        assert_eq!(
            partition_device_names("/dev/sda", 1, 2),
            ("/dev/sda1".into(), "/dev/sda2".into())
        );
        assert_eq!(
            partition_device_names("/dev/sdb", 1, 2),
            ("/dev/sdb1".into(), "/dev/sdb2".into())
        );
    }

    // --- lsblk / blockdev parsers ---

    #[test]
    fn lsblk_json_keeps_multiword_model_and_filters_non_disk() {
        let js = r#"{"blockdevices": [
            {"NAME": "sda", "SIZE": 21474836480, "TYPE": "disk", "MODEL": "VMware Virtual S"},
            {"NAME": "loop0", "SIZE": 2348718592, "TYPE": "rom", "MODEL": "live"},
            {"NAME": "sda1", "SIZE": 538968064, "TYPE": "part", "MODEL": ""},
            {"NAME": "sr0", "SIZE": 2348718592, "TYPE": "rom", "MODEL": "CD01"}
        ]}"#;
        let got = parse_lsblk_block_devices(js);
        assert_eq!(
            got,
            vec![DiskInfo {
                name: "sda".into(),
                size_bytes: 21_474_836_480,
                model: "VMware Virtual S".into()
            }]
        );
    }

    #[test]
    fn lsblk_whitespace_fallback() {
        let raw = "sda 21474836480 disk VMware 0 0\n\
                   loop0 2348718592 rom live 0 1\n\
                   sr0 2348718592 rom VMware SATA CD01 0 0\n";
        let got = parse_lsblk_block_devices(raw);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "sda");
    }

    #[test]
    fn sysfs_size_parses() {
        assert_eq!(total_sectors_from_sysfs("41943040\n"), 41_943_040);
        assert_eq!(total_sectors_from_sysfs("   42\n"), 42);
        assert_eq!(total_sectors_from_sysfs(""), 0);
    }

    #[test]
    fn isofs_label_parses() {
        let mut pvd = [0u8; ISOFS_BLOCK];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        // ISO9660 pads the volume ID with spaces, not NULs
        pvd[40..72].fill(b' ');
        let label = b"GOZJARO_LIVE";
        pvd[40..40 + label.len()].copy_from_slice(label);
        assert_eq!(
            isofs_label_from_pvd(&pvd).as_deref(),
            Some("GOZJARO_LIVE")
        );
        // bad magic
        pvd[1..6].copy_from_slice(b"XXXXX");
        assert_eq!(isofs_label_from_pvd(&pvd), None);
        // empty label
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[40..72].fill(b' ');
        assert_eq!(isofs_label_from_pvd(&pvd), None);
    }

    #[test]
    fn proc_mounts_finds_iso9660_devices() {
        let mounts = "\
/dev/sr0 /run/livecd iso9660 ro,relatime 0 0
/dev/loop0 /run/squashfs squashfs ro,relatime 0 0
/dev/sr0 /mnt/iso iso9660 ro,relatime 0 0
/dev/sda2 / ext4 rw 0 0
";
        assert_eq!(find_iso9660_devices(mounts), vec!["/dev/sr0".to_string()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sysfs_probes_are_harmless_on_missing_devices() {
        assert!(!is_partition("/dev/definitely-missing-0"));
        assert!(read_device_sectors("/dev/definitely-missing-0").is_err());
        assert!(read_isofs_volume_label("/dev/definitely-missing-0").is_ok());
    }
}
