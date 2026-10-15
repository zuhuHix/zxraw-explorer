//! Concurrency policy for background work on network locations.
//!
//! SMB servers, especially NAS boxes built on spinning disks, slow down sharply when many
//! random reads arrive at once: the heads seek between files and the server's protocol
//! queue fills. Each file open also costs a network round trip that local disks do not
//! pay. Background fan-out (thumbnails, folder scans, hashing) therefore uses fewer
//! workers when the folder it works on is a network path. Local paths keep their own
//! counts.
//!
//! Callers pick the count from the path they are processing: [`workers_for`] for a single
//! folder or source, [`pick_workers`] when the caller has already decided the kind.

use std::path::Path;

use crate::netpath::is_network_path;

/// Image thumbnails read whole files from a share. Two keep a NAS busy without its
/// random reads thrashing; local thumbnails use half the cores.
pub const NETWORK_THUMB_WORKERS: usize = 2;

/// Directory-listing style work: opening each file costs a round trip on SMB, so a couple
/// of opens in flight hide latency without flooding the server.
pub const NETWORK_SCAN_WORKERS: usize = 2;

/// Full sequential reads of candidate files (hashing). One at a time keeps a spinning NAS
/// streaming; parallel streams would seek between files and run slower overall.
pub const NETWORK_HASH_WORKERS: usize = 1;

/// Worker count for `path`: `network` (capped at `local`) when `path` is a network path,
/// otherwise `local`. Never below 1.
pub fn workers_for(path: &Path, local: usize, network: usize) -> usize {
    pick_workers(is_network_path(path), local, network)
}

/// Pure form of [`workers_for`] for callers that already know whether the work is on a
/// network location. Never below 1, and a network count never exceeds the local count.
pub fn pick_workers(network: bool, local: usize, network_limit: usize) -> usize {
    let local = local.max(1);
    if network {
        network_limit.min(local).max(1)
    } else {
        local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_work_keeps_the_local_count() {
        assert_eq!(pick_workers(false, 8, NETWORK_HASH_WORKERS), 8);
        assert_eq!(pick_workers(false, 1, NETWORK_THUMB_WORKERS), 1);
        assert_eq!(pick_workers(false, 0, NETWORK_SCAN_WORKERS), 1);
    }

    #[test]
    fn network_work_uses_the_network_count() {
        assert_eq!(
            pick_workers(true, 8, NETWORK_THUMB_WORKERS),
            NETWORK_THUMB_WORKERS
        );
        assert_eq!(pick_workers(true, 8, NETWORK_HASH_WORKERS), 1);
    }

    #[test]
    fn network_never_exceeds_local_and_is_at_least_one() {
        assert_eq!(pick_workers(true, 1, NETWORK_THUMB_WORKERS), 1);
        assert_eq!(pick_workers(true, 2, 16), 2);
        assert_eq!(pick_workers(true, 0, 0), 1);
        assert_eq!(pick_workers(true, 4, 0), 1);
        for local in 0..16 {
            for net in 0..16 {
                let n = pick_workers(true, local, net);
                assert!((1..=local.max(1)).contains(&n));
                assert!(n <= net.max(1));
            }
        }
    }

    #[test]
    fn path_selection_follows_the_location() {
        // Local paths use the local count whatever the network count is.
        let local = std::env::temp_dir();
        assert_eq!(workers_for(&local, 6, NETWORK_SCAN_WORKERS), 6);
        // UNC paths are network paths everywhere, so they take the network count.
        let unc = Path::new(r"\\nas\photos\2024");
        assert_eq!(
            workers_for(unc, 6, NETWORK_SCAN_WORKERS),
            NETWORK_SCAN_WORKERS
        );
        assert_eq!(workers_for(unc, 1, NETWORK_SCAN_WORKERS), 1);
    }
}
