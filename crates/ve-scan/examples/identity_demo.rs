//! Evidence for Volume identity: `identity_demo <index-file> [path-on-the-volume-to-index]`.
//! Reconciles the saved Index with the Volumes mounted now (indexing the one that holds the
//! given path), catches it up, saves, and prints every Volume's state.
use std::path::PathBuf;
use ve_core::Index;
use ve_scan::{Progress, SharedIndex};

fn main() {
    let mut args = std::env::args().skip(1);
    let file = PathBuf::from(args.next().expect("index file"));
    let target = args.next().map(PathBuf::from);
    let index = SharedIndex::new(Index::load(&file).unwrap_or_default());
    let mounted = ve_scan::discover_volumes();
    let wanted: Vec<_> = mounted
        .iter()
        .filter(|v| target.as_deref().is_some_and(|t| t.starts_with(&v.mount)) && v.mount.parent().is_some())
        .cloned()
        .collect();
    let (added, _) = ve_scan::reconcile(&index, &mounted, &wanted);
    println!("newly added (scanned from scratch): {}", added.len());
    let online: Vec<_> = index.read().volumes().filter(|(_, v)| v.online).map(|(id, _)| id).collect();
    for id in online {
        let progress = Progress::default();
        ve_scan::sync(&index, id, &progress).unwrap();
        println!("synced volume {}: {} entries read", id.0, progress.entries());
        index.write().mark_synced(id, 1, None);
    }
    index.read().save(&file).unwrap();
    let index = index.read();
    for (id, v) in index.volumes() {
        println!(
            "volume {} identity={} mount={} {} top-level entries={}",
            id.0,
            v.identity,
            v.mount.display(),
            if v.online { "ONLINE" } else { "OFFLINE" },
            index.children(v.root).count()
        );
    }
}
