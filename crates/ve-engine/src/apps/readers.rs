//! Readers of what a Linux system says is installed: the pacman, dpkg and rpm databases,
//! Flatpak installations, Steam libraries and the folders under `/opt`. Each reads below a
//! [`Sources`] root, so tests point them at fixture folders. They read the same places Owner
//! detection does ([`ve_core::owners`]) and name each program's Owner the way it does.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;
use ve_core::Owner;
use ve_core::owners::{self, vdf_value};

/// Where installed programs are read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sources {
    /// The filesystem root the system databases and `/opt` are found under: `/`.
    pub root: PathBuf,
    /// The user's home folder, for per-user Flatpaks and the Steam libraries.
    pub home: Option<PathBuf>,
}

impl Sources {
    /// The running system's, on Linux; `None` elsewhere (Apps reads Linux only for now).
    pub fn local() -> Option<Sources> {
        cfg!(target_os = "linux").then(|| Sources {
            root: PathBuf::from("/"),
            home: std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute()),
        })
    }
}

/// Which sort of installed program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AppKind {
    /// Installed by the system's package manager.
    Package,
    Flatpak,
    /// Installed by a game launcher.
    Game,
    /// A folder under `/opt`.
    Program,
}

impl AppKind {
    pub fn name(self) -> &'static str {
        match self {
            AppKind::Package => "Package",
            AppKind::Flatpak => "Flatpak",
            AppKind::Game => "Game",
            AppKind::Program => "Program",
        }
    }
}

/// One installed program as its source describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    /// What to show: the package's name, the application's or game's title.
    pub name: String,
    pub kind: AppKind,
    /// Where it was read from: `pacman`, `dpkg`, `rpm`, `Flatpak`, `Steam` or `/opt`.
    pub source: &'static str,
    /// What the source calls it: package name, Flatpak application id, Steam app id, folder name.
    pub id: String,
    pub version: Option<String>,
    /// When the installed version was installed, in seconds since the Unix epoch.
    pub installed: Option<i64>,
    /// The Owner its Owner roots carry in the Index.
    pub owner: Owner,
}

/// Every installed program the sources list.
pub fn read(sources: &Sources) -> Vec<Program> {
    let mut programs = pacman(&sources.root);
    programs.extend(dpkg(&sources.root));
    programs.extend(rpm(&sources.root));
    programs.extend(flatpak(sources));
    if let Some(home) = &sources.home {
        programs.extend(steam(home));
    }
    programs.extend(opt(&sources.root));
    programs
}

/// `path` below `root`, for an absolute `path`.
fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// The names in a folder; nothing when it cannot be listed.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// A modification time in seconds since the Unix epoch; the link's own for a symlink.
fn modified(path: &Path) -> Option<i64> {
    let time = fs::symlink_metadata(path).ok()?.modified().ok()?;
    Some(time.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// The value under `%KEY%` in a pacman `desc` file.
fn desc_field<'a>(desc: &'a str, key: &str) -> Option<&'a str> {
    desc.lines().skip_while(|l| *l != key).nth(1).filter(|v| !v.is_empty())
}

/// The pacman database: `var/lib/pacman/local/<name>-<version>/desc`.
pub fn pacman(root: &Path) -> Vec<Program> {
    let db = under(root, "/var/lib/pacman/local");
    names(&db)
        .into_iter()
        .filter_map(|entry| {
            let desc = fs::read_to_string(db.join(&entry).join("desc")).ok()?;
            let name = desc_field(&desc, "%NAME%")?.to_owned();
            Some(Program {
                kind: AppKind::Package,
                source: "pacman",
                version: desc_field(&desc, "%VERSION%").map(str::to_owned),
                installed: desc_field(&desc, "%INSTALLDATE%").and_then(|d| d.parse().ok()),
                owner: owners::package_owner(&name, "pacman"),
                id: name.clone(),
                name,
            })
        })
        .collect()
}

/// The dpkg database: the installed paragraphs of `var/lib/dpkg/status`, each dated by its
/// file list, which dpkg rewrites when it installs that version.
pub fn dpkg(root: &Path) -> Vec<Program> {
    let Ok(status) = fs::read_to_string(under(root, "/var/lib/dpkg/status")) else {
        return Vec::new();
    };
    let info = under(root, "/var/lib/dpkg/info");
    status
        .split("\n\n")
        .filter_map(|paragraph| {
            let field = |key: &str| {
                paragraph
                    .lines()
                    .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
            };
            let name = field("Package")?;
            if !field("Status")?.ends_with(" installed") {
                return None;
            }
            let arch = field("Architecture").unwrap_or_default();
            let installed = modified(&info.join(format!("{name}.list"))).or_else(|| modified(&info.join(format!("{name}:{arch}.list"))));
            Some(Program {
                name: name.to_owned(),
                kind: AppKind::Package,
                source: "dpkg",
                id: name.to_owned(),
                version: field("Version").map(str::to_owned),
                installed,
                owner: owners::package_owner(name, "dpkg"),
            })
        })
        .collect()
}

