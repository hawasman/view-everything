//! Startup items: what starts when the user logs in. XDG autostart entries (the user's and the
//! system's, a user entry hiding the system one of the same name) and the systemd user units
//! enabled in the default target, each with what it runs and the file that defines it.
//! Read-only; read below [`Sources`] like the program readers.

use super::readers::{Sources, group_entries, group_value, names, under};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

/// Where a startup item comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StartupKind {
    /// An XDG autostart desktop entry.
    Autostart,
    /// A systemd user unit enabled in `default.target`.
    Systemd,
}

/// Whose folder defines a startup item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    User,
    System,
}

/// One thing that starts at login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupItem {
    /// What to show: the entry's `Name=`, the unit's `Description=`, else its file name.
    pub name: String,
    pub kind: StartupKind,
    /// The desktop file's or the unit's file name.
    pub id: String,
    /// What it runs: the entry's `Exec=`, the unit's `ExecStart=` lines; `None` when the file
    /// names no command (a timer or socket unit, say).
    pub command: Option<String>,
    /// The file that defines it.
    pub source: PathBuf,
    pub scope: Scope,
    /// The desktops an autostart entry is limited to (`OnlyShowIn=`) or kept out of
    /// (`NotShowIn=`); empty for no limit.
    pub only_in: Vec<String>,
    pub not_in: Vec<String>,
    /// A unit's drop-in files (`<unit>.d/*.conf`), which `command` and `name` take into account,
    /// in the order systemd applies them.
    pub drop_ins: Vec<PathBuf>,
}

impl StartupItem {
    /// Tells startup items apart: `startup:<kind>:<file name>`, a key no program's has.
    pub fn key(&self) -> String {
        format!("startup:{}:{}", self.kind.name(), self.id)
    }
}

impl StartupKind {
    pub fn name(self) -> &'static str {
        match self {
            StartupKind::Autostart => "Autostart",
            StartupKind::Systemd => "systemd unit",
        }
    }
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::User => "User",
            Scope::System => "System",
        }
    }
}

/// Every startup item the sources define: autostart entries, then systemd units, each by file
/// name.
pub fn read(sources: &Sources) -> Vec<StartupItem> {
    let mut items = autostart(sources);
    items.extend(systemd(sources));
    items.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    items
}

const ENTRY: &str = "Desktop Entry";

/// The autostart entries: `autostart/*.desktop` in the user's configuration folder, then in
/// each system one. A file hides those of the same name in the folders after it, even when it
/// starts nothing itself (`Hidden=true`), which is how a user turns a system entry off.
fn autostart(sources: &Sources) -> Vec<StartupItem> {
    let folders = (sources.config_home.iter().map(|d| (d, Scope::User)))
        .chain(sources.config_dirs.iter().map(|d| (d, Scope::System)))
        .map(|(dir, scope)| (dir.join("autostart"), scope));
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for (folder, scope) in folders {
        for id in names(&folder) {
            if !id.ends_with(".desktop") || !seen.insert(id.clone()) {
                continue;
            }
            let source = folder.join(&id);
            let Ok(text) = fs::read_to_string(&source) else {
                continue;
            };
            let value = |key| group_value(&text, ENTRY, key);
            let off = value("Hidden") == Some("true")
                || value("X-GNOME-Autostart-enabled") == Some("false")
                || value("Type").is_some_and(|t| t != "Application");
            if off {
                continue;
            }
            let list = |key| -> Vec<String> {
                value(key)
                    .map(|v| v.split(';').filter(|d| !d.is_empty()).map(str::to_owned).collect())
                    .unwrap_or_default()
            };
            items.push(StartupItem {
                name: value("Name").unwrap_or_else(|| id.trim_end_matches(".desktop")).to_owned(),
                kind: StartupKind::Autostart,
                command: value("Exec").map(without_field_codes).filter(|c| !c.is_empty()),
                only_in: list("OnlyShowIn"),
                not_in: list("NotShowIn"),
                drop_ins: Vec::new(),
                scope,
                source,
                id,
            });
        }
    }
    items
}

