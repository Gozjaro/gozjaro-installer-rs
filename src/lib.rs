//! gozjaro-installer — rust rewrite of the python gozjaro-install backend.
//!
//! - [`disks`]   — pure GPT geometry + sgdisk/mkfs command strings + lsblk
//!                  parser (port of lib/disks.py, host-testable)
//! - [`state`]   — install state + mount/label constants + kernel naming
//! - [`plan`]    — fstab/hosts/grub/NVRAM/kver pure planning functions
//! - [`installer`] — the stateful, audited driver (port of lib/actions.py)

pub mod disks;
pub mod installer;
pub mod plan;
pub mod state;
