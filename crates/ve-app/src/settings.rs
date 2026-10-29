use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use ve_core::{CategoryMap, MANUAL_REFRESH, Remote};
use ve_scan::VolumeInfo;

const APP_DIR: &str = "view-everything";

/// User choices that outlive a session, stored as `settings.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// Removable Volumes and network shares the user asked to index, by identity. A mount path from an older
    /// settings file still matches the Volume mounted there.
    #[serde(default)]
    pub include: Vec<String>,
    /// Fixed Volumes the user asked not to index, by identity.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Removable Volumes the user answered "Not now" to when offered, by identity.
    #[serde(default)]
    pub declined: Vec<String>,
    /// How often a walked Volume is re-walked while the app runs, in minutes, and how old its
    /// Last Synced may be before launch Catch-up re-walks it. Applies to Volumes without a
    /// refresh interval of their own.
    #[serde(default = "default_refresh_minutes")]
    pub refresh_minutes: u64,
    /// Per-Volume refresh interval in minutes, by identity.
    #[serde(default)]
    pub volume_refresh_minutes: BTreeMap<String, u64>,
    /// Light, Dark, or following the system.
    #[serde(default)]
    pub theme: ThemeChoice,
    /// The Remotes (SSH, S3 and FTP) the user added. Their refresh interval is in `volume_refresh_minutes`
    /// under their identity; Manual unless set.
    #[serde(default)]
    pub remotes: Vec<Remote>,
    /// The Query behind the Dashboard's Recently modified table; the default when unset.
    #[serde(default)]
    pub recent_query: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

fn default_refresh_minutes() -> u64 {
    30
}

/// Slow, shared links make a re-walk expensive, so network shares are re-read less often.
const NETWORK_REFRESH_MINUTES: u64 = 120;

impl Default for Settings {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            declined: Vec::new(),
            refresh_minutes: default_refresh_minutes(),
            volume_refresh_minutes: BTreeMap::new(),
            theme: ThemeChoice::System,
            remotes: Vec::new(),
            recent_query: None,
        }
    }
}

/// Under the home folder, outside hidden folders: no path part starts with a dot.
pub fn default_recent_query() -> String {
    match dirs::home_dir() {
        Some(home) => format!("\"{}/*\" !\"*/.*\"", home.display()),
        None => "!\"*/.*\"".to_owned(),
    }
}

/// Whether a settings key names `volume`: its identity, or (older files) its mount path.
fn names(key: &str, volume: &VolumeInfo) -> bool {
    key == volume.identity || Path::new(key) == volume.mount
}

impl Settings {
    /// Local fixed Volumes are indexed unless excluded; removable ones and network shares only
    /// when included.
    pub fn is_indexed(&self, volume: &VolumeInfo) -> bool {
        if self.exclude.iter().any(|k| names(k, volume)) {
            false
        } else {
            !volume.opt_in() || self.include.iter().any(|k| names(k, volume))
        }
    }

    /// The Query behind Recently modified: the saved one, or the default.
    pub fn recent_query_text(&self) -> String {
        self.recent_query
            .as_ref()
            .filter(|q| !q.trim().is_empty())
            .cloned()
            .unwrap_or_else(default_recent_query)
    }

    /// Records whether the user wants `volume` indexed.
    pub fn set_indexed(&mut self, volume: &VolumeInfo, indexed: bool) {
        self.include.retain(|k| !names(k, volume));
        self.exclude.retain(|k| !names(k, volume));
        self.declined.retain(|k| !names(k, volume));
        match (volume.opt_in(), indexed) {
            (true, true) => self.include.push(volume.identity.clone()),
            (false, false) => self.exclude.push(volume.identity.clone()),
            _ => {}
        }
    }

    /// Remembers "Not now" for `volume`, so it is not offered again.
    pub fn decline(&mut self, volume: &VolumeInfo) {
        if !self.declined.contains(&volume.identity) {
            self.declined.push(volume.identity.clone());
        }
    }

