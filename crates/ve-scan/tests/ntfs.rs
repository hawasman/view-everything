//! Loads a real NTFS image (made by `tests/fixtures/make-ntfs-fixture.sh`) through the parser.

use flate2::read::GzDecoder;
use std::io::{Cursor, Read};
use std::path::Path;
use ve_core::{EntryMeta, Index, SyncState};
use ve_scan::Progress;
use ve_scan::SharedIndex;

fn loaded() -> Index {
    let mut image = Vec::new();
    GzDecoder::new(&include_bytes!("fixtures/small.ntfs.gz")[..])
        .read_to_end(&mut image)
        .unwrap();
    let root = EntryMeta {
        is_dir: true,
        ..EntryMeta::default()
    };
    let mut index = Index::new();
    let volume = index.add_volume(Path::new("X:\\"), "NTFS", "guid:x", root, SyncState::Walk);
    let index = SharedIndex::new(index);
    let progress = Progress::default();
    ve_scan::ntfs::load(&index, volume, Cursor::new(image), &progress).unwrap();
    assert!(progress.entries() > 3);
    index.into_inner()
}

#[test]
fn reads_names_and_sizes_from_a_real_ntfs_image() {
    let index = loaded();
    let root = index.volumes().next().unwrap().1.root;
    let file = |name: &str| index.meta(index.child_named(root, name).unwrap_or_else(|| panic!("missing {name}")));

    // Sizes written by make-ntfs-fixture.sh.
    let tiny = file("tiny.txt");
    assert_eq!((tiny.is_dir, tiny.logical, tiny.on_disk), (false, 11, 0), "resident data");
    let big = file("big.bin");
    assert_eq!(big.logical, 300_000);
    assert_eq!(big.on_disk, 303_104, "300000 bytes in 4 KiB clusters");
    assert_eq!(big.on_disk % 4096, 0);
    let empty = file("empty.dat");
    assert_eq!((empty.logical, empty.on_disk), (0, 0));
    assert!(tiny.modified > 1_500_000_000 * 1_000_000_000, "modification time is ns since 1970");

    assert!(file("$Extend").is_dir);
    assert!(file("$MFT").logical > 0);
}
