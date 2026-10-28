//! What the panels show for one Query over one Index generation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use ve_core::category::extension;
use ve_core::{Category, CategoryMap, EntryId, Filter, Index, Query, Totals};

/// Extensions listed under each Category in the breakdown.
const EXTENSIONS_PER_CATEGORY: usize = 12;

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
    pub took: Duration,
}

pub struct CategoryRow {
    pub category: Category,
    pub totals: Totals,
    /// Largest extensions in this Category.
    pub extensions: Vec<(String, Totals)>,
}

impl View {
    pub fn compute(
        index: &Index,
        query_text: &str,
        query: Option<&Query>,
        categories: &CategoryMap,
    ) -> View {
        let started = Instant::now();
        let filter = Filter::compute(index, query);

        let mut sized: Vec<(u64, EntryId)> = filter
            .matched()
            .iter()
            .map(|i| EntryId(i as u32))
            .filter(|&id| index.is_live(id) && index.parent(id).is_some())
            .map(|id| (filter.totals(index, id).on_disk, id))
            .collect();
        sized.sort_unstable_by_key(|&(size, _)| std::cmp::Reverse(size));
        let flat = sized.into_iter().map(|(_, id)| id).collect();

        let mut total = Totals::default();
        for (_, volume) in index.volumes() {
            total.add(filter.totals(index, volume.root));
        }

        View {
            id: NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed),
            query: query_text.to_owned(),
            categories: breakdown(index, &filter, categories),
            filter,
            flat,
            total,
            took: started.elapsed(),
        }
    }
}

/// Filtered Size per Category and extension, counting each hard-linked file once.
fn breakdown(index: &Index, filter: &Filter, categories: &CategoryMap) -> Vec<CategoryRow> {
    let mut by_extension: HashMap<&str, Totals> = HashMap::new();
    let mut lowered: HashMap<String, Totals> = HashMap::new();
    for i in 0..index.capacity() {
        let id = EntryId(i as u32);
        if !index.is_live(id)
            || index.is_dir(id)
            || !filter.is_counted(id)
            || filter.is_repeated_link(id)
        {
            continue;
        }
        let ext = extension(index.name(id)).unwrap_or("");
        let entry = if ext.bytes().any(|b| b.is_ascii_uppercase()) {
            lowered.entry(ext.to_ascii_lowercase()).or_default()
        } else {
            by_extension.entry(ext).or_default()
        };
        entry.add(Totals {
            logical: index.logical(id),
            on_disk: index.on_disk(id),
            files: 1,
        });
    }
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
        let label = if ext.is_empty() {
            "(none)".to_owned()
        } else {
            ext.to_owned()
        };
        row.extensions.push((label, totals));
    }
    let mut rows: Vec<CategoryRow> = rows.into_values().collect();
    for row in &mut rows {
        row.extensions
            .sort_unstable_by_key(|(_, t)| std::cmp::Reverse(t.on_disk));
        row.extensions.truncate(EXTENSIONS_PER_CATEGORY);
    }
    rows.sort_unstable_by_key(|r| std::cmp::Reverse(r.totals.on_disk));
    rows
}
