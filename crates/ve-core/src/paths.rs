//! Where the app keeps its files; shared so the CLI reads the same Index and Categories.

use crate::CategoryMap;
use std::fs;
use std::path::PathBuf;

pub const APP_DIR: &str = "view-everything";

pub fn index_path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join(APP_DIR).join("index.bin"))
}

pub fn categories_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(APP_DIR).join("categories.toml"))
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
