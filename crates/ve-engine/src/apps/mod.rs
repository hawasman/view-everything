//! The Apps Module's engine side: `apps.list`, the installed programs with their On-disk Size,
//! and `apps.startup`, what starts at login. A program's size is the Folder Size of its Owner
//! roots in the Index (the folders Owner detection gave its Owner), so it matches what Browse
//! shows; a program none of whose folders is indexed has no size rather than a guess.

mod readers;
mod startup;

pub(crate) use readers::group_value;
pub use readers::{AppKind, Launch, Program, Sources};
pub use startup::{Scope, StartupItem, StartupKind};

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

/// What the programs were read under.
#[derive(PartialEq)]
struct ReadKey {
    sources: Option<Sources>,
    /// Every Location's Last Synced, stamped once Owner detection of that Location finished.
    synced: Vec<Option<i64>>,
    /// How many Owner roots the Index holds, so two detections within one second differ.
    owner_roots: usize,
}

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

    /// The installed programs, sized from the Index as it is now: the same list, at no cost,
    /// while the Index has not changed. The sources are read again once a Location's Owner
    /// detection has finished since the last read; otherwise only the sizes are redone. Reads
    /// files without an Index lock.
    pub fn list(&self, shared: &Shared) -> Arc<Vec<App>> {
        let mut cache = self.cache.lock();
        {
            let index = shared.index.read();
            if let (Some((sized_at, apps)), Some((read_under, _))) = (&cache.apps, &cache.programs)
                && *sized_at == index.generation()
                && read_under.sources == *self.sources.lock()
            {
                return apps.clone();
            }
        }
        let programs = self.read(&mut cache, shared);
        let index = shared.index.read();
        let generation = index.generation();
        let apps = Arc::new(size(&index, &programs));
        drop(index);
        cache.apps = Some((generation, apps.clone()));
        apps
    }

    /// The installed programs, unsized: read again under the same rule as [`Apps::list`].
    pub fn programs(&self, shared: &Shared) -> Arc<Vec<Program>> {
        let mut cache = self.cache.lock();
        self.read(&mut cache, shared)
    }

    /// The programs read under the current key, reading the sources when it changed.
    fn read(&self, cache: &mut Cache, shared: &Shared) -> Arc<Vec<Program>> {
        let key = {
            let index = shared.index.read();
            ReadKey {
                sources: self.sources.lock().clone(),
                synced: index.volumes().map(|(_, v)| v.last_synced).collect(),
                owner_roots: index.owner_roots().count(),
            }
        };
        match &cache.programs {
            Some((read_under, programs)) if *read_under == key => programs.clone(),
            _ => {
                let programs = Arc::new(key.sources.as_ref().map(readers::read).unwrap_or_default());
                cache.programs = Some((key, programs.clone()));
                programs
            }
        }
    }

    /// The startup items, read afresh from the sources: a few small files, without an Index
    /// lock.
    pub fn startup(&self) -> Vec<StartupItem> {
        let sources = self.sources.lock().clone();
        sources.as_ref().map(startup::read).unwrap_or_default()
    }
}

impl Engine {
    /// `apps.list`: every installed program with its On-disk Size from its Owner roots,
    /// refreshed after Owner detection. See [`Apps::list`].
    pub fn apps_list(&self) -> Arc<Vec<App>> {
        self.apps.list(&self.shared)
    }

    /// `apps.startup`: what starts at login, XDG autostart entries and enabled systemd user
    /// units, each with what it runs and the file that defines it. See [`Apps::startup`].
    pub fn apps_startup(&self) -> Vec<StartupItem> {
        self.apps.startup()
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
            launch: None,
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

    /// Owner detection's files, from memory: `(path, text)`.
    #[cfg(unix)]
    struct Files(&'static [(&'static str, &'static str)]);

    #[cfg(unix)]
    impl owners::Disk for Files {
        fn read(&self, path: &Path) -> Option<String> {
            self.0
                .iter()
                .find(|(p, _)| Path::new(p) == path)
                .map(|(_, text)| (*text).to_owned())
        }
        fn list(&self, path: &Path) -> Vec<String> {
            let mut names: Vec<String> = self
                .0
                .iter()
                .filter_map(|(p, _)| Path::new(p).strip_prefix(path).ok()?.iter().next()?.to_str().map(str::to_owned))
                .collect();
            names.dedup();
            names
        }
    }

    #[cfg(unix)]
    const MANIFEST: &str = "\"AppState\"\n{\n\t\"appid\"\t\t\"70\"\n\t\"name\"\t\t\"Half-Life\"\n\t\"installdir\"\t\t\"HL\"\n}\n";

    /// The readers name each program's Owner as Owner detection does, so every source's
    /// programs find the roots detection gave them.
    #[test]
    #[cfg(unix)]
    fn programs_read_from_each_source_find_the_roots_detection_found() {
        let dir = tempfile::tempdir().unwrap();
        let (root, home) = (dir.path().join("root"), dir.path().join("home"));
        let write = |path: PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(root.join("var/lib/pacman/local/vim-9.1-1/desc"), "%NAME%\nvim\n");
        std::fs::create_dir_all(root.join("opt/tool")).unwrap();
        let gimp = root.join("var/lib/flatpak/app/org.gimp.GIMP");
        std::fs::create_dir_all(gimp.join("x86_64/stable/active")).unwrap();
        std::os::unix::fs::symlink("x86_64/stable", gimp.join("current")).unwrap();
        write(home.join(".local/share/Steam/steamapps/appmanifest_70.acf"), MANIFEST);
        std::fs::create_dir_all(home.join(".local/share/Steam/steamapps/common/HL")).unwrap();
        let programs = readers::read(&Sources::new(root, Some(home)));

        // An Index of `/` holding each program's folder, its roots found by Owner detection.
        let mut index = Index::new();
        let dir_meta = EntryMeta {
            is_dir: true,
            ..EntryMeta::default()
        };
        let file = EntryMeta {
            logical: 100,
            on_disk: 100,
            ..EntryMeta::default()
        };
        let v = index.add_volume(Path::new("/"), "ext4", "id", dir_meta, SyncState::Walk);
        let volume_root = index.volume(v).unwrap().root;
        for path in [
            "usr/share/vim/vimrc",
            "usr/share/doc/README",
            "opt/tool/bin",
            "var/lib/flatpak/app/org.gimp.GIMP/files/gimp",
            "games/steamapps/common/HL/hl.bin",
            "games/steamapps/appmanifest_70.acf",
        ] {
            let parts: Vec<&str> = path.split('/').collect();
            let mut at = volume_root;
            for (i, name) in parts.iter().enumerate() {
                let meta = if i + 1 == parts.len() { file } else { dir_meta };
                at = index.child_named(at, name).unwrap_or_else(|| index.insert(at, name, meta));
            }
        }
        let disk = Files(&[
            ("/games/steamapps/appmanifest_70.acf", MANIFEST),
            (
                "/var/lib/pacman/local/vim-9.1-1/files",
                "%FILES%\nusr/\nusr/share/\nusr/share/vim/\nusr/share/vim/vimrc\n",
            ),
        ]);
        let detected = owners::detect(&index, v, Some(owners::Platform::Linux), &disk);
        index.set_owners(volume_root, detected);

        let sized: Vec<(&str, Option<u64>)> = size(&index, &programs).iter().map(|a| (a.program.source, a.size)).collect();
        assert_eq!(
            sized,
            [
                ("pacman", Some(100)),
                ("Flatpak", Some(100)),
                ("Steam", Some(100)),
                ("/opt", Some(100))
            ]
        );
    }
}
