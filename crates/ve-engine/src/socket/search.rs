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
    let scope = scope(index, search.path.as_deref().map(Path::new))?;
    let mut ids: Vec<EntryId> = match search.select {
        Select::Matches => scope.into_iter().filter(|&id| query.is_none() || filter.is_matched(id)).collect(),
        Select::LargestFiles => scope.into_iter().filter(|&id| !index.is_dir(id) && filter.is_counted(id)).collect(),
        Select::LargestFolders => scope
            .into_iter()
            .filter(|&id| index.is_dir(id) && filter.totals(index, id).files > 0)
            .collect(),
    };
    if search.select == Select::Matches {
        ids.sort_by_cached_key(|&id| index.path(id));
    } else {
        ids.sort_by_cached_key(|&id| (std::cmp::Reverse(filter.totals(index, id).on_disk), index.path(id)));
    }
    if let Some(limit) = search.limit {
        ids.truncate(limit);
    }
    Ok(ids.into_iter().map(|id| row(index, &filter, categories, id)).collect())
}

fn row(index: &Index, filter: &Filter, categories: &CategoryMap, id: EntryId) -> Row {
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
fn scope(index: &Index, path: Option<&Path>) -> Result<Vec<EntryId>, String> {
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
        if matches!(shared.status.lock().phase, Phase::LoadingIndex) {
            return Err("the app is still loading its Index; try again in a moment".into());
        }
        let query = match search.query.trim() {
            "" => None,
            text => {
                let context = QueryContext::new(shared.categories.clone(), Local::now())
                    .with_duplicates(shared.duplicates.lock().set.clone())
                    .with_cleanables(shared.cleanables.lock().set.clone());
                Some(Query::parse(text, &context).map_err(|e| format!("bad Query: {e}"))?)
            }
        };
        // Rows are built under the read lock and sent after it is released, so a slow reader
        // never holds up a Sync.
        let index = shared.index.read();
        let rows = run(&index, &shared.categories, query.as_ref(), search)?;
        Ok(Found {
            rows,
            last_synced: LastSynced::of(&index),
        })
    }
}