    /// The Volumes in `now` that were not mounted `before` and are worth offering to index:
    /// opt-in (removable or a mounted network share), not indexed, not declined, and not
    /// already in the Index (`in_index`, by identity: a returning Offline Volume catches up on
    /// its own).
    pub fn index_offers(&self, before: &[VolumeInfo], now: &[VolumeInfo], in_index: impl Fn(&str) -> bool) -> Vec<VolumeInfo> {
        now.iter()
            .filter(|v| v.opt_in() && !self.is_indexed(v))
            .filter(|v| !before.iter().any(|b| b.identity == v.identity))
            .filter(|v| !self.declined.contains(&v.identity) && !in_index(&v.identity))
            .cloned()
            .collect()
    }

    /// Drops everything remembered about the Volume `identity`, also under its older
    /// mount-path key.
    pub fn forget(&mut self, identity: &str, mount: &Path) {
        let is = |k: &String| k == identity || Path::new(k) == mount;
        self.include.retain(|k| !is(k));
        self.exclude.retain(|k| !is(k));
        self.declined.retain(|k| !is(k));
        self.volume_refresh_minutes.retain(|k, _| !is(k));
    }

    /// The refresh interval of `volume`, in minutes.
    pub fn refresh_minutes_for(&self, volume: &VolumeInfo) -> u64 {
        self.volume_refresh_minutes.iter().find(|(k, _)| names(k, volume)).map_or_else(
            || {
                if volume.network {
                    NETWORK_REFRESH_MINUTES
                } else {
                    self.refresh_minutes
                }
            },
            |(_, m)| *m,
        )
    }

    /// Adds a Remote (the same one twice is one). Returns whether it was new.
    pub fn add_remote(&mut self, remote: Remote, refresh_minutes: u64) -> bool {
        // One Remote per identity: S3 and FTP ones that differ only in keys, profile or TLS are the
        // same Location, and the same keyring entry. The later one replaces the earlier.
        let id = remote.identity();
        let new = !self.remotes.contains(&remote);
        self.volume_refresh_minutes.insert(id.clone(), refresh_minutes);
        match self.remotes.iter().position(|r| r.identity() == id) {
            Some(at) => self.remotes[at] = remote,
            None => self.remotes.push(remote),
        }
        new
    }

    pub fn remove_remote(&mut self, remote: &Remote) {
        self.remotes.retain(|r| r != remote);
        self.volume_refresh_minutes.remove(&remote.identity());
    }

    /// Puts `new` where the Remote `old` was, with its refresh interval. A different Remote of the
    /// same identity as `new` is the same Location, and goes.
    pub fn replace_remote(&mut self, old: &Remote, new: Remote, refresh_minutes: u64) {
        let id = new.identity();
        self.volume_refresh_minutes.remove(&old.identity());
        self.volume_refresh_minutes.insert(id.clone(), refresh_minutes);
        self.remotes.retain(|r| r == old || r.identity() != id);
        match self.remotes.iter().position(|r| r == old) {
            Some(at) => self.remotes[at] = new,
            None => self.remotes.push(new),
        }
    }

    /// The refresh interval of a Remote, in minutes: Manual (0) unless chosen.
    pub fn remote_refresh_minutes(&self, remote: &Remote) -> u64 {
        self.volume_refresh_minutes.get(&remote.identity()).copied().unwrap_or(0)
    }

    /// The refresh interval of each of `volumes`, and of each Remote, by mount point.
    pub fn intervals(&self, volumes: &[VolumeInfo]) -> HashMap<PathBuf, Duration> {
        // ponytail: Manual (0) is an interval too long to ever come due.
        let every = |minutes: u64| {
            if minutes == 0 {
                MANUAL_REFRESH
            } else {
                Duration::from_secs(minutes * 60)
            }
        };
        volumes
            .iter()
            .map(|v| (v.mount.clone(), every(self.refresh_minutes_for(v))))
            .chain(
                self.remotes
                    .iter()
                    .map(|r| (PathBuf::from(r.identity()), every(self.remote_refresh_minutes(r)))),
            )
            .collect()
    }

    pub fn load() -> Result<Settings, String> {
        let Some(path) = settings_path() else {
            return Ok(Settings::default());
        };
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = settings_path().ok_or("no configuration folder on this system")?;
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_file(&path, &text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, text)
}

fn settings_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(APP_DIR).join("settings.toml"))
}

pub fn categories_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(APP_DIR).join("categories.toml"))
}

