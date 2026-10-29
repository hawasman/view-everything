//! Matching the Volumes the Index holds against the Volumes mounted now.

use crate::{SharedIndex, VolumeInfo, add_volume};
use std::io;
use std::path::PathBuf;
use ve_core::VolumeId;

/// Makes the Index agree with what is mounted. `mounted` is every Volume present, `wanted` the
/// ones to index.
///
/// A Volume is matched by identity, so one mounted at a new path keeps its Entries and only
/// needs a Catch-up. An Indexed Volume that is not mounted stays as an Offline Volume. One that
/// is mounted but no longer wanted is removed. Returns the Volumes newly added, and those that
/// could not be added. The first list is every Volume to sync now: the new ones, and the ones
/// that came back online (a remount may follow changes made elsewhere, so it catches up).
pub fn reconcile(index: &SharedIndex, mounted: &[VolumeInfo], wanted: &[VolumeInfo]) -> (Vec<VolumeId>, Vec<(PathBuf, io::Error)>) {
    let (mut added, mut failed) = (Vec::new(), Vec::new());
    {
        let mut index = index.write();
        let known: Vec<(VolumeId, String)> = index.volumes().map(|(id, v)| (id, v.identity.clone())).collect();
        for (id, identity) in known {
            // Remotes are not mounted; `reconcile_remotes` looks after them.
            if index.volume(id).is_some_and(|v| v.is_remote()) {
                continue;
            }
            let find = |list: &[VolumeInfo]| list.iter().find(|m| m.identity == identity).cloned();
            match (find(mounted), find(wanted)) {
                (_, Some(here)) => {
                    let back = index.volume(id).is_some_and(|v| !v.online || v.mount != here.mount);
                    index.set_mount(id, Some(&here.mount));
                    if back {
                        added.push(id);
                    }
                }
                (Some(_), None) => index.remove_volume(id),
                (None, None) => index.set_mount(id, None),
            }
        }
    }
    for volume in wanted {
        if index.read().volume_with_identity(&volume.identity).is_some() {
            continue;
        }
        match add_volume(index, &volume.mount, &volume.fs_type, &volume.identity) {
            Ok(id) => added.push(id),
            Err(e) => failed.push((volume.mount.clone(), e)),
        }
    }
    (added, failed)
}
