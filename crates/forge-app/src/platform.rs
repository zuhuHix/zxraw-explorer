//! Thin adapter between the UI and `forge-win`.

use std::path::PathBuf;

use forge_win::DriveKind;

use crate::sidebar::Drive;

pub fn drives() -> Vec<Drive> {
    forge_win::list_drives()
        .into_iter()
        .filter(|d| d.kind != DriveKind::NoRootDir)
        .map(|d| Drive {
            kind: match d.kind {
                DriveKind::Removable => "Removable",
                DriveKind::Network => "Network",
                DriveKind::CdRom => "Optical",
                DriveKind::RamDisk => "RAM disk",
                _ => "Local Disk",
            },
            root: d.root,
            label: d.label,
            total: d.total_bytes.unwrap_or(0),
            free: d.free_bytes.unwrap_or(0),
            available: d.available,
        })
        .collect()
}

pub fn known_folders() -> Vec<(String, PathBuf)> {
    forge_win::known_folders()
}
