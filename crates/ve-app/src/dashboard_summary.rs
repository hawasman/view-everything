//! What the Dashboard shows, computed in one pass over the whole Index: Category totals
//! (overall and per Volume), the largest files, and the most recently modified files that match
//! the saved Query. It ignores the Explorer's Query.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::time::{Duration, Instant};
use ve_core::{Candidate, Category, CategoryMap, EntryId, Index, OwnerId, OwnerKind, Query};

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
    /// The Owners with the largest Folder Size, largest first; empty without Owner roots.
    pub owners: Vec<OwnerRow>,
    pub recent: Vec<FileRow>,
    pub took: Duration,
}

impl Summary {
    /// On-disk Size of every file in the Index.
    pub fn indexed(&self) -> u64 {
        self.categories.iter().map(|c| c.0).sum()
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
        Self {
            n,
            heap: BinaryHeap::with_capacity(n + 1),
        }
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
pub fn summarize(index: &Index, categories: &CategoryMap, recent: Option<&Query>, recent_error: Option<String>) -> Summary {
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
        let mut summary = VolumeSummary {
            identity: volume.identity.clone(),
            ..Default::default()
        };
        let mut seen_links = HashSet::new();
        let mut stack = vec![volume.root];
        while let Some(dir) = stack.pop() {
            for child in index.children(dir) {
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
        let mut ids: Vec<EntryId> = pool
            .iter()
            .copied()
            .filter(|&id| matches(index, query, id, &mut scratch))
            .take(LIST_LEN)
            .collect();
        // A Query few of the newest files satisfy: look through every file for the newest ones.
        if ids.len() < LIST_LEN && truncated {
            let mut best = Best::new(LIST_LEN);
            let mut seen_links = HashSet::new();
            for i in 0..index.capacity() {
                let id = EntryId(i as u32);
                if !index.is_live(id) || index.is_dir(id) || !best.wants(index.modified(id)) {
                    continue;
                }
                // A hard link to a file already listed (in its Volume) adds nothing.
                if index.is_hardlinked(id) && !seen_links.insert((index.volume_of(id), index.key(id))) {
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
        owners: largest_owners(index, OWNER_LEN),
        largest: largest.into_sorted().into_iter().map(row).collect(),
        recent: recent_ids.into_iter().map(row).collect(),
        took: started.elapsed(),
    }
}

/// Owners kept for the Largest Owners panel, as many as the other lists.
pub const OWNER_LEN: usize = LIST_LEN;

/// One Owner in the Largest Owners panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerRow {
    pub name: String,
    pub kind: OwnerKind,
    /// Folder Size summed over the Owner's roots.
    pub size: u64,
    /// Its largest Owner root, which a click inspects.
    pub root: EntryId,
}

/// The `n` Owners with the largest Folder Size, largest first. An Owner with several roots sums
/// them. A root nested in another Owner's root counts toward both: each Owner's size is simply
/// the Folder Size of its roots, so an enclosing Owner includes what the nested one holds.
/// Hard links count once per root.
fn largest_owners(index: &Index, n: usize) -> Vec<OwnerRow> {
    /// What one Owner adds up to so far.
    struct Tally {
        size: u64,
        largest_root: EntryId,
        largest_root_size: u64,
    }
    let mut tallies: HashMap<OwnerId, Tally> = HashMap::new();
    for (root, owner) in index.owner_roots() {
        let mut seen_links = HashSet::new();
        let mut total = 0u64;
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if index.is_dir(id) {
                stack.extend(index.children(id));
            } else if !index.is_hardlinked(id) || seen_links.insert(index.key(id)) {
                total += index.on_disk(id);
            }
        }
        let tally = tallies.entry(owner).or_insert(Tally {
            size: 0,
            largest_root: root,
            largest_root_size: 0,
        });
        tally.size += total;
        if total > tally.largest_root_size {
            tally.largest_root = root;
            tally.largest_root_size = total;
        }
    }
    let mut rows: Vec<OwnerRow> = tallies
        .into_iter()
        .map(|(o, tally)| {
            let owner = index.owner(o);
            OwnerRow {
                name: owner.name.clone(),
                kind: owner.kind,
                size: tally.size,
                root: tally.largest_root,
            }
        })
        .collect();
    rows.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    rows.truncate(n);
    rows
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
        depth: if query.needs_depth() { index.depth(id) } else { 0 },
        owner: if query.needs_owner() {
            index.entry_owner(id).map(|o| (o.kind, o.name.as_str()))
        } else {
            None
        },
        ..Candidate::default()
    })
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

/// The three questions the Overview answers, in the order a tie keeps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Question {
    /// What is filling the disk.
    Disk,
    /// What is slowing the machine.
    Slowing,
    /// What is talking on the network.
    Talking,
}

impl Question {
    pub fn title(self) -> &'static str {
        match self {
            Question::Disk => "Filling the disk",
            Question::Slowing => "Slowing the machine",
            Question::Talking => "Talking on the network",
        }
    }

    /// The Module page following the answer opens.
    pub fn module(self) -> &'static str {
        match self {
            Question::Disk => "Browse",
            Question::Slowing => "Performance",
            Question::Talking => "Network",
        }
    }
}