/// The rpm database, through `rpm` itself (its database is a binary format), when the root
/// has one.
pub fn rpm(root: &Path) -> Vec<Program> {
    let has_db = ["/var/lib/rpm", "/usr/lib/sysimage/rpm"].iter().any(|db| under(root, db).is_dir());
    if !has_db {
        return Vec::new();
    }
    Command::new("rpm")
        .arg("--root")
        .arg(root)
        .args(["-qa", "--queryformat", "%{NAME}\\t%{VERSION}-%{RELEASE}\\t%{INSTALLTIME}\\n"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| rpm_query(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default()
}

/// The lines `rpm -qa` printed in the format [`rpm`] asks for: name, version, install time.
pub fn rpm_query(text: &str) -> Vec<Program> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next().filter(|n| !n.is_empty())?;
            // `gpg-pubkey` entries are signing keys rpm keeps as packages.
            (name != "gpg-pubkey").then(|| Program {
                name: name.to_owned(),
                kind: AppKind::Package,
                source: "rpm",
                id: name.to_owned(),
                version: fields.next().filter(|v| !v.is_empty()).map(str::to_owned),
                installed: fields.next().and_then(|t| t.trim().parse().ok()),
                owner: owners::package_owner(name, "rpm"),
            })
        })
        .collect()
}

/// Flatpak applications installed system-wide and for the user: each `app/<id>` with a
/// `current` deployment. The name is the one its desktop file gives, the version its metainfo
/// file's latest release, the date that of its `active` deployment.
pub fn flatpak(sources: &Sources) -> Vec<Program> {
    let installations = std::iter::once(under(&sources.root, "/var/lib/flatpak/app"))
        .chain(sources.home.iter().map(|h| h.join(".local/share/flatpak/app")));
    let mut programs = Vec::new();
    for apps in installations {
        for id in names(&apps) {
            let Ok(current) = fs::read_link(apps.join(&id).join("current")) else {
                continue;
            };
            let deployment = apps.join(&id).join(current);
            let active = deployment.join("active");
            let share = active.join("files/share");
            let name = fs::read_to_string(active.join(format!("export/share/applications/{id}.desktop")))
                .ok()
                .and_then(|desktop| desktop_name(&desktop));
            let version = [
                share.join(format!("metainfo/{id}.metainfo.xml")),
                share.join(format!("metainfo/{id}.appdata.xml")),
                share.join(format!("appdata/{id}.appdata.xml")),
            ]
            .iter()
            .find_map(|path| release_version(&fs::read_to_string(path).ok()?));
            let owner = owners::flatpak_owner(&id);
            programs.push(Program {
                name: name.unwrap_or_else(|| owner.name.clone()),
                kind: AppKind::Flatpak,
                source: "Flatpak",
                version,
                installed: modified(&active),
                owner,
                id,
            });
        }
    }
    programs
}

/// `Name=` of a desktop file's `[Desktop Entry]` group.
fn desktop_name(text: &str) -> Option<String> {
    text.lines()
        .skip_while(|l| l.trim() != "[Desktop Entry]")
        .skip(1)
        .take_while(|l| !l.starts_with('['))
        .find_map(|l| l.strip_prefix("Name="))
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
}

/// The `version` of the first `<release>` in AppStream metainfo, which lists the newest first.
fn release_version(xml: &str) -> Option<String> {
    let tag = &xml[xml.find("<release ")?..];
    let tag = &tag[..tag.find('>')?];
    let value = &tag[tag.find("version=")? + "version=".len()..];
    let quote = value.chars().next().filter(|q| *q == '"' || *q == '\'')?;
    let value = &value[1..];
    Some(value[..value.find(quote)?].to_owned()).filter(|v| !v.is_empty())
}

