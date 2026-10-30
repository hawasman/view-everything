//! Owner detection (ADR-0014): finds the Owner roots of one Location from the Index itself
//! (project markers, Steam libraries, install folders) and, for a mounted local Volume, from
//! what the platform keeps on disk (Steam manifests, the package database, `os-release`).
//!
//! Runs as the last step of a Sync or Catch-up in three steps, so no file is read while the
//! Index is locked: [`plan`] under a read lock, [`Detection::read`] with no lock, and
//! [`Detection::finish`] whose result goes to [`Index::set_owners`] under the write lock.

use crate::index::{EntryId, Index, VolumeId};
use crate::owner::{Owner, OwnerKind};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Files whose presence makes their folder a Project root, with the Owner's detail.
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "CMakeLists.txt",
    "build.gradle",
    "build.gradle.kts",
    "Package.swift",
];
/// Extensions (lower case) that mark a Project root as [`PROJECT_MARKERS`] do.
const PROJECT_MARKER_EXTENSIONS: &[&str] = &["sln", "csproj"];
/// Folders holding other people's code: markers inside them do not start a Project, and
/// nothing beneath them is looked at.
const DEPENDENCY_FOLDERS: &[&str] = &[
    "node_modules",
    ".git",
    "vendor",
    "site-packages",
    "dist-packages",
    ".venv",
    "venv",
    ".cargo",
    ".rustup",
    ".npm",
    ".gradle",
    ".m2",
    "bower_components",
    "Pods",
];

/// The platform conventions to apply to a mounted local Volume. `None` (a Remote or an
/// Offline Volume) applies only what the Index alone can tell: project markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Linux,
    MacOs,
}

impl Platform {
    /// The platform this build runs on, if Owner detection knows its conventions.
    pub fn current() -> Option<Platform> {
        if cfg!(target_os = "windows") {
            Some(Platform::Windows)
        } else if cfg!(target_os = "linux") {
            Some(Platform::Linux)
        } else if cfg!(target_os = "macos") {
            Some(Platform::MacOs)
        } else {
            None
        }
    }
}

/// What detection reads from disk, so tests can supply it.
pub trait Disk {
    /// The text of a file, if it can be read.
    fn read(&self, path: &Path) -> Option<String>;
    /// The names in a folder, if it can be listed.
    fn list(&self, path: &Path) -> Vec<String>;
}

/// The real filesystem.
pub struct LocalDisk;

