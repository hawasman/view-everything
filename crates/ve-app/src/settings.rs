use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use ve_core::CategoryMap;
use ve_scan::VolumeInfo;

const APP_DIR: &str = "view-everything";

/// User choices that outlive a session, stored as `settings.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// Removable Volumes the user asked to index.
    #[serde(default)]
    pub include: Vec<PathBuf>,
    /// Fixed Volumes the user asked not to index.
    #[serde(default)]
    pub exclude: Vec<PathBuf>,
    /// How often a walked Volume is re-walked while the app runs, in minutes.
    #[serde(default = "default_refresh_minutes")]
    pub refresh_minutes: u64,
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
            theme: ThemeChoice::System,
        }
    }
}

impl Settings {
    /// Local fixed Volumes are indexed unless excluded; removable ones only when included.
    pub fn is_indexed(&self, volume: &VolumeInfo) -> bool {
        if self.exclude.contains(&volume.mount) {
            false
        } else {
            !volume.removable || self.include.contains(&volume.mount)
        }
    }

    /// Records whether the user wants `volume` indexed.
    pub fn set_indexed(&mut self, volume: &VolumeInfo, indexed: bool) {
        self.include.retain(|m| m != &volume.mount);
        self.exclude.retain(|m| m != &volume.mount);
        match (volume.removable, indexed) {
            (true, true) => self.include.push(volume.mount.clone()),
            (false, false) => self.exclude.push(volume.mount.clone()),
            _ => {}
        }
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
        s.theme = ThemeChoice::Dark;
        let back: Settings = toml::from_str(&toml::to_string_pretty(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert!(!back.is_indexed(&fixed) && back.is_indexed(&usb));
        // Restoring the default leaves no override behind.
        s.set_indexed(&fixed, true);
        assert!(s.exclude.is_empty());
    }
}