pub fn index_path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join(APP_DIR).join("index.bin"))
}

/// The built-in Categories with the user's `categories.toml` overrides, if any.
pub fn load_categories() -> Result<CategoryMap, String> {
    let Some(path) = categories_path() else {
        return Ok(CategoryMap::builtin());
    };
    match fs::read_to_string(&path) {
        Ok(text) => CategoryMap::from_toml(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CategoryMap::builtin()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ve_core::S3Remote;

    fn volume(mount: &str, removable: bool) -> VolumeInfo {
        VolumeInfo {
            mount: mount.into(),
            identity: format!("uuid:{mount}"),
            fs_type: "ext4".into(),
            label: String::new(),
            removable,
            network: false,
            read_only: false,
        }
    }

    fn share() -> VolumeInfo {
        VolumeInfo {
            identity: "share:nas.lan:/volume1/media".into(),
            fs_type: "nfs4".into(),
            network: true,
            ..volume("/mnt/nas", false)
        }
    }

    #[test]
    fn a_network_share_is_opt_in_and_refreshes_every_two_hours() {
        let (nas, fixed) = (share(), volume("/", false));
        let mut s = Settings::default();
        assert!(!s.is_indexed(&nas));
        s.set_indexed(&nas, true);
        assert!(s.is_indexed(&nas) && s.exclude.is_empty());
        // Mounted elsewhere, it is the same share; its interval ignores the general default.
        let moved = VolumeInfo {
            mount: "/media/nas".into(),
            ..nas.clone()
        };
        assert!(s.is_indexed(&moved));
        assert_eq!(s.refresh_minutes_for(&moved), 120);
        assert_eq!(s.refresh_minutes_for(&fixed), 30);
        s.volume_refresh_minutes.insert(nas.identity.clone(), 10);
        assert_eq!(s.refresh_minutes_for(&nas), 10);
        s.set_indexed(&nas, false);
        assert!(s.include.is_empty() && !s.is_indexed(&nas));
    }

    #[test]
    fn an_s3_remote_is_saved_without_any_secret_and_an_ssh_one_still_loads() {
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let s3 = S3Remote::new("bkt", "a/b", "minio.lan:9000", "us-east-1", true, "", "AKIAEXAMPLE").unwrap();
        let mut s = Settings::default();
        s.add_remote(Remote::S3(s3.clone()), 0);
        s.add_remote(Remote::new("me@box", "/srv").unwrap(), 60);
        let text = toml::to_string(&s).unwrap();
        // The typed secret is not a field of anything saved; only the key ID and the connection are.
        assert!(!text.contains(secret) && !text.to_lowercase().contains("secret"), "{text}");
        assert!(text.contains("access_key = \"AKIAEXAMPLE\""));
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.remotes, s.remotes);
        assert_eq!(back.remotes[0], Remote::S3(s3));
        // A settings file from before S3 Remotes has only SSH ones, in their old shape.
        let old: Settings = toml::from_str("[[remotes]]\nhost = \"me@box\"\npath = \"/srv\"\n").unwrap();
        assert_eq!(old.remotes, [Remote::new("me@box", "/srv").unwrap()]);
    }

    #[test]
    fn an_ftp_remote_is_saved_with_its_tls_mode_and_no_password_and_ssh_and_s3_still_load_beside_it() {
        let ftp = Remote::Ftp(ve_core::FtpRemote::new("ftp.example.com", 2121, "me", "/pub", ve_core::FtpTls::Plain).unwrap());
        let mut s = Settings::default();
        s.add_remote(ftp.clone(), 0);
        s.add_remote(Remote::new("me@box", "/srv").unwrap(), 60);
        s.add_remote(Remote::S3(S3Remote::new("bkt", "", "", "us-east-1", false, "", "").unwrap()), 0);
        let text = toml::to_string(&s).unwrap();
        assert!(
            !text.to_lowercase().contains("password") && !text.to_lowercase().contains("secret"),
            "{text}"
        );
        for field in [
            "host = \"ftp.example.com\"",
            "port = 2121",
            "user = \"me\"",
            "path = \"/pub\"",
            "tls = \"plain\"",
        ] {
            assert!(text.contains(field), "{field} in {text}");
        }
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.remotes, s.remotes, "each Remote comes back as the kind it was");
        // TLS mode is Explicit unless the file says otherwise.
        let file: Settings = toml::from_str("[[remotes]]\nhost = \"h\"\nport = 21\nuser = \"u\"\npath = \"/\"\n").unwrap();
        assert_eq!(
            file.remotes,
            [Remote::Ftp(
                ve_core::FtpRemote::new("h", 21, "u", "/", ve_core::FtpTls::Explicit).unwrap()
            )]
        );
        assert_eq!(s.remote_refresh_minutes(&ftp), 0);
    }

    #[test]
    fn remotes_of_one_identity_replace_each_other() {
        let mut s = Settings::default();
        s.add_remote(Remote::S3(S3Remote::new("b", "", "", "us-east-1", false, "work", "").unwrap()), 0);
        s.add_remote(Remote::S3(S3Remote::new("b", "", "", "us-east-1", false, "", "AKIA").unwrap()), 60);
        assert_eq!(s.remotes.len(), 1);
        assert_eq!(
            s.remotes[0],
            Remote::S3(S3Remote::new("b", "", "", "us-east-1", false, "", "AKIA").unwrap())
        );
    }

    #[test]
    fn an_edited_remote_keeps_its_place_and_takes_its_interval_to_its_new_identity() {
        let (a, b, c) = (
            Remote::new("me@a", "/srv").unwrap(),
            Remote::new("me@b", "/srv").unwrap(),
            Remote::new("me@c", "/srv").unwrap(),
        );
        let mut s = Settings::default();
        for r in [&a, &b, &c] {
            s.add_remote(r.clone(), 60);
        }
        let moved = Remote::new("me@b", "/data").unwrap();
        s.replace_remote(&b, moved.clone(), 120);
        assert_eq!(s.remotes, [a.clone(), moved.clone(), c.clone()]);
        assert!(!s.volume_refresh_minutes.contains_key(&b.identity()));
        assert_eq!(s.remote_refresh_minutes(&moved), 120);
        // Edited into the Location of another Remote: one Remote, where the edited one was.
        s.replace_remote(&moved, c.clone(), 15);
        assert_eq!(s.remotes, [a, c.clone()]);
        assert_eq!(s.remote_refresh_minutes(&c), 15);
    }

    #[test]
    fn remotes_default_to_manual_and_keep_their_interval() {
        let remote = Remote::new("me@box", "/srv/backup").unwrap();
        let mut s = Settings::default();
        assert!(s.add_remote(remote.clone(), 0));
        assert!(!s.add_remote(remote.clone(), 120));
        assert_eq!(s.remotes.len(), 1);
        let back: Settings = toml::from_str(&toml::to_string_pretty(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let key = PathBuf::from("ssh:me@box:/srv/backup");
        assert_eq!(back.intervals(&[])[&key], Duration::from_secs(120 * 60));
        s.volume_refresh_minutes.clear();
        assert_eq!(s.intervals(&[])[&key], MANUAL_REFRESH);
        s.remove_remote(&remote);
        assert!(s.remotes.is_empty() && s.intervals(&[]).is_empty());
        // An older settings file has no Remotes.
        assert!(toml::from_str::<Settings>("include = []").unwrap().remotes.is_empty());
    }

    #[test]
    fn volume_choices_and_other_settings_survive_a_save() {
        let (fixed, usb) = (volume("/", false), volume("/mnt/usb", true));
        let mut s = Settings::default();
        assert!(s.is_indexed(&fixed) && !s.is_indexed(&usb));
        s.set_indexed(&fixed, false);
        s.set_indexed(&usb, true);
        s.refresh_minutes = 5;
        s.volume_refresh_minutes.insert(usb.identity.clone(), 90);
        s.theme = ThemeChoice::Dark;
        let back: Settings = toml::from_str(&toml::to_string_pretty(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert!(!back.is_indexed(&fixed) && back.is_indexed(&usb));
        assert_eq!(back.refresh_minutes_for(&fixed), 5);
        assert_eq!(back.refresh_minutes_for(&usb), 90);
        let intervals = back.intervals(&[fixed.clone(), usb.clone()]);
        assert_eq!(intervals[&usb.mount], Duration::from_secs(90 * 60));
        // Restoring the default leaves no override behind.
        s.set_indexed(&fixed, true);
        assert!(s.exclude.is_empty());
    }

    #[test]
    fn a_remounted_volume_keeps_its_choices_and_forget_drops_them() {
        let mut usb = volume("/mnt/usb", true);
        let mut s = Settings::default();
        s.set_indexed(&usb, true);
        s.volume_refresh_minutes.insert(usb.identity.clone(), 90);
        // Plugged in elsewhere: same identity, so same choices.
        usb.mount = "/run/media/me/usb".into();
        assert!(s.is_indexed(&usb));
        assert_eq!(s.refresh_minutes_for(&usb), 90);
        // A settings file from before identities named it by mount path.
        let old: Settings = toml::from_str("include = [\"/run/media/me/usb\"]").unwrap();
        assert!(old.is_indexed(&usb));

        s.forget(&usb.identity, &usb.mount);
        assert!(!s.is_indexed(&usb));
        assert!(s.include.is_empty() && s.volume_refresh_minutes.is_empty());
    }

    #[test]
    fn a_new_removable_volume_is_offered_until_the_user_says_not_now() {
        let (usb, disk) = (volume("/mnt/usb", true), volume("/mnt/data", false));
        let mut s = Settings::default();
        let offers = |s: &Settings, before: &[VolumeInfo], known: bool| s.index_offers(before, &[usb.clone(), disk.clone()], |_| known);
        // Newly mounted, never seen: offered. A fixed Volume is indexed already, never offered.
        assert_eq!(offers(&s, &[], false), vec![usb.clone()]);
        // Already mounted before this change, or already in the Index (returning): no prompt.
        assert!(offers(&s, std::slice::from_ref(&usb), false).is_empty());
        assert!(offers(&s, &[], true).is_empty());
        // "Not now" is remembered by identity, also across a save and at another mount path.
        s.decline(&usb);
        let back: Settings = toml::from_str(&toml::to_string_pretty(&s).unwrap()).unwrap();
        assert!(offers(&back, &[], false).is_empty());
        let moved = VolumeInfo {
            mount: "/run/media/usb".into(),
            ..usb.clone()
        };
        assert!(back.index_offers(&[], &[moved], |_| false).is_empty());
        // Accepting later (Settings tick) clears the answer; Forget drops it too.
        s.set_indexed(&usb, true);
        assert!(s.declined.is_empty() && s.is_indexed(&usb));
        s.decline(&usb);
        s.forget(&usb.identity, &usb.mount);
        assert!(s.declined.is_empty());
    }

    #[test]
    fn the_recent_query_defaults_to_home_outside_hidden_folders_and_persists() {
        use ve_core::{Candidate, CategoryMap, Query, QueryContext};
        let default = Settings::default();
        let context = QueryContext::new(CategoryMap::builtin(), chrono::Local::now());
        let query = Query::parse(&default.recent_query_text(), &context).unwrap();
        // Candidates carry match paths, which use `/` on every OS (see Index::write_match_path).
        let home = dirs::home_dir().expect("a home folder").display().to_string().replace('\\', "/");
        let holds = |path: &str| {
            let name = path.rsplit('/').next().unwrap();
            query.matches(&Candidate {
                name,
                path: Some(path),
                is_dir: false,
                logical: 0,
                modified: 0,
            })
        };
        assert!(holds(&format!("{home}/Documents/a.pdf")));
        assert!(!holds(&format!("{home}/.cache/a.pdf")));
        assert!(!holds(&format!("{home}/Projects/.git/config")));
        assert!(!holds("/var/lib/other/a.pdf"));
        // An edited Query is saved with the settings and read back.
        let edited = Settings {
            recent_query: Some("type:video".into()),
            ..default
        };
        let back: Settings = toml::from_str(&toml::to_string_pretty(&edited).unwrap()).unwrap();
        assert_eq!(back.recent_query_text(), "type:video");
        // A blank one means the default again.
        assert_eq!(
            Settings {
                recent_query: Some("  ".into()),
                ..back
            }
            .recent_query_text(),
            default_recent_query()
        );
    }
}