impl Disk for LocalDisk {
    fn read(&self, path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn list(&self, path: &Path) -> Vec<String> {
        fs::read_dir(path)
            .map(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
            .unwrap_or_default()
    }
}

/// The Owner roots of the Location `volume`, for [`Index::set_owners`]: the three steps in one
/// go, for a caller that holds no Index lock. A folder found by a platform convention and by a
/// project marker keeps the platform's Owner.
pub fn detect(index: &Index, volume: VolumeId, platform: Option<Platform>, disk: &dyn Disk) -> Vec<(EntryId, Owner)> {
    plan(index, volume, platform).map_or_else(Vec::new, |detection| {
        let text = detection.read(disk);
        detection.finish(index, text).1
    })
}

/// The first step: what the Index alone shows for the Location (project markers, `steamapps`
/// folders and their manifests). `None` when the Location is gone.
pub fn plan(index: &Index, volume: VolumeId, platform: Option<Platform>) -> Option<Detection> {
    let root = index.volume(volume)?.root;
    let walk = Walk::new(index, root);
    let manifests = if platform.is_some() {
        walk.steamapps
            .iter()
            .filter_map(|&apps| Some((index.child_named(apps, "common")?, apps)))
            .flat_map(|(common, apps)| {
                index.children(apps).filter_map(move |manifest| {
                    let name = index.name(manifest);
                    (!index.is_dir(manifest) && name.starts_with("appmanifest_") && name.ends_with(".acf"))
                        .then(|| (common, index.path(manifest)))
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Some(Detection {
        volume,
        root,
        platform,
        projects: walk.projects,
        manifests,
    })
}

/// A detection between its steps: what [`plan`] found and which files [`Detection::read`]
/// needs. Ids stay valid while the Index is not replaced; Entries removed in between are skipped.
pub struct Detection {
    volume: VolumeId,
    root: EntryId,
    platform: Option<Platform>,
    projects: Vec<(EntryId, Owner)>,
    /// A Steam library's `common` folder and the path of one of its manifests.
    manifests: Vec<(EntryId, PathBuf)>,
}

/// What [`Detection::read`] took from disk.
pub struct DiskText {
    /// A Steam library's `common` folder and the text of one of its manifests.
    manifests: Vec<(EntryId, String)>,
    os_release: Option<String>,
    packages: Vec<Package>,
}

impl Detection {
    /// The second step: reads the manifests and, on Linux, `os-release` and the pacman
    /// database. Needs no Index lock.
    pub fn read(&self, disk: &dyn Disk) -> DiskText {
        let manifests = self
            .manifests
            .iter()
            .filter_map(|(common, path)| Some((*common, disk.read(path)?)))
            .collect();
        let (os_release, packages) = if self.platform == Some(Platform::Linux) {
            (disk.read(Path::new("/etc/os-release")), read_pacman(disk))
        } else {
            (None, Vec::new())
        };
        DiskText {
            manifests,
            os_release,
            packages,
        }
    }

    /// The last step: the Location's root and its Owner roots, for [`Index::set_owners`]. Walks
    /// the Index but reads no file, so it can run under the write lock.
    pub fn finish(self, index: &Index, text: DiskText) -> (EntryId, Vec<(EntryId, Owner)>) {
        let mut found: HashMap<EntryId, Owner> = HashMap::new();
        if let Some(platform) = self.platform {
            for (id, owner) in steam(index, &text.manifests) {
                found.entry(id).or_insert(owner);
            }
            let platform_roots = match platform {
                Platform::Windows => windows(index, self.root),
                Platform::Linux => linux(index, self.volume, text.os_release.as_deref(), &text.packages),
                Platform::MacOs => macos(index, self.root),
            };
            for (id, owner) in platform_roots {
                found.entry(id).or_insert(owner);
            }
        }
        for (id, owner) in self.projects {
            found.entry(id).or_insert(owner);
        }
        let mut roots: Vec<(EntryId, Owner)> = found.into_iter().filter(|(id, _)| index.is_live(*id)).collect();
        roots.sort_by_key(|(id, _)| *id);
        (self.root, roots)
    }
}

/// One pass over a Location's folders: Project roots and `steamapps` folders.
struct Walk {
    projects: Vec<(EntryId, Owner)>,
    steamapps: Vec<EntryId>,
}

impl Walk {
    fn new(index: &Index, root: EntryId) -> Walk {
        let mut walk = Walk {
            projects: Vec::new(),
            steamapps: Vec::new(),
        };
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            if index.name(dir) == "steamapps" {
                walk.steamapps.push(dir);
            }
            let mut marker: Option<&str> = None;
            for child in index.children(dir) {
                let name = index.name(child);
                if index.is_dir(child) {
                    if !DEPENDENCY_FOLDERS.contains(&name) {
                        stack.push(child);
                    }
                    if name == ".git" {
                        marker = Some(".git");
                    }
                    continue;
                }
                if marker.is_none() && is_project_marker(name) {
                    marker = Some(name);
                }
            }
            if let Some(marker) = marker
                && index.parent(dir).is_some()
            {
                walk.projects.push((dir, Owner::new(OwnerKind::Project, index.name(dir), marker)));
            }
        }
        walk
    }
}

fn is_project_marker(name: &str) -> bool {
    PROJECT_MARKERS.contains(&name)
        || crate::category::extension(name).is_some_and(|ext| PROJECT_MARKER_EXTENSIONS.iter().any(|m| m.eq_ignore_ascii_case(ext)))
}

/// Steam games: each `appmanifest_<id>.acf` in a `steamapps` folder names the game and its
/// folder under `steamapps/common`.
fn steam(index: &Index, manifests: &[(EntryId, String)]) -> Vec<(EntryId, Owner)> {
    manifests
        .iter()
        .filter_map(|(common, text)| {
            let (title, installdir) = (vdf_value(text, "name")?, vdf_value(text, "installdir")?);
            let dir = index.child_named(*common, installdir)?;
            Some((dir, Owner::new(OwnerKind::Game, title, "Steam")))
        })
        .collect()
}

/// The value of `key` in Valve's key-value text: a line `"key"  "value"`.
fn vdf_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let mut quoted = line.split('"').skip(1).step_by(2);
        (quoted.next()? == key).then(|| quoted.next()).flatten()
    })
}

/// `root/components...`, by name.
fn descend<'a>(index: &Index, root: EntryId, components: impl IntoIterator<Item = &'a str>) -> Option<EntryId> {
    components.into_iter().try_fold(root, |dir, name| index.child_named(dir, name))
}

/// The folders in `dir`.
fn folders(index: &Index, dir: Option<EntryId>) -> impl Iterator<Item = EntryId> + '_ {
    dir.into_iter()
        .flat_map(move |d| index.children(d))
        .filter(move |&c| index.is_dir(c))
}

fn has_file(index: &Index, dir: EntryId) -> bool {
    index.children(dir).any(|c| !index.is_dir(c))
}

/// Vendors whose folder under `Program Files` holds one application per subfolder.
const VENDOR_FOLDERS: &[&str] = &[
    "Microsoft",
    "Adobe",
    "Google",
    "Mozilla",
    "JetBrains",
    "NVIDIA Corporation",
    "Intel",
    "AMD",
    "Realtek",
    "Epic Games",
    "Autodesk",
    "Oracle",
    "Unity",
    "Ubisoft",
    "Electronic Arts",
    "Rockstar Games",
    "Blizzard Entertainment",
];

/// Whether a folder under `Program Files` groups applications rather than being one: a known
/// vendor, or no files of its own and several subfolders.
fn is_vendor_folder(index: &Index, dir: EntryId) -> bool {
    VENDOR_FOLDERS.contains(&index.name(dir)) || (!has_file(index, dir) && folders(index, Some(dir)).nth(1).is_some())
}

/// Windows: the system folder, `Program Files[ (x86)]\<App>` or `\<Vendor>\<App>`, and each
/// user's `AppData\Local\Programs\<App>`.
fn windows(index: &Index, root: EntryId) -> Vec<(EntryId, Owner)> {
    let mut roots = Vec::new();
    if let Some(windows) = index.child_named(root, "Windows") {
        roots.push((windows, Owner::new(OwnerKind::System, "Windows", "system folder")));
    }
    for program_files in ["Program Files", "Program Files (x86)"] {
        for entry in folders(index, index.child_named(root, program_files)) {
            let name = index.name(entry);
            if matches!(name, "Common Files" | "WindowsApps") {
                continue;
            }
            if is_vendor_folder(index, entry) {
                // Each application beneath a vendor folder is its own Program.
                for app in folders(index, Some(entry)) {
                    roots.push((app, Owner::new(OwnerKind::Program, index.name(app), name)));
                }
            } else {
                roots.push((entry, Owner::new(OwnerKind::Program, name, program_files)));
            }
        }
    }
    for user in folders(index, index.child_named(root, "Users")) {
        for app in folders(index, descend(index, user, ["AppData", "Local", "Programs"])) {
            roots.push((app, Owner::new(OwnerKind::Program, index.name(app), "AppData")));
        }
    }
    roots
}

/// macOS: the system folders and the `.app` bundles in `/Applications` and each user's.
fn macos(index: &Index, root: EntryId) -> Vec<(EntryId, Owner)> {
    let mut roots = Vec::new();
    for folder in ["System", "usr"] {
        if let Some(dir) = index.child_named(root, folder) {
            roots.push((dir, Owner::new(OwnerKind::System, "macOS", "system folder")));
        }
    }
    let applications = std::iter::once(index.child_named(root, "Applications"))
        .chain(folders(index, index.child_named(root, "Users")).map(|u| index.child_named(u, "Applications")));
    for app in applications.flat_map(|dir| folders(index, dir)) {
        if let Some(name) = index.name(app).strip_suffix(".app") {
            roots.push((app, Owner::new(OwnerKind::Program, name, "Application")));
        }
    }
    roots
}

/// Linux: the system folders (named after the distribution), `/opt/<app>`, Flatpak
/// applications and the folders a pacman package owns wholly.
fn linux(index: &Index, volume: VolumeId, os_release: Option<&str>, packages: &[Package]) -> Vec<(EntryId, Owner)> {
    let mut roots = Vec::new();
    let on_volume = |id: EntryId| index.volume_of(id) == Some(volume);
    let system = os_release
        .and_then(|text| {
            let field = |key: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .map(|v| v.trim_matches('"').to_owned())
            };
            field("PRETTY_NAME=").or_else(|| field("NAME="))
        })
        .unwrap_or_else(|| "Linux".to_owned());
    for folder in ["/usr", "/etc", "/boot", "/var/lib", "/var/cache"] {
        if let Some(dir) = index.find_path(Path::new(folder)).filter(|&d| on_volume(d)) {
            roots.push((dir, Owner::new(OwnerKind::System, system.as_str(), "system folder")));
        }
    }
    for app in folders(index, index.find_path(Path::new("/opt")).filter(|&d| on_volume(d))) {
        roots.push((app, Owner::new(OwnerKind::Program, index.name(app), "/opt")));
    }
    let homes = folders(index, index.find_path(Path::new("/home"))).map(|h| index.path(h));
    let flatpak_apps: Vec<PathBuf> = [PathBuf::from("/var/lib/flatpak/app")]
        .into_iter()
        .chain(homes.flat_map(|home| [home.join(".local/share/flatpak/app"), home.join(".var/app")]))
        .collect();
    for dir in flatpak_apps {
        for app in folders(index, index.find_path(&dir).filter(|&d| on_volume(d))) {
            let id = index.name(app);
            let name = id.rsplit('.').next().unwrap_or(id);
            roots.push((app, Owner::new(OwnerKind::Program, name, format!("Flatpak {id}"))));
        }
    }
    roots.extend(pacman(index, &on_volume, packages));
    roots
}

