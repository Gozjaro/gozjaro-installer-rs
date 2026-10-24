//! Pure, testable disk-layout logic. 1:1 port of gozjaro-install's
//! `lib/disks.py` — nothing here touches a running system: it builds
//! sgdisk *command strings* and filesystem specs that the executor runs.
//!
//! Partition plan (GPT, x86_64 UEFI; also valid for a legacy-BIOS boot via
//! the protected-MBR + LegacyBIOSBootable attribute on p1):
//!
//! ```text
//! p1  ESP   1 MiB .. 513 MiB   FAT32   GUID C12A7328-... (EFI)   LegacyBIOSBootable
//! p2  root  513 MiB .. end     ext4    GUID 0FC63DAF-... (Linux)
//! ```
//!
//! Why sgdisk and not parted: the live image's parted 3.6 segfaults in its
//! geometry-probe/exit path on some virtual SCSI disks; sgdisk is robust,
//! scriptable, and is what LFS ships.

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde::Serialize;

/// GPT partition type GUIDs (upper-case, canonical).
pub const ESP_GUID: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"; // EFI System
pub const LINUX_GUID: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"; // Linux filesystem

pub const SECTOR: u64 = 512;
pub const ESP_START_SECTOR: u64 = 2048; // 1 MiB
pub const ESP_SIZE_MIB: u64 = 512;
pub const GPT_TAIL_RESERVE: u64 = 33; // backup GPT + protective MBR, sgdisk reserves it

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GptLayout {
    pub disk: String,
    pub esp_no: u32,
    pub root_no: u32,
    pub esp_start: u64,
    pub esp_end: u64,
    pub root_start: u64,
    pub root_end: u64,
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

/// Inclusive end sector of the ESP given 512-byte sectors.
pub fn esp_end_sector(esp_mib: u64) -> u64 {
    ESP_START_SECTOR + esp_mib * 1024 * 1024 / SECTOR - 1
}

pub fn root_start_sector(esp_mib: u64) -> u64 {
    esp_end_sector(esp_mib) + 1
}

/// Last usable sector for the root partition. sgdisk reserves 33 sectors at
/// the tail for the backup GPT header + protective MBR; a partition must not
/// run past total-33.
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
        esp_no: 1,
        root_no: 2,
        esp_start: ESP_START_SECTOR,
        esp_end: esp_end_sector(ESP_SIZE_MIB),
        root_start: root_start_sector(ESP_SIZE_MIB),
        root_end: root_end_sector(total_sectors)?,
        esp_guid: ESP_GUID,
        root_guid: LINUX_GUID,
    };
    // Hard invariant: the ESP must sit entirely inside the disk and the root
    // must have at least one sector.
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
/// on both ends; the GUIDs are spelled explicitly so the command is
/// self-documenting and independent of sgdisk's keyword table.
pub fn partition_commands(l: &GptLayout) -> Vec<String> {
    vec![
        format!(
            "sgdisk --new={no}:{start}:{end} --type={no}:{guid} --change-name={no}:ESP --set-boot={no} {disk}",
            no = l.esp_no,
            start = l.esp_start,
            end = l.esp_end,
            guid = l.esp_guid,
            disk = l.disk
        ),
        format!(
            "sgdisk --new={no}:{start}:{end} --type={no}:{guid} --change-name={no}:root {disk}",
            no = l.root_no,
            start = l.root_start,
            end = l.root_end,
            guid = l.root_guid,
            disk = l.disk
        ),
    ]
}

/// FAT32 ESP + ext4 root with the given volume labels.
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

/// `blockdev --getsz DEV` prints a single sector count (512-byte units).
pub fn total_sectors_from_blockdev(raw: &str) -> u64 {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- geometry (ported from test/test_disks.py) ---

    #[test]
    fn known_21gb_disk_matches_sfdisk_dump() {
        // 21.5 GiB VMware disk, 512-byte sectors, total = 41943040 sectors.
        let lay = build_layout("/dev/sda", 41_943_040).unwrap();
        assert_eq!(lay.esp_start, 2048);
        assert_eq!(lay.esp_end, 2048 + 1_048_576 - 1); // 512 MiB
        assert_eq!(lay.root_start, 2048 + 1_048_576);
        assert_eq!(lay.root_end, 41_943_040 - 33); // tail reserved
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
        assert!(cmds[1].contains("--new=1:2048:1050623"));
        assert!(cmds[1].contains("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"));
        assert!(cmds[1].contains("--set-boot=1"));
        assert!(cmds[2].contains("--new=2:1050624:41943007"));
        assert!(cmds[2].contains("0FC63DAF-8483-4772-8E79-3D69D8477DE4"));
        // no partition command references a protective-MBR step
        assert!(!cmds.iter().any(|c| c.contains("protective")));
    }

    #[test]
    fn format_specs_use_correct_device_nodes() {
        let lay = build_layout("/dev/nvme0n1", 41_943_040).unwrap();
        let fmt = format_specs(&lay, "GOZJARO_EFI", "GOZJARO_ROOT");
        assert!(fmt[0].contains("/dev/nvme0n1p1"));
        assert!(fmt[1].contains("/dev/nvme0n1p2"));
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
    fn blockdev_parses_single_number() {
        assert_eq!(total_sectors_from_blockdev("41943040\n"), 41_943_040);
        assert_eq!(total_sectors_from_blockdev("   42\n"), 42);
        assert_eq!(total_sectors_from_blockdev(""), 0);
    }
}
