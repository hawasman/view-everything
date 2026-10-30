//! What the panels show for one Query over one Index generation.

use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use ve_core::category::extension;
use ve_core::{Category, CategoryMap, EntryId, Filter, Index, Query, Totals};

/// Extensions listed under each Category in the breakdown.
pub const EXTENSIONS_PER_CATEGORY: usize = 12;

static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);

/// Whether two optional Views are the very same computation.
pub fn same(a: &Option<std::sync::Arc<View>>, b: &Option<std::sync::Arc<View>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

pub struct View {
    /// Unique per computed View; caches of derived drawing data key on it.
    pub id: u64,
    /// The Query text this View was computed for.
    pub query: String,
    pub filter: Filter,
    /// Entries that match the Query themselves, largest On-disk Size first.
    pub flat: Vec<EntryId>,
    /// Filtered Size per Category, largest first.
    pub categories: Vec<CategoryRow>,
    /// Filtered Size over every Indexed Volume.
    pub total: Totals,
    /// What the Treemap draws: the deepest folder that holds everything shown, or the whole
    /// Index when that spans several Locations.
    pub focus: Focus,
    pub took: Duration,
}

/// What the Treemap draws: one folder, or the whole Index with one tile per Location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Focus {
    Index,
    Folder(EntryId),
}

impl Focus {
    /// The folder drawn; None for the whole Index.
    pub fn folder(self) -> Option<EntryId> {
        match self {
            Focus::Index => None,
            Focus::Folder(id) => Some(id),
        }
    }

    /// What is drawn directly inside: the folder's children, or the root of every Location.
    pub fn children(self, index: &Index) -> std::borrow::Cow<'_, [EntryId]> {
        match self {
            Focus::Index => index.volumes().map(|(_, v)| v.root).filter(|&id| index.is_live(id)).collect(),
            Focus::Folder(id) => index.children(id).into(),
        }
    }

    /// The Filtered Size drawn: the folder's, or that of every Location together.
    pub fn totals(self, index: &Index, filter: &Filter) -> Totals {
        match self {
            Focus::Index => {
                let mut total = Totals::default();
                for &root in self.children(index).iter() {
                    total.add(filter.totals(index, root));
                }
                total
            }
            Focus::Folder(id) => filter.totals(index, id),
        }
    }

    /// What the Treemap and its breadcrumb call it.
    pub fn name(self, index: &Index) -> &str {
        match self {
            Focus::Index => "Index",
            Focus::Folder(id) => index.display_name(id),
        }
    }
}

#[derive(Clone)]
pub struct CategoryRow {
    pub category: Category,
    pub totals: Totals,
    /// Largest extensions in this Category.
    pub extensions: Vec<(String, Totals)>,
}

/// Every file of an Index generation, largest On-disk Size first, kept between Queries: a file
/// matched by a Query keeps its size, so a Query's list is this one filtered rather than sorted
/// again. Only folders (whose size is the Query's Filtered Size) are sorted per Query.
#[derive(Default)]
pub struct FileOrder {
    generation: Option<u64>,
    /// On-disk Sizes, largest first; `ids[i]` is the file of `sizes[i]`. Two Vecs, 12 bytes a file.
    sizes: Vec<u64>,
    ids: Vec<u32>,
}

/// Entries looked at between two checks for cancellation.
const COLLECT_CHUNK: usize = 1 << 18;

impl FileOrder {
    /// Sorts the files of `index` unless this is already their generation. `false` when
    /// `cancelled` stopped it; the order is then unchanged.
    pub fn ensure(&mut self, index: &Index, cancelled: &(dyn Fn() -> bool + Sync)) -> bool {
        if self.generation == Some(index.generation()) {
            return true;
        }
        let generation = index.generation();
        match Self::collect(index, cancelled) {
            Some(files) => self.install(generation, files, cancelled),
            None => false,
        }
    }

    /// Like [`FileOrder::ensure`], but holds the read lock only while the files are collected, not
    /// while they are sorted, so writers and Queries are not kept waiting for the sort.
    pub fn ensure_shared(&mut self, index: &ve_scan::SharedIndex, cancelled: &(dyn Fn() -> bool + Sync)) -> bool {
        let (generation, files) = {
            let index = index.read();
            if self.generation == Some(index.generation()) {
                return true;
            }
            let Some(files) = Self::collect(&index, cancelled) else {
                return false;
            };
            (index.generation(), files)
        };
        self.install(generation, files, cancelled)
    }