/// One installed pacman package: its name, the folders it lists and the files it owns, all
/// relative to the filesystem root.
pub struct Package {
    name: String,
    dirs: Vec<String>,
    files: HashSet<String>,
}

/// The local pacman database, if there is one.
fn read_pacman(disk: &dyn Disk) -> Vec<Package> {
    let db = Path::new("/var/lib/pacman/local");
    let mut packages = Vec::new();
    for entry in disk.list(db) {
        let dir = db.join(&entry);
        let Some(files) = disk.read(&dir.join("files")) else {
            continue;
        };
        let name = disk
            .read(&dir.join("desc"))
            .and_then(|desc| desc.lines().skip_while(|l| *l != "%NAME%").nth(1).map(str::to_owned))
            .unwrap_or_else(|| entry.rsplitn(3, '-').last().unwrap_or(&entry).to_owned());
        let mut package = Package {
            name,
            dirs: Vec::new(),
            files: HashSet::new(),
        };
        for line in files.lines().skip_while(|l| *l != "%FILES%").skip(1).take_while(|l| !l.is_empty()) {
            match line.strip_suffix('/') {
                Some(dir) => package.dirs.push(dir.to_owned()),
                None => {
                    package.files.insert(line.to_owned());
                }
            }
        }
        packages.push(package);
    }
    packages
}

