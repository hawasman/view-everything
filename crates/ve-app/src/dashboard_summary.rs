//! What the Dashboard shows, computed in one pass over the whole Index: Category totals
//! (overall and per Volume), the largest files, and the most recently modified files that match
//! the saved Query. It ignores the Explorer's Query.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::time::{Duration, Instant};
use ve_core::{Candidate, Capacity, Category, CategoryMap, EntryId, Index, Query};

/// Rows in each of the Largest files and Recently modified tables.
pub const LIST_LEN: usize = 6;
/// How many of the newest files the main pass remembers to pick Recently modified from.
const POOL: usize = 4096;

/// One file in a Dashboard table.
#[derive(Clone, Debug)]
pub struct FileRow {
    pub id: EntryId,
    pub name: String,
    /// The folder holding the file, as a path.
    pub folder: String,
    /// Identity of the Volume the file is on.
    pub volume: String,
    pub category: Category,
    /// On-disk Size.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub modified: i64,
}

/// What one Volume's Entries add up to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumeSummary {
    pub identity: String,
    /// On-disk Size per Category, in `Category::ALL` order.
    pub categories: [u64; 8],
    /// Files in the Index for this Volume.
    pub files: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    /// The Index generation this was computed from.
    pub generation: u64,
    /// Why that Query could not be used, if so.
    pub recent_error: Option<String>,
    /// On-disk Size and file count per Category, in `Category::ALL` order.
    pub categories: [(u64, u64); 8],
    /// Entries (files and folders) in the Index.
    pub entries: u64,
    pub volumes: Vec<VolumeSummary>,
    pub largest: Vec<FileRow>,
    pub recent: Vec<FileRow>,
    pub took: Duration,
}

impl Summary {
    /// On-disk Size of every file in the Index.
    pub fn indexed(&self) -> u64 {
        self.categories.iter().map(|c| c.0).sum()
    }

    /// Categories with their (On-disk Size, files), largest first, empty ones left out.
    pub fn by_size(&self) -> Vec<(Category, u64, u64)> {
        let mut rows: Vec<_> = Category::ALL
            .iter()
            .map(|&c| (c, self.categories[c as usize].0, self.categories[c as usize].1))
            .filter(|r| r.2 > 0)
            .collect();
        rows.sort_by_key(|r| Reverse(r.1));
        rows
    }

    pub fn volume(&self, identity: &str) -> Option<&VolumeSummary> {
        self.volumes.iter().find(|v| v.identity == identity)
    }
}

/// The `n` best items by key, kept as a min-heap so a pass over millions of files holds only a few.
struct Best {
    n: usize,
    heap: BinaryHeap<Reverse<(i64, EntryId)>>,
}

impl Best {
    fn new(n: usize) -> Self {
        Self { n, heap: BinaryHeap::with_capacity(n + 1) }
    }

    /// Whether an item with `key` could still make the cut.
    fn wants(&self, key: i64) -> bool {
        self.heap.len() < self.n || self.heap.peek().is_some_and(|Reverse((k, _))| key > *k)
    }

    fn push(&mut self, key: i64, id: EntryId) {
        self.heap.push(Reverse((key, id)));
        if self.heap.len() > self.n {
            self.heap.pop();
        }
    }

    /// Best first.
    fn into_sorted(self) -> Vec<EntryId> {
        let mut items: Vec<_> = self.heap.into_iter().map(|Reverse(i)| i).collect();
        items.sort_by_key(|&(k, id)| Reverse((k, id)));
        items.into_iter().map(|(_, id)| id).collect()
    }
}

/// One pass over the Index. `recent` selects the files Recently modified may list; `None` lists
/// none. Hard links count once per Volume, as everywhere else.
pub fn summarize(
    index: &Index,
    categories: &CategoryMap,
    recent: Option<&Query>,
    recent_error: Option<String>,
) -> Summary {
    summarize_with(POOL, index, categories, recent, recent_error)
}