/// Steam games: the manifests of every library the user's Steam knows (its own folder and
/// those `libraryfolders.vdf` lists) whose install folder is there. A game in two libraries
/// is listed once.
pub fn steam(home: &Path) -> Vec<Program> {
    let steam_roots = [
        ".local/share/Steam",
        ".steam/steam",
        ".var/app/com.valvesoftware.Steam/.local/share/Steam",
    ];
    let mut libraries: Vec<PathBuf> = Vec::new();
    for root in steam_roots.iter().map(|r| home.join(r)) {
        let listed = fs::read_to_string(root.join("steamapps/libraryfolders.vdf")).unwrap_or_default();
        let paths = listed.lines().filter_map(|line| vdf_value(line, "path")).map(PathBuf::from);
        for library in std::iter::once(root.clone()).chain(paths) {
            if let Ok(real) = library.canonicalize()
                && !libraries.contains(&real)
            {
                libraries.push(real);
            }
        }
    }
    let mut seen = HashSet::new();
    let mut programs = Vec::new();
    for steamapps in libraries.iter().map(|l| l.join("steamapps")) {
        for file in names(&steamapps) {
            if !(file.starts_with("appmanifest_") && file.ends_with(".acf")) {
                continue;
            }
            let Ok(text) = fs::read_to_string(steamapps.join(&file)) else {
                continue;
            };
            let (Some(appid), Some(title), Some(installdir)) =
                (vdf_value(&text, "appid"), vdf_value(&text, "name"), vdf_value(&text, "installdir"))
            else {
                continue;
            };
            if !steamapps.join("common").join(installdir).is_dir() || !seen.insert(appid.to_owned()) {
                continue;
            }
            programs.push(Program {
                name: title.to_owned(),
                kind: AppKind::Game,
                source: "Steam",
                id: appid.to_owned(),
                version: vdf_value(&text, "buildid").map(|b| format!("build {b}")),
                installed: vdf_value(&text, "LastUpdated").and_then(|t| t.parse().ok()).filter(|&t| t > 0),
                owner: owners::steam_owner(title),
            });
        }
    }
    programs
}

