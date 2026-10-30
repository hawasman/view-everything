//! Owners: the program, game, package or project a folder belongs to (CONTEXT.md). An Owner
//! is found at one folder, its Owner root, and inherited by every Entry beneath it unless a
//! nearer Owner root sits in between. Detection is in [`crate::owners`]; storage in the Index.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Which sort of thing an Owner is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OwnerKind {
    /// The operating system and what its package manager installed.
    System,
    /// An application the user installed.
    Program,
    /// Installed by a game launcher.
    Game,
    /// A code project, recognised by its marker files.
    Project,
}

impl OwnerKind {
    pub const ALL: [OwnerKind; 4] = [OwnerKind::System, OwnerKind::Program, OwnerKind::Game, OwnerKind::Project];

    pub fn name(self) -> &'static str {
        match self {
            OwnerKind::System => "System",
            OwnerKind::Program => "Program",
            OwnerKind::Game => "Game",
            OwnerKind::Project => "Project",
        }
    }

    /// Parses an Owner kind name, ignoring case.
    pub fn parse(name: &str) -> Option<OwnerKind> {
        OwnerKind::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(name))
    }
}

impl fmt::Display for OwnerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One Owner as stored in the Index. Two Owner roots with equal `Owner`s are the same Owner
/// (a Steam game installed in two library folders is still one game).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Owner {
    pub kind: OwnerKind,
    /// What to show: the game's title, the application's or package's name, the project folder's name.
    pub name: String,
    /// Where the Owner was recognised from, for tooltips: `Steam`, `Flatpak org.gimp.GIMP`,
    /// `pacman`, the vendor folder under `Program Files`, or the project marker (`Cargo.toml`).
    pub detail: String,
}

impl Owner {
    pub fn new(kind: OwnerKind, name: impl Into<String>, detail: impl Into<String>) -> Owner {
        Owner {
            kind,
            name: name.into(),
            detail: detail.into(),
        }
    }
}

/// Identifies one Owner within an Index; reassigned when the Index is saved and loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OwnerId(pub u32);