/// [`summarize`] with a pool of the `pool` newest files to pick Recently modified from.
fn summarize_with(
    pool_len: usize,
    index: &Index,
    categories: &CategoryMap,
    recent: Option<&Query>,
    recent_error: Option<String>,
) -> Summary {
    let started = Instant::now();
    let mut totals = [(0u64, 0u64); 8];
    let mut volumes = Vec::new();
    let mut largest = Best::new(LIST_LEN);
    let mut newest = Best::new(pool_len);

    for (_, volume) in index.volumes() {
        let mut summary = VolumeSummary { identity: volume.identity.clone(), ..Default::default() };
        let mut seen_links = HashSet::new();
        let mut stack = vec![volume.root];
        while let Some(dir) = stack.pop() {
            for &child in index.children(dir) {
                if index.is_dir(child) {
                    stack.push(child);
                    continue;
                }
                let modified = index.modified(child);
                // A hard link to a file already counted adds nothing.
                let repeated = index.is_hardlinked(child) && !seen_links.insert(index.key(child));
                if repeated {
                    continue;
                }
                if newest.wants(modified) {
                    newest.push(modified, child);
                }
                let size = index.on_disk(child);
                let category = categories.of_name(index.name(child));
                let slot = &mut totals[category as usize];
                slot.0 += size;
                slot.1 += 1;
                summary.categories[category as usize] += size;
                summary.files += 1;
                if largest.wants(size as i64) {
                    largest.push(size as i64, child);
                }
            }
        }
        volumes.push(summary);
    }

    let pool = newest.into_sorted();
    let truncated = pool.len() == pool_len;
    let recent_ids = recent.map_or_else(Vec::new, |query| {
        let mut scratch = (String::new(), Vec::new());
        let mut ids: Vec<EntryId> = pool.iter().copied().filter(|&id| matches(index, query, id, &mut scratch)).take(LIST_LEN).collect();
        // A Query few of the newest files satisfy: look through every file for the newest ones.
        if ids.len() < LIST_LEN && truncated {
            let mut best = Best::new(LIST_LEN);
            for i in 0..index.capacity() {
                let id = EntryId(i as u32);
                if !index.is_live(id) || index.is_dir(id) || !best.wants(index.modified(id)) {
                    continue;
                }
                if matches(index, query, id, &mut scratch) {
                    best.push(index.modified(id), id);
                }
            }
            ids = best.into_sorted();
        }
        ids
    });

    let row = |id: EntryId| {
        let parent = index.parent(id);
        FileRow {
            id,
            name: index.name(id).to_owned(),
            folder: parent.map(|p| index.path(p).display().to_string()).unwrap_or_default(),
            volume: index
                .volume_of(id)
                .and_then(|v| index.volume(v))
                .map(|v| v.identity.clone())
                .unwrap_or_default(),
            category: categories.of_name(index.name(id)),
            size: index.on_disk(id),
            modified: index.modified(id),
        }
    };
    Summary {
        generation: index.generation(),
        recent_error,
        categories: totals,
        entries: index.live_count() as u64,
        volumes,
        largest: largest.into_sorted().into_iter().map(row).collect(),
        recent: recent_ids.into_iter().map(row).collect(),
        took: started.elapsed(),
    }
}

/// Whether the file `id` matches `query`; `scratch` is reused between calls.
fn matches(index: &Index, query: &Query, id: EntryId, scratch: &mut (String, Vec<EntryId>)) -> bool {
    let path = query.needs_path().then(|| {
        index.write_match_path(id, &mut scratch.0, &mut scratch.1);
        scratch.0.as_str()
    });
    query.matches(&Candidate {
        name: index.name(id),
        path,
        is_dir: false,
        logical: index.logical(id),
        modified: index.modified(id),
    })
}

/// A Location's bar: its Categories, the used space no Entry accounts for, and what is free.
/// The parts add up to the capacity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lane {
    pub categories: [u64; 8],
    pub unindexed: u64,
    pub free: u64,
}

impl Lane {
    /// Splits a Volume of `capacity` given the On-disk Size of its files per Category. Without a
    /// capacity (never synced) the bar is only the Categories.
    pub fn new(mut categories: [u64; 8], capacity: Option<Capacity>) -> Lane {
        let Some(capacity) = capacity else {
            return Lane { categories, unindexed: 0, free: 0 };
        };
        let total = capacity.total;
        let used = total.saturating_sub(capacity.free);
        let indexed: u64 = categories.iter().sum();
        // More indexed than the Volume holds (a stale Index, sparse tricks): scale it to fit.
        if indexed > total {
            for c in &mut categories {
                *c = (*c as u128 * total as u128 / indexed as u128) as u64;
            }
        }
        let indexed: u64 = categories.iter().sum();
        // Whatever used space the Entries do not explain is Unindexed Space; the rest is free.
        let unindexed = used.saturating_sub(indexed);
        Lane { categories, unindexed, free: total - indexed - unindexed }
    }

    pub fn total(&self) -> u64 {
        self.categories.iter().sum::<u64>() + self.unindexed + self.free
    }
}