/// Folders that one pacman package owns wholly: listed by that package alone, with every
/// indexed file beneath them in its file list. The outermost such folder is the Owner root.
fn pacman(index: &Index, on_volume: &dyn Fn(EntryId) -> bool, packages: &[Package]) -> Vec<(EntryId, Owner)> {
    let mut listed_by: HashMap<&str, u32> = HashMap::new();
    for dir in packages.iter().flat_map(|p| &p.dirs) {
        *listed_by.entry(dir).or_default() += 1;
    }
    let mut roots = Vec::new();
    for package in packages {
        let mut dirs: Vec<&str> = package.dirs.iter().map(String::as_str).collect();
        dirs.sort_unstable();
        let mut taken: Vec<&str> = Vec::new();
        for dir in dirs {
            if listed_by[dir] != 1 || taken.iter().any(|t| dir.starts_with(t) && dir[t.len()..].starts_with('/')) {
                continue;
            }
            let Some(id) = index.find_path(&Path::new("/").join(dir)).filter(|&d| on_volume(d)) else {
                continue;
            };
            if files_within(index, id, dir).all(|file| package.files.contains(&file)) {
                roots.push((id, Owner::new(OwnerKind::System, package.name.as_str(), "pacman")));
                taken.push(dir);
            }
        }
    }
    roots
}

