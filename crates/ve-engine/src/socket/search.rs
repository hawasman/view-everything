//! The `search` method: Entries a Query selects under a path, as Rows. The same selection runs
//! on the Engine's live Index (over the socket) and on a saved Index file (the CLI's fallback).

use crate::{Engine, Phase};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::Path;
use ve_core::{CategoryMap, EntryId, Filter, Index, Query, QueryContext};

/// Which Entries a [`Search`] lists, and in what order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Select {
    /// Every Entry the Query matches (every Entry without a Query), by path.
    #[default]
    Matches,
    /// Folders holding files the Query counts, largest Filtered On-disk Size first.
    LargestFolders,
    /// Files the Query counts, largest On-disk Size first.
    LargestFiles,
}

/// What a client asks the `search` method for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Search {
    /// Query text; empty selects everything.
    pub query: String,
    /// Only Entries under this absolute path (not the path itself).
    pub path: Option<String>,
    /// Which Entries, in what order.
    pub select: Select,
    /// At most this many Rows; `None` for all.
    pub limit: Option<usize>,
}

/// One Entry as a client prints it. Sizes are Filtered Sizes under the Search's Query.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// Full path, lossily made UTF-8.
    pub path: String,
    pub name: String,
    /// A folder (or a Volume's root).
    pub is_dir: bool,
    /// Logical Size in bytes.
    pub logical_size: u64,
    /// On-disk Size in bytes.
    pub on_disk_size: u64,
    /// Nanoseconds since the Unix epoch; `None` when unknown.
    pub modified_ns: Option<i64>,
    /// `None` for folders.
    pub category: Option<String>,
    /// The Owner's name; `None` without one.
    pub owner: Option<String>,
    /// System, Program, Game or Project; `None` without an Owner.
    pub owner_kind: Option<String>,
}

/// The app's overall Last Synced: its least recently synced Location's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastSynced {
    /// The Index has no Locations.
    NoLocations,
    /// A Location has not finished its first Sync.
    NotYet,
    /// Seconds since the Unix epoch.
    At(i64),
}

impl LastSynced {
    /// The oldest Last Synced of `index`'s Locations.
    pub fn of(index: &Index) -> LastSynced {
        match index.oldest_synced() {
            None => LastSynced::NoLocations,
            Some(None) => LastSynced::NotYet,
            Some(Some(at)) => LastSynced::At(at),
        }
    }
}

/// What a Search found, and how current the Index it read was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// In the Search's order.
    pub rows: Vec<Row>,
    pub last_synced: LastSynced,
}

/// Runs `search` on `index` with an already parsed Query.
pub fn run(index: &Index, categories: &CategoryMap, query: Option<&Query>, search: &Search) -> Result<Vec<Row>, String> {
    let filter = Filter::compute(index, query);
    let mut ids = select(index, &filter, query, search.path.as_deref().map(Path::new), search.select)?;
    if let Some(limit) = search.limit {
        ids.truncate(limit);
    }
    Ok(ids.into_iter().map(|id| row(index, &filter, categories, id)).collect())
}

/// The Entries under `path` (everywhere without one) that `select` lists, in its order.
pub(crate) fn select(
    index: &Index,
    filter: &Filter,
    query: Option<&Query>,
    path: Option<&Path>,
    select: Select,
) -> Result<Vec<EntryId>, String> {
    let scope = scope(index, path)?;
    let mut ids: Vec<EntryId> = match select {
        Select::Matches => scope.into_iter().filter(|&id| query.is_none() || filter.is_matched(id)).collect(),
        Select::LargestFiles => scope.into_iter().filter(|&id| !index.is_dir(id) && filter.is_counted(id)).collect(),
        Select::LargestFolders => scope
            .into_iter()
            .filter(|&id| index.is_dir(id) && filter.totals(index, id).files > 0)
            .collect(),
    };
    if select == Select::Matches {
        ids.sort_by_cached_key(|&id| index.path(id));
    } else {
        ids.sort_by_cached_key(|&id| (std::cmp::Reverse(filter.totals(index, id).on_disk), index.path(id)));
    }
    Ok(ids)
}

pub(crate) fn row(index: &Index, filter: &Filter, categories: &CategoryMap, id: EntryId) -> Row {
    let totals = filter.totals(index, id);
    let name = index.name(id);
    let owner = index.entry_owner(id);
    Row {
        path: index.path(id).to_string_lossy().into_owned(),
        name: name.to_owned(),
        is_dir: index.is_dir(id),
        logical_size: totals.logical,
        on_disk_size: totals.on_disk,
        modified_ns: Some(index.modified(id)).filter(|&ns| ns != 0),
        category: (!index.is_dir(id)).then(|| categories.of_name(name).name().to_owned()),
        owner: owner.map(|o| o.name.clone()),
        owner_kind: owner.map(|o| o.kind.name().to_owned()),
    }
}

/// Live Entries under `path` (not `path` itself), or every live Entry without one.
pub(crate) fn scope(index: &Index, path: Option<&Path>) -> Result<Vec<EntryId>, String> {
    let Some(path) = path else {
        return Ok((0..index.capacity() as u32).map(EntryId).filter(|&id| index.is_live(id)).collect());
    };
    let root = index
        .find_path(path)
        .ok_or_else(|| format!("{} is not in the Index", path.display()))?;
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for child in index.children(dir) {
            if index.is_live(child) {
                found.push(child);
                if index.is_dir(child) {
                    stack.push(child);
                }
            }
        }
    }
    Ok(found)
}

impl Engine {
    /// Runs `search` on the live Index (the `search` method). `dupe:`, `is:duplicate` and
    /// `is:cleanable` use the Duplicates and Cleanables found so far; `is:offline` the Volumes
    /// mounted now. `Err` is a reason for the client: a bad Query, a path not in the Index, or
    /// the Index still loading.
    pub fn find_rows(&self, search: &Search) -> Result<Found, String> {
        let shared = &self.shared;
        if self.is_loading_index() {
            return Err(LOADING.into());
        }
        let query = self.parse_query(&search.query).map_err(|e| format!("bad Query: {e}"))?;
        // Rows are built under the read lock and sent after it is released, so a slow reader
        // never holds up a Sync.
        let index = shared.index.read();
        let rows = run(&index, &shared.categories, query.as_ref(), search)?;
        Ok(Found {
            rows,
            last_synced: LastSynced::of(&index),
        })
    }

    /// Whether the saved Index is still being read: nothing can be answered from it yet.
    pub(crate) fn is_loading_index(&self) -> bool {
        matches!(self.shared.status.lock().phase, Phase::LoadingIndex)
    }

    /// Parses Query `text` as the app knows it now: `dupe:`, `is:duplicate` and `is:cleanable`
    /// use the Duplicates and Cleanables found so far. `None` for empty text.
    pub(crate) fn parse_query(&self, text: &str) -> Result<Option<Query>, String> {
        let shared = &self.shared;
        match text.trim() {
            "" => Ok(None),
            text => {
                let context = QueryContext::new(shared.categories.clone(), Local::now())
                    .with_duplicates(shared.duplicates.lock().set.clone())
                    .with_cleanables(shared.cleanables.lock().set.clone());
                Query::parse(text, &context).map(Some).map_err(|e| e.to_string())
            }
        }
    }
}

/// Why the engine cannot answer yet.
pub(crate) const LOADING: &str = "the app is still loading its Index; try again in a moment";