/// How the Overview orders its answers, as the page states it.
pub const RANKING_RULE: &str = "Most severe first: how full the fullest Location is; the busier of CPU and memory over the last hour; \
     the busiest interface's average over the last hour against 100 Mbit/s. A question with nothing recorded comes last.";

/// The throughput a busiest interface is measured against: 100 Mbit/s, in bytes per second.
const REFERENCE_RATE: f64 = 12_500_000.;

/// What an answer says when its Module has no samples: never zero.
pub const NOT_RECORDED: &str = "not recorded";

/// One of the Overview's answers.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub question: Question,
    /// How severe, 0 to 1 (see [`RANKING_RULE`]); `None` when nothing was recorded to judge by.
    pub severity: Option<f32>,
    pub text: String,
    /// What its Module page shows in the Inspector when the line is followed.
    pub inspect: Option<crate::inspector::Inspected>,
}

/// The fullest Location with a known capacity: identity, name, used and total bytes.
pub struct Fullest<'a> {
    pub identity: &'a str,
    pub name: &'a str,
    pub used: u64,
    pub total: u64,
}

/// A share of 0 to 1 as a whole percentage.
pub fn percent(share: f32) -> String {
    format!("{:.0}%", share * 100.)
}

/// What the Cleaner found: `count` Cleanables of `reclaim` bytes, `admin` of them freed only
/// with administrator rights.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleanable {
    pub reclaim: u64,
    pub count: usize,
    pub admin: u64,
}

impl Cleanable {
    /// "4.00 KB can be cleaned up across 3 Cleanables"; the administrator share only when there
    /// is one.
    fn phrase(self) -> String {
        if self.count == 0 {
            return "nothing to clean up".to_owned();
        }
        let mut phrase = format!(
            "{} can be cleaned up across {} Cleanable{}",
            crate::format::size(self.reclaim),
            crate::format::count(self.count as u64),
            if self.count == 1 { "" } else { "s" }
        );
        if self.admin > 0 {
            phrase.push_str(&format!("; {} of it needs administrator", crate::format::size(self.admin)));
        }
        phrase
    }
}

/// What is filling the disk: the fullest Location, what the Cleaner found (`None` while it has
/// not looked) and the largest Owner. Its severity is how full that Location is.
pub fn disk_answer(fullest: Option<Fullest>, cleanable: Option<Cleanable>, owner: Option<&OwnerRow>) -> Answer {
    let fullest = fullest.filter(|f| f.total > 0);
    let severity = fullest.as_ref().map(|f| (f.used as f64 / f.total as f64).clamp(0., 1.) as f32);
    let mut parts = Vec::new();
    if let (Some(f), Some(share)) = (&fullest, severity) {
        parts.push(format!("{} is {} full", f.name, percent(share)));
    }
    match cleanable {
        Some(c) => parts.push(c.phrase()),
        None if fullest.is_some() || owner.is_some() => parts.push("looking for Cleanables…".to_owned()),
        None => {}
    }
    if let Some(o) = owner {
        parts.push(format!("{} the largest Owner at {}", o.name, crate::format::size(o.size)));
    }
    let inspect = match (owner, &fullest) {
        (Some(o), _) => Some(crate::inspector::Inspected::Entry(o.root)),
        (None, Some(f)) => Some(crate::inspector::Inspected::Location(f.identity.to_owned())),
        (None, None) => None,
    };
    Answer {
        question: Question::Disk,
        severity,
        text: if parts.is_empty() {
            "not read yet".to_owned()
        } else {
            parts.join(" · ")
        },
        inspect,
    }
}

