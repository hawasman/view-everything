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
    /// The folder everything shown sits in, which the Treemap draws: the deepest folder of the
    /// Volume with the most Filtered Size that still holds all of that Volume's shown Entries.
    pub focus: Option<EntryId>,
    pub took: Duration,
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
    sorted: Vec<(u64, u32)>,
}

impl FileOrder {
    /// Sorts the files of `index` unless this is already their generation.
    pub fn ensure(&mut self, index: &Index) {
        if self.generation == Some(index.generation()) {
            return;
        }
        self.sorted = (0..index.capacity())
            .into_par_iter()
            .map(|i| EntryId(i as u32))
            .filter(|&id| index.is_live(id) && !index.is_dir(id) && index.parent(id).is_some())
            .map(|id| (index.on_disk(id), id.0))
            .collect();
        // Stable, and collected in id order: equal sizes list in id order.
        self.sorted.par_sort_by_key(|&(size, _)| std::cmp::Reverse(size));
        self.generation = Some(index.generation());
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
        order.ensure(index);
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
        let files = &order.sorted;
        let slots: Vec<(usize, u32)> = dirs
            .iter()
            .map(|&(size, dir)| (files.partition_point(|&(s, _)| s >= size), dir))
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
                for (at, &(_, id)) in (from..to).zip(&files[from..to]) {
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
        let mut total = Totals::default();
        for (_, volume) in index.volumes() {
            total.add(filter.totals(index, volume.root));
        }

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

/// See [`View::focus`]. The descent goes through folders shown only for what they contain and
/// stops above one the Query matches itself, so a zoomed-in Query (`"/home/me/*"`, which matches
/// what is inside the folder) lands on that folder.
fn focus(index: &Index, filter: &Filter) -> Option<EntryId> {
    let roots = || index.volumes().map(|(_, v)| v.root).filter(|&id| index.is_live(id));
    let mut folder = roots()
        .filter(|&id| filter.is_shown(id))
        .max_by_key(|&id| filter.totals(index, id).on_disk)
        .or_else(|| roots().next())?;
    loop {
        let mut shown = index
            .children(folder)
            .iter()
            .copied()
            .filter(|&c| index.is_live(c) && filter.is_shown(c));
        match (shown.next(), shown.next()) {
            (Some(only), None) if index.is_dir(only) && !filter.is_matched(only) => folder = only,
            _ => return Some(folder),
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
}