    fn collect(index: &Index, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Vec<(u64, u32)>> {
        let capacity = index.capacity();
        let mut files = Vec::new();
        for from in (0..capacity).step_by(COLLECT_CHUNK) {
            if cancelled() {
                return None;
            }
            let chunk: Vec<(u64, u32)> = (from..(from + COLLECT_CHUNK).min(capacity))
                .into_par_iter()
                .map(|i| EntryId(i as u32))
                .filter(|&id| index.is_live(id) && !index.is_dir(id) && index.parent(id).is_some())
                .map(|id| (index.on_disk(id), id.0))
                .collect();
            files.extend(chunk);
        }
        Some(files)
    }

    fn install(&mut self, generation: u64, mut files: Vec<(u64, u32)>, cancelled: &(dyn Fn() -> bool + Sync)) -> bool {
        // Stable, and collected in id order: equal sizes list in id order.
        files.par_sort_by_key(|&(size, _)| std::cmp::Reverse(size));
        if cancelled() {
            return false;
        }
        self.sizes = files.iter().map(|&(size, _)| size).collect();
        self.ids = files.iter().map(|&(_, id)| id).collect();
        self.generation = Some(generation);
        true
    }
}

impl View {
    #[cfg(test)]
    pub fn compute(index: &Index, query_text: &str, query: Option<&Query>, categories: &CategoryMap, order: &mut FileOrder) -> View {
        Self::compute_until(index, query_text, query, categories, order, &|| false).expect("never cancelled")
    }

    /// Like [`View::compute`], but gives up with `None` once `cancelled` says so (checked between
    /// phases and inside the Filter), so a Query typed over costs no more time.
    pub fn compute_until(
        index: &Index,
        query_text: &str,
        query: Option<&Query>,
        categories: &CategoryMap,
        order: &mut FileOrder,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<View> {
        let started = Instant::now();
        let filter = Filter::compute_until(index, query, cancelled)?;
        if !order.ensure(index, cancelled) {
            return None;
        }
        if cancelled() {
            return None;
        }

        let mut dirs: Vec<(u64, u32)> = (0..index.capacity())
            .into_par_iter()
            .map(|i| EntryId(i as u32))
            .filter(|&id| filter.is_matched(id) && index.is_live(id) && index.is_dir(id) && index.parent(id).is_some())
            .map(|id| (filter.totals(index, id).on_disk, id.0))
            .collect();
        dirs.par_sort_by_key(|&(size, _)| std::cmp::Reverse(size));
        // Where each folder slots into the file order: after the files of equal size.
        let files = &order.sizes;
        let slots: Vec<(usize, u32)> = dirs
            .iter()
            .map(|&(size, dir)| (files.partition_point(|&s| s >= size), dir))
            .collect();
        // Each worker turns one stretch of the file order into the matching files and the folders
        // that fall in it; the stretches join in order.
        const STRETCH: usize = 1 << 16;
        let stretches = files.len().div_ceil(STRETCH).max(1);
        let parts: Vec<Vec<EntryId>> = (0..stretches)
            .into_par_iter()
            .map(|n| {
                let (from, to) = (n * STRETCH, ((n + 1) * STRETCH).min(files.len()));
                let last = n + 1 == stretches;
                let mut next = slots.partition_point(|&(at, _)| at < from);
                let mut out = Vec::with_capacity(to - from);
                for (at, &id) in (from..to).zip(&order.ids[from..to]) {
                    while next < slots.len() && slots[next].0 == at {
                        out.push(EntryId(slots[next].1));
                        next += 1;
                    }
                    if filter.is_matched(EntryId(id)) {
                        out.push(EntryId(id));
                    }
                }
                if last {
                    out.extend(slots[next..].iter().map(|&(_, dir)| EntryId(dir)));
                }
                out
            })
            .collect();
        let mut flat = Vec::with_capacity(parts.iter().map(Vec::len).sum());
        for part in parts {
            flat.extend(part);
        }
        if cancelled() {
            return None;
        }
        let total = Focus::Index.totals(index, &filter);

        Some(View {
            id: NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed),
            query: query_text.to_owned(),
            categories: breakdown(index, &filter, categories),
            focus: focus(index, &filter),
            filter,
            flat,
            total,
            took: started.elapsed(),
        })
    }
}

/// See [`View::focus`]. With shown Entries on one Location only, the focus is [`descend`] from
/// its root.
fn focus(index: &Index, filter: &Filter) -> Focus {
    let mut shown = index
        .volumes()
        .map(|(_, v)| v.root)
        .filter(|&id| index.is_live(id) && filter.is_shown(id));
    match (shown.next(), shown.next()) {
        (Some(root), None) => Focus::Folder(descend(index, filter, root)),
        _ => Focus::Index,
    }
}

/// The deepest folder under `folder` that holds everything it shows: the descent goes through
/// folders shown only for what they contain and stops above one the Query matches itself, so a
/// zoomed-in Query (`"/home/me/*"`, which matches what is inside the folder) lands on that folder.
pub fn descend(index: &Index, filter: &Filter, mut folder: EntryId) -> EntryId {
    loop {
        let mut shown = index
            .children(folder)
            .iter()
            .copied()
            .filter(|&c| index.is_live(c) && filter.is_shown(c));
        match (shown.next(), shown.next()) {
            (Some(only), None) if index.is_dir(only) && !filter.is_matched(only) => folder = only,
            _ => return folder,
        }
    }
}

/// Filtered Size per Category and extension, counting each hard-linked file once.
fn breakdown(index: &Index, filter: &Filter, categories: &CategoryMap) -> Vec<CategoryRow> {
    // Each worker folds its slice of the Index into private maps; the maps merge afterwards.
    type Maps<'a> = (HashMap<&'a str, Totals>, HashMap<String, Totals>);
    let (mut by_extension, lowered): Maps = (0..index.capacity())
        .into_par_iter()
        .fold(
            || (HashMap::new(), HashMap::new()),
            |mut maps: Maps, i| {
                let id = EntryId(i as u32);
                if !index.is_live(id) || index.is_dir(id) || !filter.is_counted(id) || filter.is_repeated_link(id) {
                    return maps;
                }
                let ext = extension(index.name(id)).unwrap_or("");
                let entry = if ext.bytes().any(|b| b.is_ascii_uppercase()) {
                    maps.1.entry(ext.to_ascii_lowercase()).or_default()
                } else {
                    maps.0.entry(ext).or_default()
                };
                entry.add(Totals {
                    logical: index.logical(id),
                    on_disk: index.on_disk(id),
                    files: 1,
                });
                maps
            },
        )
        .reduce(
            || (HashMap::new(), HashMap::new()),
            |mut a, b| {
                for (k, t) in b.0 {
                    a.0.entry(k).or_default().add(t);
                }
                for (k, t) in b.1 {
                    a.1.entry(k).or_default().add(t);
                }
                a
            },
        );
    for (ext, totals) in &lowered {
        by_extension.entry(ext.as_str()).or_default().add(*totals);
    }