/// What is slowing the machine, from the Performance Module's last hour: the busier of CPU and
/// memory first, then the busiest process. Its severity is the busier one's share.
pub fn slowing_answer(perf: &ve_engine::perf::Summary) -> Answer {
    let severity = match (perf.cpu, perf.memory) {
        (Some(c), Some(m)) => Some(c.max(m)),
        (c, m) => c.or(m),
    };
    if perf.seconds == 0 {
        return Answer {
            question: Question::Slowing,
            severity: None,
            text: NOT_RECORDED.to_owned(),
            inspect: None,
        };
    }
    let known = |share: Option<f32>| share.map_or("—".to_owned(), percent);
    let (cpu, memory) = (known(perf.cpu), known(perf.memory));
    // The busier subsystem leads.
    let subsystems = if perf.memory.unwrap_or(0.) > perf.cpu.unwrap_or(0.) {
        format!("Memory {memory} taken, CPU {cpu} busy")
    } else {
        format!("CPU {cpu} busy, memory {memory} taken")
    };
    let process = match &perf.top {
        Some(top) => format!("{} the busiest process at {} CPU", top.name, percent(top.cpu)),
        None => "busiest process —".to_owned(),
    };
    Answer {
        question: Question::Slowing,
        severity,
        text: format!("{subsystems} in the last hour · {process}"),
        inspect: perf.top.as_ref().map(|t| crate::inspector::Inspected::Process(t.id)),
    }
}

/// What is talking, from the Network Module's last hour: the busiest interface and the open
/// connections (`None` until they are recorded). Its severity is that interface's average
/// throughput against [`REFERENCE_RATE`].
pub fn talking_answer(net: &ve_engine::net::Summary, connections: Option<u32>) -> Answer {
    if net.seconds == 0 {
        return Answer {
            question: Question::Talking,
            severity: None,
            text: NOT_RECORDED.to_owned(),
            inspect: None,
        };
    }
    let connections = match connections {
        Some(n) => format!(
            "{} connection{} open",
            crate::format::count(u64::from(n)),
            if n == 1 { "" } else { "s" }
        ),
        None => format!("connections {NOT_RECORDED}"),
    };
    let (severity, busiest) = match net.busiest() {
        Some(i) => {
            // For the ranking an unknown direction counts as none; shown, it stays unknown.
            let rate = i.moved() / net.seconds as u64;
            let moved = match i.received.zip(i.sent) {
                Some((received, sent)) => format!(
                    "{} moved {} in the last hour, {}/s on average",
                    i.name,
                    crate::format::size(received + sent),
                    crate::format::size(rate)
                ),
                None => format!("{} the busiest interface in the last hour; what it moved —", i.name),
            };
            ((rate as f64 / REFERENCE_RATE).min(1.) as f32, moved)
        }
        None => (0., "nothing moved in the last hour".to_owned()),
    };
    Answer {
        question: Question::Talking,
        severity: Some(severity),
        text: format!("{busiest} · {connections}"),
        inspect: None,
    }
}

