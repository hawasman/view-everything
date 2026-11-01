//! The Apps Module's engine side: `apps.list`, the installed programs with their On-disk Size.
//! A program's size is the Folder Size of its Owner roots in the Index (the folders Owner
//! detection gave its Owner), so it matches what Browse shows; a program none of whose
//! folders is indexed has no size rather than a guess.

mod readers;

pub use readers::{AppKind, Program, Sources};

use crate::{Engine, Shared};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use ve_core::{EntryId, Index, Owner};

/// One installed program with what the Index knows of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    pub program: Program,
    /// Its Owner roots in the Index with their Folder Size (On-disk), largest first. A root
    /// inside another of them is left out, as its size is in that one's.
    pub roots: Vec<(PathBuf, u64)>,
    /// The sum of `roots`; `None` when no folder of the program is indexed.
    pub size: Option<u64>,
}

impl App {
    /// Tells programs apart across sources: `<source>:<id>`.
    pub fn key(&self) -> String {
        format!("{}:{}", self.program.source, self.program.id)
    }
}

/// Sizes `programs` from their Owner roots in `index`.
pub fn size(index: &Index, programs: &[Program]) -> Vec<App> {
    let mut by_owner: HashMap<&Owner, Vec<EntryId>> = HashMap::new();
    for (root, owner) in index.owner_roots() {
        by_owner.entry(index.owner(owner)).or_default().push(root);
    }
    programs
        .iter()
        .map(|program| {
            let ids = by_owner.get(&program.owner).map_or(&[][..], Vec::as_slice);
            // A hard-linked file counts once across all of the program's roots.
            let mut links = HashSet::new();
            let mut roots: Vec<(PathBuf, u64)> = ids
                .iter()
                .filter(|&&id| !std::iter::successors(index.parent(id), |&p| index.parent(p)).any(|a| ids.contains(&a)))
                .map(|&id| (index.path(id), folder_size(index, id, &mut links)))
                .collect();
            roots.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            App {
                program: program.clone(),
                size: (!roots.is_empty()).then(|| roots.iter().map(|r| r.1).sum()),
                roots,
            }
        })
        .collect()
}

/// The On-disk Folder Size of `root`, skipping hard-linked files already in `links`.
fn folder_size(index: &Index, root: EntryId, links: &mut HashSet<u64>) -> u64 {
    let mut total = 0;
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if index.is_dir(id) {
            stack.extend(index.children(id));
        } else if !index.is_hardlinked(id) || links.insert(index.key(id)) {
            total += index.on_disk(id);
        }
    }
    total
}

/// The Engine's Apps: where programs are read from, and the latest list.
pub struct Apps {
    sources: Mutex<Option<Sources>>,
    cache: Mutex<Cache>,
}

/// What the programs were read under: the sources, every Location's Last Synced (stamped once
/// Owner detection of that Location finished) and how many Owner roots the Index holds (so two
/// detections within one second still tell apart).
type ReadKey = (Option<Sources>, Vec<Option<i64>>, usize);

#[derive(Default)]
struct Cache {
    programs: Option<(ReadKey, Arc<Vec<Program>>)>,
    /// The list sized from `programs`, with the Index generation it was sized at.
    apps: Option<(u64, Arc<Vec<App>>)>,
}

impl Default for Apps {
    fn default() -> Self {
        Self::new()
    }
}

impl Apps {
    /// Reads the running system's programs ([`Sources::local`]).
    pub fn new() -> Apps {
        Apps {
            sources: Mutex::new(Sources::local()),
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Reads programs from `sources` from now on; `None` lists none.
    pub fn set_sources(&self, sources: Option<Sources>) {
        *self.sources.lock() = sources;
    }

    /// The installed programs, sized from the Index as it is now. The sources are read again
    /// once a Location's Owner detection has finished since the last read; otherwise only the
    /// sizes are redone, and only when the Index changed. Reads files without an Index lock.
    pub fn list(&self, shared: &Shared) -> Arc<Vec<App>> {
        let mut cache = self.cache.lock();
        let sources = self.sources.lock().clone();
        let (synced, roots) = {
            let index = shared.index.read();
            let synced: Vec<Option<i64>> = index.volumes().map(|(_, v)| v.last_synced).collect();
            (synced, index.owner_roots().count())
        };
        let key = (sources, synced, roots);
        let programs = match &cache.programs {
            Some((read_under, programs)) if *read_under == key => programs.clone(),
            _ => {
                let programs = Arc::new(key.0.as_ref().map(readers::read).unwrap_or_default());
                cache.programs = Some((key, programs.clone()));
                cache.apps = None;
                programs
            }
        };
        let index = shared.index.read();
        let generation = index.generation();
        if let Some((sized_at, apps)) = &cache.apps
            && *sized_at == generation
        {
            return apps.clone();
        }
        let apps = Arc::new(size(&index, &programs));
        drop(index);
        cache.apps = Some((generation, apps.clone()));
        apps
    }
}

impl Engine {
    /// `apps.list`: every installed program with its On-disk Size from its Owner roots,
    /// refreshed after Owner detection. See [`Apps::list`].
    pub fn apps_list(&self) -> Arc<Vec<App>> {
        self.apps.list(&self.shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use ve_core::owners;
    use ve_core::{EntryMeta, SyncState};

    fn program(name: &str, owner: Owner) -> Program {
        Program {
            name: name.into(),
            kind: AppKind::Program,
            source: "/opt",
            id: name.into(),
            version: None,
            installed: None,
            owner,
        }
    }

    #[test]
    fn size_sums_outer_roots_counts_links_once_and_is_none_without_any() {
        let mut index = Index::new();
        let dir = EntryMeta {
            is_dir: true,
            ..EntryMeta::default()
        };
        let file = |n| EntryMeta {
            logical: n,
            on_disk: n,
            ..EntryMeta::default()
        };
        let link = EntryMeta {
            key: 7,
            hardlinked: true,
            ..file(1000)
        };
        let v = index.add_volume(Path::new("/"), "ext4", "id", dir, SyncState::Walk);
        let root = index.volume(v).unwrap().root;
        let a = index.insert(root, "a", dir);
        index.insert(a, "big", file(8000));
        index.insert(a, "link", link);
        let nested = index.insert(a, "nested", dir);
        index.insert(nested, "x", file(400));
        let b = index.insert(root, "b", dir);
        index.insert(b, "y", file(30));
        index.insert(b, "same-link", link);
        let game = owners::steam_owner("Game");
        index.set_owners(root, vec![(a, game.clone()), (nested, game.clone()), (b, game.clone())]);
        let apps = size(&index, &[program("Game", game), program("Gone", owners::opt_owner("gone"))]);
        // `nested` is inside `a`; the link counts in `a` only.
        assert_eq!(apps[0].roots, [(PathBuf::from("/a"), 9400), (PathBuf::from("/b"), 30)]);
        assert_eq!(apps[0].size, Some(9430));
        assert_eq!((apps[1].size, apps[1].roots.len()), (None, 0));
    }
}