    let mut rows: HashMap<Category, CategoryRow> = HashMap::new();
    for (ext, totals) in by_extension {
        let category = categories.of_extension(ext);
        let row = rows.entry(category).or_insert_with(|| CategoryRow {
            category,
            totals: Totals::default(),
            extensions: Vec::new(),
        });
        row.totals.add(totals);
        let label = if ext.is_empty() { "(none)".to_owned() } else { ext.to_owned() };
        row.extensions.push((label, totals));
    }
    let mut rows: Vec<CategoryRow> = rows.into_values().collect();
    for row in &mut rows {
        row.extensions.sort_unstable_by_key(|(_, t)| std::cmp::Reverse(t.on_disk));
        row.extensions.truncate(EXTENSIONS_PER_CATEGORY);
    }
    rows.sort_unstable_by_key(|r| std::cmp::Reverse(r.totals.on_disk));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use ve_core::{EntryMeta, SyncState};

    #[::core::prelude::v1::test]
    fn the_flat_list_is_largest_first_with_folders_among_the_files() {
        let mut index = Index::new();
        let dir = EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let file = |size: u64| EntryMeta {
            logical: size,
            on_disk: size,
            ..Default::default()
        };
        let volume = index.add_volume(Path::new("/v"), "ext4", "v", dir, SyncState::Walk);
        let root = index.volume(volume).unwrap().root;
        let small = index.insert(root, "a.rs", file(10));
        let folder = index.insert(root, "d", dir);
        let mid = index.insert(folder, "b.rs", file(50));
        let big = index.insert(root, "c.rs", file(100));
        let tie = index.insert(root, "e.rs", file(10));
        let categories = CategoryMap::builtin();
        let mut order = FileOrder::default();
        let view = View::compute(&index, "", None, &categories, &mut order);
        // `d` holds 50 bytes, as much as `b.rs`; a folder follows the files of its size.
        assert_eq!(view.flat, vec![big, mid, folder, small, tie]);
        // The same order serves a Query: only matches, sizes unchanged.
        let ctx = ve_core::QueryContext::new(categories.clone(), chrono::Local::now());
        let query = Query::parse("ext:rs !b", &ctx).unwrap();
        let view = View::compute(&index, "ext:rs !b", Some(&query), &categories, &mut order);
        assert_eq!(view.flat, vec![big, small, tie]);
    }