/// The `/`-joined paths, relative to the filesystem root, of every indexed file under `dir`
/// (whose own such path is `prefix`).
fn files_within<'a>(index: &'a Index, dir: EntryId, prefix: &str) -> impl Iterator<Item = String> + 'a {
    let mut stack = vec![(dir, prefix.to_owned())];
    let mut pending: Vec<String> = Vec::new();
    std::iter::from_fn(move || {
        loop {
            if let Some(file) = pending.pop() {
                return Some(file);
            }
            let (d, path) = stack.pop()?;
            for child in index.children(d) {
                let child_path = format!("{path}/{}", index.name(child));
                if index.is_dir(child) {
                    stack.push((child, child_path));
                } else {
                    pending.push(child_path);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{EntryMeta, SyncState};

    struct FakeDisk(HashMap<PathBuf, String>);

    impl Disk for FakeDisk {
        fn read(&self, path: &Path) -> Option<String> {
            self.0.get(path).cloned()
        }

        fn list(&self, path: &Path) -> Vec<String> {
            let mut names: Vec<String> = self
                .0
                .keys()
                .filter_map(|p| {
                    p.strip_prefix(path)
                        .ok()?
                        .components()
                        .next()?
                        .as_os_str()
                        .to_str()
                        .map(str::to_owned)
                })
                .collect();
            names.sort();
            names.dedup();
            names
        }
    }

    fn nothing() -> FakeDisk {
        FakeDisk(HashMap::new())
    }

    /// An Index of one Volume at `mount` holding `paths` (a trailing `/` makes a folder; parents
    /// are created as needed).
    fn index_of(mount: &str, paths: &[&str]) -> (Index, VolumeId) {
        let mut index = Index::new();
        let dir = EntryMeta {
            is_dir: true,
            ..EntryMeta::default()
        };
        let v = index.add_volume(Path::new(mount), "ext4", "id", dir, SyncState::Walk);
        let root = index.volume(v).unwrap().root;
        for path in paths {
            let is_dir = path.ends_with('/');
            let mut current = root;
            let components: Vec<&str> = path.split(['/', '\\']).filter(|c| !c.is_empty()).collect();
            for (i, name) in components.iter().enumerate() {
                let last = i + 1 == components.len();
                current = match index.child_named(current, name) {
                    Some(c) => c,
                    None => index.insert(
                        current,
                        name,
                        if last && !is_dir {
                            EntryMeta {
                                logical: 10,
                                on_disk: 4096,
                                ..EntryMeta::default()
                            }
                        } else {
                            dir
                        },
                    ),
                };
            }
        }
        (index, v)
    }

    fn named(index: &Index, roots: &[(EntryId, Owner)]) -> Vec<(String, OwnerKind, String, String)> {
        let mut out: Vec<_> = roots
            .iter()
            .map(|(id, o)| {
                (
                    index.path(*id).to_string_lossy().replace('\\', "/").replace("//", "/"),
                    o.kind,
                    o.name.clone(),
                    o.detail.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn project_markers_make_roots_except_inside_dependency_folders() {
        let (index, v) = index_of(
            "/",
            &[
                "home/u/app/Cargo.toml",
                "home/u/app/crates/core/Cargo.toml",
                "home/u/app/node_modules/left-pad/package.json",
                "home/u/app/.git/HEAD",
                "home/u/site/Acme.sln",
                "home/u/worktree/.git",
                "home/u/notes/todo.txt",
            ],
        );
        let roots = detect(&index, v, None, &nothing());
        assert_eq!(
            named(&index, &roots),
            [
                ("/home/u/app".to_owned(), OwnerKind::Project, "app".to_owned(), ".git".to_owned()),
                (
                    "/home/u/app/crates/core".into(),
                    OwnerKind::Project,
                    "core".into(),
                    "Cargo.toml".into()
                ),
                ("/home/u/site".into(), OwnerKind::Project, "site".into(), "Acme.sln".into()),
                ("/home/u/worktree".into(), OwnerKind::Project, "worktree".into(), ".git".into()),
            ]
        );
    }

    #[test]
    fn steam_manifests_name_the_games_and_win_over_project_markers() {
        let (index, v) = index_of(
            "/",
            &[
                "games/steamapps/appmanifest_427520.acf",
                "games/steamapps/appmanifest_999.acf",
                "games/steamapps/common/Factorio/bin/x64/factorio",
                "games/steamapps/common/Factorio/.git/HEAD",
                "games/steamapps/common/Orphan/data",
            ],
        );
        let disk = FakeDisk(HashMap::from([
            (
                PathBuf::from("/games/steamapps/appmanifest_427520.acf"),
                "\"AppState\"\n{\n\t\"appid\"\t\t\"427520\"\n\t\"name\"\t\t\"Factorio\"\n\t\"installdir\"\t\t\"Factorio\"\n}\n".to_owned(),
            ),
            (
                PathBuf::from("/games/steamapps/appmanifest_999.acf"),
                "\"AppState\"\n{\n\t\"name\"\t\t\"Not installed\"\n\t\"installdir\"\t\t\"Missing\"\n}\n".to_owned(),
            ),
        ]));
        let roots = detect(&index, v, Some(Platform::Linux), &disk);
        assert_eq!(
            named(&index, &roots),
            [(
                "/games/steamapps/common/Factorio".to_owned(),
                OwnerKind::Game,
                "Factorio".to_owned(),
                "Steam".to_owned()
            )]
        );
        // Without the platform (an Offline Volume) the manifest is not read; the marker stands.
        let roots = detect(&index, v, None, &disk);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].1.kind, OwnerKind::Project);
    }

    #[test]
    fn windows_install_folders_are_programs_with_or_without_a_vendor_folder() {
        let (index, v) = index_of(
            "C:\\",
            &[
                "Windows/System32/ntdll.dll",
                "Program Files/Git/cmd/git.exe",
                "Program Files/Adobe/Photoshop/photoshop.exe",
                "Program Files/Adobe/Bridge/bridge.exe",
                "Program Files/Common Files/x.dll",
                "Program Files (x86)/Steam/steam.exe",
                "Users/mo/AppData/Local/Programs/Zed/zed.exe",
            ],
        );
        let roots = detect(&index, v, Some(Platform::Windows), &nothing());
        assert_eq!(
            named(&index, &roots),
            [
                (
                    "C:/Program Files (x86)/Steam".to_owned(),
                    OwnerKind::Program,
                    "Steam".to_owned(),
                    "Program Files (x86)".to_owned()
                ),
                (
                    "C:/Program Files/Adobe/Bridge".into(),
                    OwnerKind::Program,
                    "Bridge".into(),
                    "Adobe".into()
                ),
                (
                    "C:/Program Files/Adobe/Photoshop".into(),
                    OwnerKind::Program,
                    "Photoshop".into(),
                    "Adobe".into()
                ),
                (
                    "C:/Program Files/Git".into(),
                    OwnerKind::Program,
                    "Git".into(),
                    "Program Files".into()
                ),
                (
                    "C:/Users/mo/AppData/Local/Programs/Zed".into(),
                    OwnerKind::Program,
                    "Zed".into(),
                    "AppData".into()
                ),
                ("C:/Windows".into(), OwnerKind::System, "Windows".into(), "system folder".into()),
            ]
        );
    }

    #[test]
    fn macos_app_bundles_are_programs() {
        let (index, v) = index_of(
            "/",
            &[
                "Applications/Xcode.app/Contents/Info.plist",
                "Users/mo/Applications/Arc.app/Contents/Info.plist",
                "Applications/notes.txt",
                "System/Library/x",
            ],
        );
        let roots = detect(&index, v, Some(Platform::MacOs), &nothing());
        assert_eq!(
            named(&index, &roots),
            [
                (
                    "/Applications/Xcode.app".to_owned(),
                    OwnerKind::Program,
                    "Xcode".to_owned(),
                    "Application".to_owned()
                ),
                ("/System".into(), OwnerKind::System, "macOS".into(), "system folder".into()),
                (
                    "/Users/mo/Applications/Arc.app".into(),
                    OwnerKind::Program,
                    "Arc".into(),
                    "Application".into()
                ),
            ]
        );
    }

    #[test]
    fn linux_system_folders_flatpaks_and_wholly_owned_pacman_folders() {
        let (index, v) = index_of(
            "/",
            &[
                "usr/bin/ls",
                "usr/bin/vim",
                "usr/share/vim/vimrc",
                "usr/share/vim/extra/added-by-user",
                "usr/share/doc/vim/README",
                "usr/share/doc/coreutils/README",
                "etc/fstab",
                "opt/brave/brave",
                "var/lib/flatpak/app/org.gimp.GIMP/current/active/files/bin/gimp",
                "home/u/.var/app/com.spotify.Client/cache/x",
                "home/u/.local/share/flatpak/app/org.zealdocs.Zeal/x",
            ],
        );
        let disk = FakeDisk(HashMap::from([
            (PathBuf::from("/etc/os-release"), "NAME=\"Arch Linux\"\nPRETTY_NAME=\"CachyOS Linux\"\n".to_owned()),
            (PathBuf::from("/var/lib/pacman/local/vim-9.1-1/desc"), "%NAME%\nvim\n\n%VERSION%\n9.1-1\n".to_owned()),
            (
                PathBuf::from("/var/lib/pacman/local/vim-9.1-1/files"),
                "%FILES%\nusr/\nusr/bin/\nusr/bin/vim\nusr/share/\nusr/share/vim/\nusr/share/vim/vimrc\nusr/share/doc/\nusr/share/doc/vim/\nusr/share/doc/vim/README\n\n%BACKUP%\n".to_owned(),
            ),
            (PathBuf::from("/var/lib/pacman/local/coreutils-9.5-1/desc"), "%NAME%\ncoreutils\n".to_owned()),
            (
                PathBuf::from("/var/lib/pacman/local/coreutils-9.5-1/files"),
                "%FILES%\nusr/\nusr/bin/\nusr/bin/ls\nusr/share/\nusr/share/doc/\nusr/share/doc/coreutils/\nusr/share/doc/coreutils/README\n".to_owned(),
            ),
        ]));
        let roots = detect(&index, v, Some(Platform::Linux), &disk);
        let system = |path: &str| {
            (
                path.to_owned(),
                OwnerKind::System,
                "CachyOS Linux".to_owned(),
                "system folder".to_owned(),
            )
        };
        assert_eq!(
            named(&index, &roots),
            [
                system("/etc"),
                (
                    "/home/u/.local/share/flatpak/app/org.zealdocs.Zeal".into(),
                    OwnerKind::Program,
                    "Zeal".into(),
                    "Flatpak org.zealdocs.Zeal".into()
                ),
                (
                    "/home/u/.var/app/com.spotify.Client".into(),
                    OwnerKind::Program,
                    "Client".into(),
                    "Flatpak com.spotify.Client".into()
                ),
                ("/opt/brave".into(), OwnerKind::Program, "brave".into(), "/opt".into()),
                system("/usr"),
                // `usr/share/vim` holds a file vim does not list, so only its doc folder is vim's.
                (
                    "/usr/share/doc/coreutils".into(),
                    OwnerKind::System,
                    "coreutils".into(),
                    "pacman".into()
                ),
                ("/usr/share/doc/vim".into(), OwnerKind::System, "vim".into(), "pacman".into()),
                system("/var/lib"),
                (
                    "/var/lib/flatpak/app/org.gimp.GIMP".into(),
                    OwnerKind::Program,
                    "GIMP".into(),
                    "Flatpak org.gimp.GIMP".into()
                ),
            ]
        );
    }
}