/// An `Exec=` line without the field codes (`%U`, `--file=%f`, ...) a launcher fills in with
/// files or URLs, which it passes none of at login; `%%` stays a literal `%`. A word that was
/// only a field code goes.
fn without_field_codes(exec: &str) -> String {
    exec.split_whitespace()
        .filter_map(|word| {
            let mut out = String::with_capacity(word.len());
            let mut chars = word.chars().peekable();
            while let Some(c) = chars.next() {
                match (c, chars.peek()) {
                    ('%', Some('%')) => {
                        out.push('%');
                        chars.next();
                    }
                    ('%', Some(_)) => {
                        chars.next();
                    }
                    _ => out.push(c),
                }
            }
            (!out.is_empty()).then_some(out)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The systemd user unit search path: its folders, most important first, with whose each is.
fn search_path(sources: &Sources) -> Vec<(PathBuf, Scope)> {
    let mut folders: Vec<(PathBuf, Scope)> = Vec::new();
    folders.extend(sources.config_home.iter().map(|c| (c.join("systemd/user"), Scope::User)));
    folders.extend(sources.config_dirs.iter().map(|d| (d.join("systemd/user"), Scope::System)));
    folders.push((under(&sources.root, "/etc/systemd/user"), Scope::System));
    folders.extend(sources.home.iter().map(|h| (h.join(".local/share/systemd/user"), Scope::User)));
    folders.extend(
        [
            "/usr/local/share/systemd/user",
            "/usr/share/systemd/user",
            "/usr/local/lib/systemd/user",
            "/usr/lib/systemd/user",
        ]
        .map(|p| (under(&sources.root, p), Scope::System)),
    );
    folders
}

/// The user units enabled in the default target: the names in `default.target.wants` of any
/// folder of the search path, each loaded the way systemd loads it, by name from the first
/// folder that has it. A masked unit (linked to `/dev/null`, or empty) or one no folder has
/// does not start and is left out.
fn systemd(sources: &Sources) -> Vec<StartupItem> {
    let folders = search_path(sources);
    let mut wanted: Vec<String> = folders
        .iter()
        .flat_map(|(dir, _)| names(&dir.join("default.target.wants")))
        .collect();
    wanted.sort();
    wanted.dedup();
    wanted.into_iter().filter_map(|unit| unit_item(&folders, unit)).collect()
}

/// What `unit` runs, read from its file and drop-ins in the search path's `folders`.
fn unit_item(folders: &[(PathBuf, Scope)], unit: String) -> Option<StartupItem> {
    // `name@instance.type` falls back to the template `name@.type`.
    let template = unit.split_once('@').and_then(|(prefix, rest)| {
        let (instance, kind) = rest.rsplit_once('.')?;
        (!instance.is_empty()).then(|| (format!("{prefix}@.{kind}"), instance.to_owned()))
    });
    let find = |name: &str| {
        folders.iter().find_map(|(dir, scope)| {
            let file = dir.join(name);
            fs::symlink_metadata(&file).is_ok().then_some((file, *scope))
        })
    };
    let (source, scope) = find(&unit).or_else(|| find(&template.as_ref()?.0))?;
    let text = fs::read_to_string(&source).ok().filter(|t| !t.trim().is_empty())?;

    // Drop-ins of the unit itself, then of its template, in every folder: a file hides those
    // of the same name after it (the instance's over the template's, as systemd does), and
    // systemd applies them by file name.
    let mut confs: HashMap<String, PathBuf> = HashMap::new();
    let unit_names = std::iter::once(unit.as_str()).chain(template.iter().map(|(t, _)| t.as_str()));
    for name in unit_names {
        for (dir, _) in folders {
            let folder = dir.join(format!("{name}.d"));
            for conf in names(&folder).into_iter().filter(|c| c.ends_with(".conf")) {
                confs.entry(conf.clone()).or_insert_with(|| folder.join(conf));
            }
        }
    }
    let mut drop_ins: Vec<(String, PathBuf)> = confs.into_iter().collect();
    drop_ins.sort();
    let drop_ins: Vec<PathBuf> = drop_ins.into_iter().map(|(_, p)| p).collect();
    let texts: Vec<String> = std::iter::once(text)
        .chain(drop_ins.iter().filter_map(|p| fs::read_to_string(p).ok()))
        .collect();

    let mut description = None;
    let mut commands: Vec<&str> = Vec::new();
    for text in &texts {
        if let Some(d) = group_value(text, "Unit", "Description") {
            description = Some(d);
        }
        for (key, value) in group_entries(text, "Service") {
            match (key, value) {
                ("ExecStart", "") => commands.clear(),
                ("ExecStart", command) => commands.push(command.trim_start_matches(['@', '-', ':', '+', '!'])),
                _ => {}
            }
        }
    }
    let instance = |s: &str| match &template {
        Some((_, instance)) => s.replace("%i", instance).replace("%I", instance),
        None => s.to_owned(),
    };
    Some(StartupItem {
        name: description.map_or_else(|| unit.clone(), instance),
        kind: StartupKind::Systemd,
        command: (!commands.is_empty()).then(|| instance(&commands.join("; "))),
        source,
        scope,
        only_in: Vec::new(),
        not_in: Vec::new(),
        drop_ins,
        id: unit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn summary(items: &[StartupItem]) -> Vec<(&str, &str, Option<&str>, Scope)> {
        items
            .iter()
            .map(|i| (i.id.as_str(), i.name.as_str(), i.command.as_deref(), i.scope))
            .collect()
    }

    #[test]
    fn autostart_entries_with_a_user_entry_hiding_the_system_one() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sources::new(dir.path().join("root"), Some(dir.path().join("home")));
        let system = s.root.join("etc/xdg/autostart");
        let user = dir.path().join("home/.config/autostart");
        write(
            &system.join("nm-applet.desktop"),
            "[Desktop Entry]\nType=Application\nName=Network\nExec=nm-applet\nOnlyShowIn=GNOME;XFCE;\n",
        );
        // The user's copy of the same file wins: its command, its path, user scope.
        write(
            &system.join("syncthing.desktop"),
            "[Desktop Entry]\nType=Application\nName=Syncthing\nExec=syncthing serve\n",
        );
        write(
            &user.join("syncthing.desktop"),
            "[Desktop Entry]\nType=Application\nName=Syncthing\nExec=syncthing serve --no-browser %U\n\
             [Desktop Action quit]\nName=Quit\nExec=syncthing quit\n",
        );
        // A user entry with Hidden=true hides the system one and starts nothing itself.
        write(
            &system.join("tracker.desktop"),
            "[Desktop Entry]\nType=Application\nName=Tracker\nExec=tracker-miner\n",
        );
        write(&user.join("tracker.desktop"), "[Desktop Entry]\nHidden=true\n");
        // Switched off the GNOME way.
        write(
            &user.join("off.desktop"),
            "[Desktop Entry]\nType=Application\nName=Off\nExec=off\nX-GNOME-Autostart-enabled=false\n",
        );
        write(&user.join("notes.txt"), "not an entry");
        write(
            &user.join("bare.desktop"),
            "[Desktop Entry]\nType=Application\nExec=/opt/bare/run\n",
        );

        let items: Vec<StartupItem> = read(&s).into_iter().filter(|i| i.kind == StartupKind::Autostart).collect();
        assert_eq!(
            summary(&items),
            [
                ("bare.desktop", "bare", Some("/opt/bare/run"), Scope::User),
                ("nm-applet.desktop", "Network", Some("nm-applet"), Scope::System),
                ("syncthing.desktop", "Syncthing", Some("syncthing serve --no-browser"), Scope::User),
            ]
        );
        assert_eq!(items[2].source, user.join("syncthing.desktop"));
        assert_eq!(items[1].source, system.join("nm-applet.desktop"));
        assert_eq!(items[1].only_in, ["GNOME", "XFCE"]);
    }

    #[test]
    #[cfg(unix)]
    fn systemd_user_units_enabled_in_the_default_target() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let s = Sources::new(dir.path().join("root"), Some(dir.path().join("home")));
        let vendor = s.root.join("usr/lib/systemd/user");
        let user = dir.path().join("home/.config/systemd/user");
        let enable = |wants: &Path, unit: &str| {
            fs::create_dir_all(wants.join("default.target.wants")).unwrap();
            symlink(
                format!("/usr/lib/systemd/user/{unit}"),
                wants.join("default.target.wants").join(unit),
            )
            .unwrap();
        };
        // Enabled by the vendor, its command replaced by a drop-in in /etc.
        write(
            &vendor.join("pipewire.service"),
            "[Unit]\nDescription=PipeWire Multimedia Service\n\n[Service]\nType=simple\nExecStart=/usr/bin/pipewire\n",
        );
        enable(&vendor, "pipewire.service");
        let drop_in = s.root.join("etc/systemd/user/pipewire.service.d/override.conf");
        write(&drop_in, "[Service]\nExecStart=\nExecStart=-/usr/bin/pipewire -v\n");
        // Enabled by the user, whose own copy of the unit hides the vendor's.
        write(
            &vendor.join("syncthing.service"),
            "[Unit]\nDescription=Syncthing\n[Service]\nExecStart=/usr/bin/syncthing serve\n",
        );
        write(
            &user.join("syncthing.service"),
            "[Unit]\nDescription=Syncthing (mine)\n[Service]\nExecStartPre=/bin/true\nExecStart=/home/me/bin/syncthing\n",
        );
        enable(&user, "syncthing.service");
        // An instance of a template, and a unit that runs nothing itself.
        write(
            &vendor.join("tunnel@.service"),
            "[Unit]\nDescription=Tunnel to %i\n[Service]\nExecStart=/usr/bin/tunnel %i\n",
        );
        enable(&user, "tunnel@home.service");
        write(
            &vendor.join("backup.timer"),
            "[Unit]\nDescription=Nightly backup\n[Timer]\nOnCalendar=daily\n",
        );
        enable(&user, "backup.timer");
        // Masked by the user, enabled but missing: neither starts.
        write(&vendor.join("tracker.service"), "[Service]\nExecStart=/usr/bin/tracker\n");
        enable(&vendor, "tracker.service");
        symlink("/dev/null", user.join("tracker.service")).unwrap();
        enable(&user, "gone.service");

        let items: Vec<StartupItem> = read(&s).into_iter().filter(|i| i.kind == StartupKind::Systemd).collect();
        assert_eq!(
            summary(&items),
            [
                ("backup.timer", "Nightly backup", None, Scope::System),
                (
                    "pipewire.service",
                    "PipeWire Multimedia Service",
                    Some("/usr/bin/pipewire -v"),
                    Scope::System
                ),
                ("syncthing.service", "Syncthing (mine)", Some("/home/me/bin/syncthing"), Scope::User),
                ("tunnel@home.service", "Tunnel to home", Some("/usr/bin/tunnel home"), Scope::System),
            ]
        );
        assert_eq!(items[1].source, vendor.join("pipewire.service"));
        assert_eq!(items[1].drop_ins, [drop_in]);
        assert_eq!(items[2].source, user.join("syncthing.service"));
        assert_eq!(items[3].source, vendor.join("tunnel@.service"));
    }
}
