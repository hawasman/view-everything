//! Screenshots: `offline_demo <index-file>` adds a Volume that is not mounted, so the app shows
//! it Offline (and its Entries stay searchable).
use std::path::Path;
use ve_core::{EntryMeta, Index, SyncState};

fn main() {
    let file = std::env::args().nth(1).expect("index file");
    let mut index = Index::load(Path::new(&file)).unwrap_or_default();
    let dir = EntryMeta { is_dir: true, ..Default::default() };
    let volume = index.add_volume(Path::new("/run/media/user/BACKUP"), "exfat", "uuid:demo-offline", dir, SyncState::Walk);
    let root = index.volume(volume).unwrap().root;
    let photos = index.insert(root, "photos", dir);
    let size = 4_000_000;
    index.insert(photos, "holiday.jpg", EntryMeta { logical: size, on_disk: size, key: 1, ..Default::default() });
    index.mark_synced(volume, 1_700_000_000, None);
    index.save(Path::new(&file)).expect("save");
}