    #[::core::prelude::v1::test]
    fn matches_on_several_locations_draw_the_whole_index() {
        let mut index = Index::new();
        let dir = EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let file = EntryMeta {
            logical: 10,
            on_disk: 10,
            ..Default::default()
        };
        let ssd = index.add_volume(Path::new("/mnt/ssd"), "ext4", "ssd", dir, SyncState::Walk);
        let home = index.add_volume(Path::new("/home"), "ext4", "home", dir, SyncState::Walk);
        let (ssd, home) = (index.volume(ssd).unwrap().root, index.volume(home).unwrap().root);
        let games = index.insert(ssd, "games", dir);
        let poe2 = index.insert(games, "Path of Exile 2", dir);
        index.insert(poe2, "game.bin", file);
        index.insert(games, "other.bin", file);
        let poe1 = index.insert(home, "Path of Exile", dir);
        index.insert(poe1, "game.bin", file);
        let categories = CategoryMap::builtin();
        let ctx = ve_core::QueryContext::new(categories.clone(), chrono::Local::now());
        let focus = |text: &str| {
            let query = Query::parse(text, &ctx).unwrap();
            View::compute(&index, text, Some(&query), &categories, &mut FileOrder::default()).focus
        };
        // Path of Exile is on both Volumes: neither Volume holds everything shown.
        assert_eq!(focus("path of exile"), Focus::Index);
        // An empty Query shows both Volumes.
        assert_eq!(
            View::compute(&index, "", None, &categories, &mut FileOrder::default()).focus,
            Focus::Index
        );
        // Matches on one Volume only: the deepest folder holding them, as before.
        assert_eq!(focus("other"), Focus::Folder(games));
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;
    use std::path::Path;
    use ve_core::{EntryMeta, SyncState};

    fn big_index(files: usize) -> (Index, EntryId) {
        let mut index = Index::new();
        let dir = EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let volume = index.add_volume(Path::new("/v"), "ext4", "v", dir, SyncState::Walk);
        let root = index.volume(volume).unwrap().root;
        for i in 0..files {
            let size = (i % 1000) as u64 + 1;
            index.insert(
                root,
                &format!("f{i}.rs"),
                EntryMeta {
                    logical: size,
                    on_disk: size,
                    ..Default::default()
                },
            );
        }
        (index, root)
    }

    #[::core::prelude::v1::test]
    fn a_cancelled_compute_gives_nothing_and_leaves_the_order_unset() {
        let (index, _) = big_index(10);
        let mut order = FileOrder::default();
        let categories = CategoryMap::builtin();
        assert!(View::compute_until(&index, "", None, &categories, &mut order, &|| true).is_none());
        assert!(order.generation.is_none());
        assert!(!order.ensure(&index, &|| true));
        assert!(order.ensure(&index, &|| false));
        assert_eq!(order.sizes.len(), 10);
    }

    #[::core::prelude::v1::test]
    fn the_list_is_sorted_across_stretch_boundaries() {
        // More files than one stretch (65,536) so the parallel merge joins several.
        let (index, root) = big_index(140_000);
        let mut order = FileOrder::default();
        let view = View::compute(&index, "", None, &CategoryMap::builtin(), &mut order);
        // 140,000 files plus the root folder, which is not listed.
        assert_eq!(view.flat.len(), 140_000);
        assert!(view.flat.iter().all(|&id| id != root));
        let sizes: Vec<u64> = view.flat.iter().map(|&id| index.on_disk(id)).collect();
        assert!(sizes.windows(2).all(|w| w[0] >= w[1]));
        // Rebuilt from the shared lock: same generation, nothing to do.
        let shared = ve_scan::SharedIndex::new(index);
        let mut fresh = FileOrder::default();
        assert!(fresh.ensure_shared(&shared, &|| false));
        assert_eq!(fresh.ids.len(), 140_000);
    }
}
