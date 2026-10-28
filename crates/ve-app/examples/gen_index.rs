//! Writes a synthetic Index for measuring the Explorer: `gen_index <entries> <out.bin>`.
//! Its one Volume is mounted at a path that does not exist, so a sync leaves it as it is.

use std::path::Path;
use ve_core::{EntryMeta, Index, SyncState};

fn main() {
    let mut args = std::env::args().skip(1);
    let total: usize = args.next().and_then(|a| a.parse().ok()).expect("entry count");
    let out = args.next().expect("output path");
    let mut index = Index::new();
    let root_meta = EntryMeta { is_dir: true, ..Default::default() };
    let volume = index.add_volume(Path::new("/ve-bench"), "ext4", root_meta, SyncState::Walk);
    let root = index.volume(volume).unwrap().root;
    let exts = ["mp4", "jpg", "rs", "txt", "zip", "pdf", "mp3", "exe", "log", "bin"];
    let mut made = 0;
    let mut dir = 0usize;
    while made < total {
        let parent = index.insert(root, &format!("folder-{dir}"), root_meta);
        made += 1;
        for f in 0..100.min(total - made) {
            let n = (dir * 100 + f) as u64;
            // Spread sizes over several orders of magnitude; no two files share a size.
            let size = (n.wrapping_mul(2_654_435_761) % 50_000_000) + 1;
            let meta = EntryMeta {
                logical: size,
                on_disk: size.div_ceil(4096) * 4096,
                modified: 1_700_000_000_000_000_000 + (n as i64) * 1_000_000_000,
                key: n + 1,
                ..Default::default()
            };
            index.insert(parent, &format!("file-{n}.{}", exts[f % exts.len()]), meta);
            made += 1;
        }
        dir += 1;
    }
    index.save(Path::new(&out)).expect("save");
    println!("{} entries -> {out}", index.live_count());
}
