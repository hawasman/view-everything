//! What the Explorer's context menu does to Entries that needs no window state: opening a
//! terminal in a folder and the text it copies.

use crate::format;
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::{App, Window};
use std::io;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use ve_core::{EntryId, Index};

/// Opens a terminal emulator with `dir` as its working folder.
pub fn open_terminal(dir: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open").args(["-a", "Terminal"]).arg(dir).spawn().map(drop)
    }
    #[cfg(windows)]
    {
        Command::new("wt")
            .arg("-d")
            .arg(dir)
            .spawn()
            .or_else(|_| Command::new("cmd").args(["/c", "start", "cmd"]).current_dir(dir).spawn())
            .map(drop)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        if !dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("the folder {} does not exist (any more)", dir.display()),
            ));
        }
        // $TERMINAL may carry arguments ("kitty -e fish").
        let preferred = std::env::var("TERMINAL").ok().filter(|t| !t.trim().is_empty());
        let candidates = preferred.iter().map(String::as_str).chain([
            "x-terminal-emulator",
            "gnome-terminal",
            "konsole",
            "xfce4-terminal",
            "alacritty",
            "kitty",
            "xterm",
        ]);
        let mut last_error = None;
        for candidate in candidates {
            let mut words = candidate.split_whitespace();
            let Some(program) = words.next() else { continue };
            match Command::new(program).args(words).current_dir(dir).spawn() {
                Ok(mut child) => {
                    // Reaps the terminal when it exits, so it does not stay a zombie.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no terminal emulator found; set $TERMINAL")))
    }
}

/// The folder a terminal opens in for `id`: the folder itself, or a file's parent.
pub fn terminal_folder(index: &Index, id: EntryId) -> Option<std::path::PathBuf> {
    index.is_live(id).then(|| index.path(crate::engine::folder_of(index, id)))
}

/// One line per Entry: its path.
pub fn paths_text(index: &Index, ids: &[EntryId]) -> String {
    ids.iter()
        .filter(|&&id| index.is_live(id))
        .map(|&id| index.path(id).display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per Entry: its name, a tab and its Filtered On-disk Size.
pub fn names_and_sizes_text(index: &Index, filter: &ve_core::Filter, ids: &[EntryId]) -> String {
    ids.iter()
        .filter(|&&id| index.is_live(id))
        .map(|&id| format!("{}\t{}", index.display_name(id), format::size(filter.totals(index, id).on_disk)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Builds a list's context menu from the Entries it has selected and the one last clicked.
pub type MenuFn = Rc<dyn Fn(PopupMenu, Vec<EntryId>, Option<EntryId>, &mut Window, &mut App) -> PopupMenu>;