/// The filesystem a Volume identity belongs to. A btrfs subvolume or bind mount is the
/// filesystem's UUID plus a path (`uuid:UUID:/@home`) and shares the filesystem's capacity; every
/// other identity is a filesystem of its own.
pub fn filesystem_of(identity: &str) -> &str {
    match identity.strip_prefix("uuid:") {
        Some(rest) => &identity[..identity.len() - rest.len() + rest.find(':').unwrap_or(rest.len())],
        None => identity,
    }
}

/// Positions of `identities` grouped by filesystem, groups in order of first appearance.
pub fn group_by_filesystem(identities: &[&str]) -> Vec<Vec<usize>> {
    let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
    for (i, identity) in identities.iter().enumerate() {
        let fs = filesystem_of(identity);
        match groups.iter_mut().find(|(key, _)| *key == fs) {
            Some((_, members)) => members.push(i),
            None => groups.push((fs, vec![i])),
        }
    }
    groups.into_iter().map(|(_, members)| members).collect()
}

/// "just now", "6 min ago", "3 h ago", "1 day ago", "6 days ago".
pub fn ago(now: i64, at: i64) -> String {
    let secs = (now - at).max(0);
    match secs {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        86400..172800 => "1 day ago".to_owned(),
        _ => format!("{} days ago", secs / 86400),
    }
}

/// "Every 15 min", "Every 2 h", or "Manual" for 0.
pub fn every(minutes: u64) -> String {
    match minutes {
        0 => "Manual".to_owned(),
        m if m % 60 == 0 => format!("Every {} h", m / 60),
        m => format!("Every {m} min"),
    }
}