/// The folders in `/opt`, each one program, dated by when the folder was created where the
/// filesystem records it.
pub fn opt(root: &Path) -> Vec<Program> {
    let opt = under(root, "/opt");
    names(&opt)
        .into_iter()
        .filter_map(|name| {
            let meta = fs::symlink_metadata(opt.join(&name)).ok()?;
            meta.is_dir().then(|| Program {
                kind: AppKind::Program,
                source: "/opt",
                version: None,
                installed: meta
                    .created()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
                owner: owners::opt_owner(&name),
                id: name.clone(),
                name,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ve_core::OwnerKind;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn sources(dir: &Path) -> Sources {
        Sources {
            root: dir.join("root"),
            home: Some(dir.join("home")),
        }
    }

    #[test]
    fn pacman_packages_with_version_and_install_date() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("var/lib/pacman/local/vim-9.1-1/desc"),
            "%NAME%\nvim\n\n%VERSION%\n9.1-1\n\n%INSTALLDATE%\n1700000000\n\n",
        );
        write(&root.join("var/lib/pacman/local/broken-1-1/files"), "%FILES%\n");
        write(&root.join("var/lib/pacman/local/ALPM_DB_VERSION"), "9\n");
        assert_eq!(
            pacman(root),
            [Program {
                name: "vim".into(),
                kind: AppKind::Package,
                source: "pacman",
                id: "vim".into(),
                version: Some("9.1-1".into()),
                installed: Some(1_700_000_000),
                owner: Owner::new(OwnerKind::System, "vim", "pacman"),
            }]
        );
    }

    #[test]
    fn dpkg_lists_installed_packages_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("var/lib/dpkg/status"),
            "Package: htop\nStatus: install ok installed\nArchitecture: amd64\nVersion: 3.3.0-4\n\n\
             Package: gone\nStatus: deinstall ok config-files\nVersion: 1.0\n\n\
             Package: libc6\nStatus: install ok installed\nArchitecture: amd64\nVersion: 2.39-0\nDescription: GNU C Library\n continued\n",
        );
        write(&root.join("var/lib/dpkg/info/libc6:amd64.list"), "/.\n");
        let programs = dpkg(root);
        let summary: Vec<_> = programs
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    p.version.as_deref(),
                    p.installed.is_some(),
                    p.owner.detail.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [("htop", Some("3.3.0-4"), false, "dpkg"), ("libc6", Some("2.39-0"), true, "dpkg")]
        );
    }

    #[test]
    fn rpm_reads_its_query_and_skips_a_root_without_a_database() {
        let programs = rpm_query("bash\t5.2.26-3.fc40\t1712345678\ngpg-pubkey\tabc-def\t1\nzlib\t1.3-1\t(none)\n");
        let summary: Vec<_> = programs
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_deref(), p.installed, p.owner.detail.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("bash", Some("5.2.26-3.fc40"), Some(1_712_345_678), "rpm"),
                ("zlib", Some("1.3-1"), None, "rpm")
            ]
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(rpm(dir.path()).is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn flatpaks_system_wide_and_per_user() {
        let dir = tempfile::tempdir().unwrap();
        let s = sources(dir.path());
        let gimp = s.root.join("var/lib/flatpak/app/org.gimp.GIMP");
        let active = gimp.join("x86_64/stable/active");
        write(
            &active.join("export/share/applications/org.gimp.GIMP.desktop"),
            "[Desktop Entry]\nName=GNU Image Manipulation Program\n[Desktop Action new]\nName=New\n",
        );
        write(
            &active.join("files/share/metainfo/org.gimp.GIMP.metainfo.xml"),
            "<component><releases><release version=\"2.10.38\" date=\"2024-05-02\"/><release version=\"2.10.36\"/></releases></component>",
        );
        std::os::unix::fs::symlink("x86_64/stable", gimp.join("current")).unwrap();
        let zeal = s.home.as_ref().unwrap().join(".local/share/flatpak/app/org.zealdocs.Zeal");
        fs::create_dir_all(zeal.join("x86_64/stable/active")).unwrap();
        std::os::unix::fs::symlink("x86_64/stable", zeal.join("current")).unwrap();
        // Not deployed: no `current`.
        fs::create_dir_all(s.root.join("var/lib/flatpak/app/org.half.Done")).unwrap();
        let summary: Vec<_> = flatpak(&s)
            .into_iter()
            .map(|p| (p.name, p.id, p.version, p.installed.is_some(), p.owner))
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "GNU Image Manipulation Program".to_owned(),
                    "org.gimp.GIMP".to_owned(),
                    Some("2.10.38".to_owned()),
                    true,
                    Owner::new(OwnerKind::Program, "GIMP", "Flatpak org.gimp.GIMP")
                ),
                (
                    "Zeal".into(),
                    "org.zealdocs.Zeal".into(),
                    None,
                    true,
                    Owner::new(OwnerKind::Program, "Zeal", "Flatpak org.zealdocs.Zeal")
                ),
            ]
        );
    }

    #[test]
    fn steam_games_from_every_library_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let own = home.join(".local/share/Steam/steamapps");
        let other = dir.path().join("games");
        write(
            &own.join("libraryfolders.vdf"),
            &format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                home.join(".local/share/Steam").display(),
                other.display()
            ),
        );
        let manifest = |id: &str, name: &str, dir: &str| {
            format!(
                "\"AppState\"\n{{\n\t\"appid\"\t\t\"{id}\"\n\t\"name\"\t\t\"{name}\"\n\t\"installdir\"\t\t\"{dir}\"\n\t\"buildid\"\t\t\"1234\"\n\t\"LastUpdated\"\t\t\"1710000000\"\n}}\n"
            )
        };
        write(&own.join("appmanifest_427520.acf"), &manifest("427520", "Factorio", "Factorio"));
        fs::create_dir_all(own.join("common/Factorio")).unwrap();
        write(
            &other.join("steamapps/appmanifest_427520.acf"),
            &manifest("427520", "Factorio", "Factorio"),
        );
        fs::create_dir_all(other.join("steamapps/common/Factorio")).unwrap();
        write(
            &other.join("steamapps/appmanifest_70.acf"),
            &manifest("70", "Half-Life", "Half-Life"),
        );
        write(&other.join("steamapps/appmanifest_9.acf"), &manifest("9", "Uninstalled", "Missing"));
        fs::create_dir_all(other.join("steamapps/common/Half-Life")).unwrap();
        let mut summary: Vec<_> = steam(&home)
            .into_iter()
            .map(|p| (p.name, p.id, p.version, p.installed, p.owner))
            .collect();
        summary.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            summary,
            [
                (
                    "Factorio".to_owned(),
                    "427520".to_owned(),
                    Some("build 1234".to_owned()),
                    Some(1_710_000_000),
                    Owner::new(OwnerKind::Game, "Factorio", "Steam")
                ),
                (
                    "Half-Life".into(),
                    "70".into(),
                    Some("build 1234".into()),
                    Some(1_710_000_000),
                    Owner::new(OwnerKind::Game, "Half-Life", "Steam")
                ),
            ]
        );
    }

    #[test]
    fn opt_folders_are_programs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("opt/brave.com/brave")).unwrap();
        write(&root.join("opt/README"), "not a program");
        let programs = opt(root);
        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0].name, "brave.com");
        assert_eq!(programs[0].owner, Owner::new(OwnerKind::Program, "brave.com", "/opt"));
    }

    #[test]
    fn read_gathers_every_source() {
        let dir = tempfile::tempdir().unwrap();
        let s = sources(dir.path());
        write(&s.root.join("var/lib/pacman/local/vim-9.1-1/desc"), "%NAME%\nvim\n");
        fs::create_dir_all(s.root.join("opt/tool")).unwrap();
        let kinds: Vec<_> = read(&s).into_iter().map(|p| (p.kind, p.source)).collect();
        assert_eq!(kinds, [(AppKind::Package, "pacman"), (AppKind::Program, "/opt")]);
    }
}
