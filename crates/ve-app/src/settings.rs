use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use ve_core::CategoryMap;
use ve_scan::VolumeInfo;

const APP_DIR: &str = "view-everything";

/// User choices that outlive a session, stored as `settings.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// Removable Volumes the user asked to index, by identity. A mount path from an older
    /// settings file still matches the Volume mounted there.
    #[serde(default)]
    pub include: Vec<String>,
    /// Fixed Volumes the user asked not to index, by identity.
    #[serde(default)]
    pub exclude: Vec<String>,
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

impl Default for Settings {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            refresh_minutes: default_refresh_minutes(),
            volume_refresh_minutes: BTreeMap::new(),
            theme: ThemeChoice::System,
        }
    }
}

/// Whether a settings key names `volume`: its identity, or (older files) its mount path.
fn names(key: &str, volume: &VolumeInfo) -> bool {
    key == volume.identity || Path::new(key) == volume.mount
}

impl Settings {
    /// Local fixed Volumes are indexed unless excluded; removable ones only when included.
    pub fn is_indexed(&self, volume: &VolumeInfo) -> bool {
        if self.exclude.iter().any(|k| names(k, volume)) {
            false
        } else {
            !volume.removable || self.include.iter().any(|k| names(k, volume))
        }
    }

    /// Records whether the user wants `volume` indexed.
    pub fn set_indexed(&mut self, volume: &VolumeInfo, indexed: bool) {
        self.include.retain(|k| !names(k, volume));
        self.exclude.retain(|k| !names(k, volume));
        match (volume.removable, indexed) {
            (true, true) => self.include.push(volume.identity.clone()),
            (false, false) => self.exclude.push(volume.identity.clone()),
            _ => {}
        }
    }

    /// Drops everything remembered about the Volume `identity`.
    pub fn forget(&mut self, identity: &str) {
        let is = |k: &String| k == identity;
        self.include.retain(|k| !is(k));
        self.exclude.retain(|k| !is(k));
        self.volume_refresh_minutes.retain(|k, _| !is(k));
    }

    /// The refresh interval of `volume`, in minutes.
    pub fn refresh_minutes_for(&self, volume: &VolumeInfo) -> u64 {
        self.volume_refresh_minutes
            .iter()
            .find(|(k, _)| names(k, volume))
            .map_or(self.refresh_minutes, |(_, m)| *m)
    }

    /// The refresh interval of each of `volumes`, by mount point.
    pub fn intervals(&self, volumes: &[VolumeInfo]) -> HashMap<PathBuf, Duration> {
        volumes
            .iter()
            .map(|v| {
                let minutes = self.refresh_minutes_for(v).max(1);
                (v.mount.clone(), Duration::from_secs(minutes * 60))
            })
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

    fn volume(mount: &str, removable: bool) -> VolumeInfo {
        VolumeInfo {
            mount: mount.into(),
            identity: format!("uuid:{mount}"),
            fs_type: "ext4".into(),
            label: String::new(),
            removable,
            read_only: false,
        }
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

        s.forget(&usb.identity);
        assert!(!s.is_indexed(&usb));
        assert!(s.include.is_empty() && s.volume_refresh_minutes.is_empty());
    }
}
