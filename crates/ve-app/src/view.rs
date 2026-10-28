//! What the panels show for one Query over one Index generation.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use ve_core::category::extension;
use ve_core::{Category, CategoryMap, EntryId, Filter, Index, Query, Totals};

/// Extensions listed under each Category in the breakdown.
const EXTENSIONS_PER_CATEGORY: usize = 12;

pub struct View {
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

        let mut flat: Vec<EntryId> = filter
            .matched()
            .iter()
            .map(|i| EntryId(i as u32))
            .filter(|&id| index.is_live(id) && index.parent(id).is_some())
            .collect();
        flat.sort_unstable_by_key(|&id| std::cmp::Reverse(filter.totals(index, id).on_disk));

        let mut total = Totals::default();
        for (_, volume) in index.volumes() {
            let t = filter.totals(index, volume.root);
            total.logical += t.logical;
            total.on_disk += t.on_disk;
            total.files += t.files;
        }

        View {
            query: query_text.to_owned(),
            categories: breakdown(index, &filter, categories),
            filter,
            flat,
            total,
            took: started.elapsed(),
        }
    }
}

/// Filtered Size per Category and extension. A hard-linked file is counted once per link here.
fn breakdown(index: &Index, filter: &Filter, categories: &CategoryMap) -> Vec<CategoryRow> {
    let mut by_extension: HashMap<&str, Totals> = HashMap::new();
    let mut lowered: HashMap<String, Totals> = HashMap::new();
    for i in 0..index.capacity() {
        let id = EntryId(i as u32);
        if !index.is_live(id) || index.is_dir(id) || !filter.is_counted(id) {
            continue;
        }
        let ext = extension(index.name(id)).unwrap_or("");
        let entry = if ext.bytes().any(|b| b.is_ascii_uppercase()) {
            lowered.entry(ext.to_ascii_lowercase()).or_default()
        } else {
            by_extension.entry(ext).or_default()
        };
        entry.logical += index.logical(id);
        entry.on_disk += index.on_disk(id);
        entry.files += 1;
    }
    for (ext, t) in &lowered {
        let entry = by_extension.entry(ext.as_str()).or_default();
        entry.logical += t.logical;
        entry.on_disk += t.on_disk;
        entry.files += t.files;
    }

    let mut rows: HashMap<Category, CategoryRow> = HashMap::new();
    for (ext, totals) in by_extension {
        let category = categories.of_extension(ext);
        let row = rows.entry(category).or_insert_with(|| CategoryRow {
            category,
            totals: Totals::default(),
            extensions: Vec::new(),
        });
        row.totals.logical += totals.logical;
        row.totals.on_disk += totals.on_disk;
        row.totals.files += totals.files;
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
