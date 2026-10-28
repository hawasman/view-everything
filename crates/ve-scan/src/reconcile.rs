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
/// could not be added.
pub fn reconcile(
    index: &SharedIndex,
    mounted: &[VolumeInfo],
    wanted: &[VolumeInfo],
) -> (Vec<VolumeId>, Vec<(PathBuf, io::Error)>) {
    {
        let mut index = index.write();
        let known: Vec<(VolumeId, String)> = index
            .volumes()
            .map(|(id, v)| (id, v.identity.clone()))
            .collect();
        for (id, identity) in known {
            let find = |list: &[VolumeInfo]| list.iter().find(|m| m.identity == identity).cloned();
            match (find(mounted), find(wanted)) {
                (_, Some(here)) => index.set_mount(id, Some(&here.mount)),
                (Some(_), None) => index.remove_volume(id),
                (None, None) => index.set_mount(id, None),
            }
        }
    }
    let (mut added, mut failed) = (Vec::new(), Vec::new());
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