/// The oldest of the Last Synced times; a Location never synced is the oldest of all. The index
/// of the first such Location, or `None` with no Locations.
pub fn oldest(synced: &[Option<i64>]) -> Option<usize> {
    synced.iter().enumerate().min_by_key(|(_, at)| **at).map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ve_core::{EntryMeta, QueryContext, SyncState};

    fn file(size: u64, modified: i64) -> EntryMeta {
        EntryMeta { logical: size, on_disk: size, modified, ..Default::default() }
    }

    fn folder() -> EntryMeta {
        EntryMeta { is_dir: true, ..Default::default() }
    }

    /// Two Volumes: /a with a Video, an Image and a hard-linked pair; /b with an Audio.
    fn index() -> Index {
        let mut index = Index::new();
        let a = index.add_volume(std::path::Path::new("/a"), "ext4", "a", folder(), SyncState::Walk);
        let b = index.add_volume(std::path::Path::new("/b"), "ext4", "b", folder(), SyncState::Walk);
        let (ra, rb) = (index.volume(a).unwrap().root, index.volume(b).unwrap().root);
        let docs = index.insert(ra, "docs", folder());
        index.insert(docs, "film.mp4", file(1000, 5));
        index.insert(docs, "photo.JPG", file(300, 9));
        let link = EntryMeta { key: 7, hardlinked: true, ..file(50, 1) };
        index.insert(docs, "one.txt", link);
        index.insert(ra, "two.txt", link);
        let hidden = index.insert(rb, ".cache", folder());
        index.insert(hidden, "song.mp3", file(200, 20));
        index.insert(rb, "keep.mp3", file(100, 10));
        index
    }

    fn sum(index: &Index, recent: Option<&Query>) -> Summary {
        summarize(index, &CategoryMap::builtin(), recent, None)
    }

    #[test]
    fn category_totals_count_each_hard_link_once() {
        let s = sum(&index(), None);
        let of = |c: Category| s.categories[c as usize];
        assert_eq!(of(Category::Video), (1000, 1));
        assert_eq!(of(Category::Image), (300, 1));
        assert_eq!(of(Category::Audio), (300, 2));
        // one.txt and two.txt are one file: 50 bytes, once ("txt" is Document or Other).
        assert_eq!(s.indexed(), 1000 + 300 + 300 + 50);
        assert_eq!(s.entries, index().live_count() as u64);
        assert_eq!(s.by_size()[0].0, Category::Video);
    }

    #[test]
    fn each_volume_has_its_own_totals() {
        let s = sum(&index(), None);
        assert_eq!(s.volume("a").unwrap().categories.iter().sum::<u64>(), 1000 + 300 + 50);
        assert_eq!(s.volume("b").unwrap().categories.iter().sum::<u64>(), 300);
        assert_eq!(s.volume("b").unwrap().files, 2);
    }

    #[test]
    fn largest_files_are_biggest_first() {
        let s = sum(&index(), None);
        let names: Vec<_> = s.largest.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names[..3], ["film.mp4", "photo.JPG", "song.mp3"]);
        assert_eq!(s.largest[0].volume, "a");
        assert_eq!(s.largest[0].folder, "/a/docs");
    }

    #[test]
    fn recently_modified_follows_the_saved_query() {
        let index = index();
        let context = QueryContext::new(CategoryMap::builtin(), chrono::Local::now());
        // Under /b, outside hidden folders: the default shape of the saved Query.
        let query = Query::parse(r#""/b/*" !"*/.*""#, &context).unwrap();
        let s = sum(&index, Some(&query));
        let names: Vec<_> = s.recent.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["keep.mp3"]);
        // No Query lists nothing.
        assert!(sum(&index, None).recent.is_empty());
        // A broader one lists newest first.
        let all = Query::parse("type:audio|type:image", &context).unwrap();
        let names: Vec<_> = sum(&index, Some(&all)).recent.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, ["song.mp3", "keep.mp3", "photo.JPG"]);
    }

    #[test]
    fn a_selective_query_still_finds_files_older_than_the_pool() {
        let index = index();
        let context = QueryContext::new(CategoryMap::builtin(), chrono::Local::now());
        // Only film.mp4 matches, and it is not among the 2 newest files.
        let query = Query::parse("film", &context).unwrap();
        let s = summarize_with(2, &index, &CategoryMap::builtin(), Some(&query), None);
        let names: Vec<_> = s.recent.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["film.mp4"]);
    }

    #[test]
    fn a_bar_adds_up_to_the_volume_capacity() {
        let mut cats = [0u64; 8];
        cats[0] = 300;
        cats[1] = 100;
        let lane = Lane::new(cats, Some(Capacity { total: 1000, free: 500 }));
        assert_eq!((lane.unindexed, lane.free), (100, 500));
        assert_eq!(lane.total(), 1000);
        // Indexed more than used: no Unindexed Space, free shrinks, still the capacity.
        let lane = Lane::new(cats, Some(Capacity { total: 1000, free: 700 }));
        assert_eq!((lane.unindexed, lane.free), (0, 600));
        assert_eq!(lane.total(), 1000);
        // Indexed more than the capacity: scaled to fit.
        let lane = Lane::new(cats, Some(Capacity { total: 200, free: 0 }));
        assert_eq!(lane.total(), 200);
        assert_eq!(lane.free, 0);
        // Never synced: just the Categories.
        assert_eq!(Lane::new(cats, None).total(), 400);
    }

    #[test]
    fn subvolumes_of_one_filesystem_group_and_count_its_capacity_once() {
        let uuid = "d1894d9b-4d46-4135-b42b-f4e2f9062839";
        let ids = [
            format!("uuid:{uuid}"),
            "uuid:aaaa-bbbb".to_owned(),
            format!("uuid:{uuid}:/@home"),
            format!("uuid:{uuid}:/@log"),
            "share:nas:/media".to_owned(),
            "ssh:me@box:/srv".to_owned(),
        ];
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        assert_eq!(filesystem_of(ids[2]), format!("uuid:{uuid}"));
        assert_eq!(filesystem_of("dev:/dev/sda1@/mnt/x"), "dev:/dev/sda1@/mnt/x");
        assert_eq!(group_by_filesystem(&ids), vec![vec![0, 2, 3], vec![1], vec![4], vec![5]]);

        // Three subvolumes of one 1000-byte filesystem, 700 used: 100 + 200 + 50 indexed.
        let capacity = Capacity { total: 1000, free: 300 };
        let mut sum = [0u64; 8];
        for indexed in [100u64, 200, 50] {
            sum[Category::Video as usize] += indexed;
        }
        let lane = Lane::new(sum, Some(capacity));
        assert_eq!((lane.unindexed, lane.free, lane.total()), (350, 300, 1000));
    }

    #[test]
    fn the_oldest_location_is_the_one_never_synced_or_longest_ago() {
        assert_eq!(oldest(&[Some(50), Some(10), Some(30)]), Some(1));
        assert_eq!(oldest(&[Some(50), None, Some(10)]), Some(1));
        assert_eq!(oldest(&[]), None);
    }

    #[test]
    fn ages_and_intervals_read_as_in_the_design() {
        assert_eq!(ago(1000, 1000), "just now");
        assert_eq!(ago(1000 + 6 * 60, 1000), "6 min ago");
        assert_eq!(ago(1000 + 3 * 3600, 1000), "3 h ago");
        assert_eq!(ago(1000 + 86400, 1000), "1 day ago");
        assert_eq!(ago(1000 + 6 * 86400, 1000), "6 days ago");
        assert_eq!((every(15), every(120), every(0)), ("Every 15 min".into(), "Every 2 h".into(), "Manual".into()));
    }
}