/// Orders `answers` by [`RANKING_RULE`]: the most severe first, the unknown last, a tie in
/// [`Question`] order.
pub fn rank(answers: &mut [Answer]) {
    answers.sort_by(|a, b| {
        let key = |x: &Answer| x.severity.map(|s| -s);
        match (key(a), key(b)) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then(a.question.cmp(&b.question))
    });
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

/// The oldest of the Last Synced times; a Location never synced is the oldest of all. The index
/// of the first such Location, or `None` with no Locations.
pub fn oldest(synced: &[Option<i64>]) -> Option<usize> {
    synced.iter().enumerate().min_by_key(|(_, at)| **at).map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ve_core::{EntryMeta, QueryContext, SyncState};

    /// `modified` is in seconds: the Index keeps times to the second.
    fn file(size: u64, modified: i64) -> EntryMeta {
        EntryMeta {
            logical: size,
            on_disk: size,
            modified: modified * 1_000_000_000,
            ..Default::default()
        }
    }

    fn folder() -> EntryMeta {
        EntryMeta {
            is_dir: true,
            ..Default::default()
        }
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
        let link = EntryMeta {
            key: 7,
            hardlinked: true,
            ..file(50, 1)
        };
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
        assert_eq!(s.largest[0].folder, std::path::Path::new("/a").join("docs").display().to_string());
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
    fn subvolumes_of_one_filesystem_group_together() {
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
    }

    /// A Remote reached through a stand-in ssh that runs the command locally appears in the
    /// per-Location totals and the Category totals, with the capacity its df reported. The
    /// stand-in runs the local `find -printf`, which needs GNU find.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_remote_counts_in_the_locations_and_categories() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ve-dashboard-remote-{}", std::process::id()));
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("song.mp3"), vec![0u8; 5000]).unwrap();
        let ssh = dir.join("ssh");
        let body = "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done; shift; shift; exec sh -c \"$*\"\n";
        std::fs::write(&ssh, body).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();

        let shared = ve_scan::SharedIndex::new(index());
        let remote = ve_core::Remote::new("me@box", tree.to_str().unwrap()).unwrap();
        let id = shared.write().add_remote(&remote);
        ve_scan::remote::sync_with(ssh.to_str().unwrap(), &shared, id, &ve_scan::Progress::default()).unwrap();
        let before = sum(&index(), None);
        let s = sum(&shared.read(), None);
        let lane = s.volume(&remote.identity()).expect("the Remote has a lane");
        assert!(lane.categories[Category::Audio as usize] >= 5000);
        assert_eq!(
            s.categories[Category::Audio as usize].0,
            before.categories[Category::Audio as usize].0 + lane.categories[Category::Audio as usize]
        );
        assert!(shared.read().volume(id).unwrap().capacity.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
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
    }

    #[test]
    fn largest_owners_sum_roots_count_nested_twice_and_cut_at_n() {
        use ve_core::Owner;
        let mut index = Index::new();
        let v = index.add_volume(std::path::Path::new("/v"), "ext4", "v", folder(), SyncState::Walk);
        let root = index.volume(v).unwrap().root;
        let game = Owner::new(OwnerKind::Game, "Game", "Steam");
        let mut roots = Vec::new();
        // One Owner in two folders: 100 + 200.
        let (l1, l2) = (index.insert(root, "lib1", folder()), index.insert(root, "lib2", folder()));
        index.insert(l1, "a.bin", file(100, 1));
        index.insert(l2, "b.bin", file(200, 1));
        roots.push((l1, game.clone()));
        roots.push((l2, game));
        // A Project holding 50 with a nested Program of 30: the Project counts both.
        let p = index.insert(root, "proj", folder());
        index.insert(p, "x.rs", file(50, 1));
        let inner = index.insert(p, "inner", folder());
        index.insert(inner, "y.rs", file(30, 1));
        roots.push((p, Owner::new(OwnerKind::Project, "Proj", "Cargo.toml")));
        roots.push((inner, Owner::new(OwnerKind::Program, "Inner", "pacman")));
        for i in 0..12 {
            let f = index.insert(root, &format!("o{i}"), folder());
            index.insert(f, "f", file(i, 1));
            roots.push((f, Owner::new(OwnerKind::Program, format!("O{i}"), "")));
        }
        index.set_owners(root, roots);
        let rows = largest_owners(&index, 10);
        assert_eq!(rows.len(), 10);
        assert_eq!((rows[0].name.as_str(), rows[0].size, rows[0].kind), ("Game", 300, OwnerKind::Game));
        // A click inspects the larger of its two roots.
        assert_eq!(rows[0].root, l2);
        assert_eq!((rows[1].name.as_str(), rows[1].size), ("Proj", 80));
        assert_eq!((rows[2].name.as_str(), rows[2].size), ("Inner", 30));
        assert!(rows.windows(2).all(|w| w[0].size >= w[1].size));
    }

    fn perf(seconds: usize, cpu: Option<f32>, memory: Option<f32>, top: Option<(&str, f32)>) -> ve_engine::perf::Summary {
        ve_engine::perf::Summary {
            seconds,
            cpu,
            memory,
            top: top.map(|(name, cpu)| ve_engine::perf::TopProcess {
                id: ve_engine::perf::ProcessId { pid: 7, start: 3 },
                name: name.into(),
                cpu,
            }),
        }
    }

    /// `seconds` of network readings, `wlan0` having moved `moved` bytes over them.
    fn net(seconds: usize, moved: u64) -> ve_engine::net::Summary {
        ve_engine::net::Summary {
            seconds,
            interfaces: vec![ve_engine::net::InterfaceSummary {
                name: "wlan0".into(),
                received: Some(moved),
                sent: Some(0),
                last_seen: 0,
            }],
        }
    }

    fn order(answers: &[Answer]) -> Vec<Question> {
        answers.iter().map(|a| a.question).collect()
    }

    #[test]
    fn the_most_severe_answer_comes_first() {
        let fullest = Fullest {
            identity: "uuid:1",
            name: "nvme0n1p2",
            used: 87,
            total: 100,
        };
        let mut answers = [
            disk_answer(
                Some(fullest),
                Some(Cleanable {
                    reclaim: 4096,
                    count: 2,
                    admin: 0,
                }),
                None,
            ),
            // Memory nearly full outweighs a disk 87% full.
            slowing_answer(&perf(3600, Some(0.3), Some(0.95), Some(("firefox", 0.12)))),
            // 2 MB/s on average: a sixth of 100 Mbit/s.
            talking_answer(&net(100, 200_000_000), None),
        ];
        rank(&mut answers);
        assert_eq!(order(&answers), [Question::Slowing, Question::Disk, Question::Talking]);
        assert_eq!(
            answers[1].text,
            "nvme0n1p2 is 87% full · 4.00 KB can be cleaned up across 2 Cleanables"
        );
        assert_eq!(answers[1].inspect, Some(crate::inspector::Inspected::Location("uuid:1".into())));
        // The administrator share only when there is one; a largest Owner is what the line inspects.
        let owner = OwnerRow {
            name: "Steam".into(),
            kind: OwnerKind::Game,
            size: 2048,
            root: EntryId(5),
        };
        let cleanable = Cleanable {
            reclaim: 4096,
            count: 1,
            admin: 1024,
        };
        let disk = disk_answer(None, Some(cleanable), Some(&owner));
        assert_eq!(
            disk.text,
            "4.00 KB can be cleaned up across 1 Cleanable; 1.00 KB of it needs administrator · Steam the largest Owner at 2.00 KB"
        );
        assert_eq!(disk.inspect, Some(crate::inspector::Inspected::Entry(EntryId(5))));
        // No capacity known: nothing to judge how full the disk is by.
        assert_eq!(disk.severity, None);
        // The busier subsystem leads, then the busiest process, which the line inspects.
        assert_eq!(
            answers[0].text,
            "Memory 95% taken, CPU 30% busy in the last hour · firefox the busiest process at 12% CPU"
        );
        assert_eq!(
            answers[0].inspect,
            Some(crate::inspector::Inspected::Process(ve_engine::perf::ProcessId {
                pid: 7,
                start: 3
            }))
        );

        // A busy link outranks a calm machine.
        let mut answers = [
            slowing_answer(&perf(3600, Some(0.1), Some(0.2), None)),
            talking_answer(&net(10, 1_000_000_000), Some(42)),
        ];
        rank(&mut answers);
        assert_eq!(order(&answers), [Question::Talking, Question::Slowing]);
        assert_eq!(answers[0].severity, Some(1.));
        assert!(answers[0].text.ends_with("· 42 connections open"), "{}", answers[0].text);
    }

    #[test]
    fn a_module_with_no_samples_says_so_and_ranks_last() {
        let mut answers = [
            disk_answer(None, None, None),
            slowing_answer(&perf(0, None, None, None)),
            // Recorded, but nothing moved: known to be calm, not unknown.
            talking_answer(&net(60, 0), None),
        ];
        rank(&mut answers);
        // The unknown keep their own order behind the known.
        assert_eq!(order(&answers), [Question::Talking, Question::Disk, Question::Slowing]);
        assert_eq!(answers[0].severity, Some(0.));
        assert_eq!(answers[0].text, "nothing moved in the last hour · connections not recorded");
        assert_eq!((answers[1].severity, answers[1].text.as_str()), (None, "not read yet"));
        assert_eq!((answers[2].severity, answers[2].text.as_str()), (None, NOT_RECORDED));
        assert_eq!(talking_answer(&net(0, 0), Some(3)).text, NOT_RECORDED);
        // A direction never read is not shown as a total.
        let mut half = net(60, 6_000_000);
        half.interfaces[0].sent = None;
        let half = talking_answer(&half, None);
        assert_eq!(
            half.text,
            "wlan0 the busiest interface in the last hour; what it moved — · connections not recorded"
        );
        assert!(half.severity.is_some_and(|s| s > 0.));

        // CPU read but memory not: the unknown share says —, never 0%.
        let cpu_only = slowing_answer(&perf(60, Some(0.4), None, None));
        assert_eq!(cpu_only.severity, Some(0.4));
        assert_eq!(cpu_only.text, "CPU 40% busy, memory — taken in the last hour · busiest process —");
    }
}
