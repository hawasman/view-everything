//! `launch.match`: the Launcher's matches of Entry names in the Index and of installed programs
//! that have a launch command, best first. A name that starts with the text comes first, then
//! one with a word starting with it (after a separator such as a space, `-`, `_` or `.`, or at a
//! capital after a small letter, as in `MyReport`), then one holding it anywhere; within each,
//! programs before Entries, then the most recent first (an Entry's modification, a program's
//! installation). Matching ignores case.

use crate::Engine;
pub use crate::apps::Launch;
use crate::apps::Program;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use ve_core::{EntryId, Index};

/// The most matches `launch.match` returns.
pub const LIMIT: usize = 50;

/// What a match is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    File,
    /// A folder (or a Volume's root).
    Folder,
    /// An installed program; its [`Match::launch`] says how to start it.
    Program,
}

/// One Entry or program the Launcher lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    pub kind: Kind,
    pub name: String,
    /// Full path. Over the socket it is lossily made UTF-8; in process it is exact, so a name
    /// that is not UTF-8 still opens. A program's is its desktop entry's, else empty.
    #[serde(serialize_with = "lossy")]
    pub path: PathBuf,
    /// On an Offline Volume: the path is not there to open.
    pub offline: bool,
    /// On a Remote: the path is the server's, not one this machine can open.
    pub remote: bool,
    /// A program's launch command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<Launch>,
}

/// How to start a program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// Run this program with these arguments, in this folder when one is named.
    Run { argv: Vec<OsString>, dir: Option<PathBuf> },
    /// Open this URL with the system handler.
    Open(String),
}

impl Launch {
    /// What starting the program takes: a desktop entry's `Exec` line with its field codes
    /// expanded for no files, `flatpak run <id>`, or Steam's `steam://rungameid/<id>` URL.
    pub fn start(&self) -> Result<Start, String> {
        match self {
            Launch::Flatpak(id) => Ok(Start::Run {
                argv: vec!["flatpak".into(), "run".into(), id.into()],
                dir: None,
            }),
            Launch::Steam(id) => Ok(Start::Open(format!("steam://rungameid/{id}"))),
            Launch::DesktopEntry(path) => {
                let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                desktop_start(&text, path).ok_or_else(|| format!("{} has no command to run", path.display()))
            }
        }
    }
}

/// The value of `key` in a desktop entry's `[Desktop Entry]` group, with the string escapes
/// (`\s`, `\n`, `\t`, `\r`, `\\`) undone.
fn entry_value(text: &str, key: &str) -> Option<String> {
    let mut inside = false;
    let mut found = None;
    for line in text.lines().map(str::trim) {
        if let Some(group) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = group == "Desktop Entry";
        } else if inside
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            found = Some(v.trim());
        }
    }
    let raw = found.filter(|v| !v.is_empty())?;
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    Some(out)
}

/// The command a desktop entry runs with no files, per the Desktop Entry Specification: `Exec`
/// split into arguments (double quotes group, and inside them `\` escapes `"`, `` ` ``, `$` and
/// `\`), file and URL field codes dropped, `%c` its name, `%k` its own path, `%i` its icon, `%%`
/// a percent sign; `Path` the folder it runs in.
fn desktop_start(text: &str, path: &Path) -> Option<Start> {
    let exec = entry_value(text, "Exec")?;
    let mut words: Vec<(String, bool)> = Vec::new();
    let (mut word, mut quoted, mut any, mut in_quotes) = (String::new(), false, false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                quoted = true;
                any = true;
            }
            '\\' if in_quotes => word.extend(chars.next()),
            c if c.is_whitespace() && !in_quotes => {
                if any {
                    words.push((std::mem::take(&mut word), quoted));
                }
                (quoted, any) = (false, false);
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if in_quotes {
        return None;
    }
    if any {
        words.push((word, quoted));
    }
    let mut argv = Vec::new();
    for (word, quoted) in words {
        // A field code standing alone; `%i` becomes two arguments.
        if !quoted {
            match word.as_str() {
                "%f" | "%F" | "%u" | "%U" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => continue,
                "%i" => {
                    if let Some(icon) = entry_value(text, "Icon") {
                        argv.extend(["--icon".into(), OsString::from(icon)]);
                    }
                    continue;
                }
                _ => {}
            }
        }
        let mut arg = String::with_capacity(word.len());
        let mut chars = word.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                arg.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => arg.push('%'),
                Some('c') => arg.push_str(&entry_value(text, "Name").unwrap_or_default()),
                Some('k') => arg.push_str(&path.to_string_lossy()),
                // Other codes expand to nothing inside a word.
                _ => {}
            }
        }
        argv.push(OsString::from(arg));
    }
    if argv.first().is_none_or(|a| a.is_empty()) {
        return None;
    }
    Some(Start::Run {
        argv,
        dir: entry_value(text, "Path").map(PathBuf::from),
    })
}

fn lossy<S: serde::Serializer>(path: &Path, out: S) -> Result<S::Ok, S::Error> {
    out.serialize_str(&path.to_string_lossy())
}

/// How well a name matches, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Quality {
    Start,
    WordStart,
    Inside,
}

/// What a candidate is: a program by its place in the program list, or an Entry by id.
/// Programs come first, so a few installed programs are not pushed past the cap by the many
/// files that share their name (an application's icons, translations and documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Found {
    Program(u32),
    Entry(u32),
}

/// A candidate's sort key: quality, programs before Entries, newest first, shorter name first,
/// then what it is so the order is stable.
type Key = (Quality, bool, Reverse<i64>, usize, Found);

/// Whether a match starting at `cur` (after `prev`) starts a word.
fn starts_word(prev: char, cur: char) -> bool {
    !prev.is_alphanumeric() || (prev.is_lowercase() && cur.is_uppercase())
}

/// How `name` matches `needle` (already lower-cased, not empty), if at all.
fn quality(name: &str, needle: &str) -> Option<Quality> {
    if name.is_ascii() && needle.is_ascii() {
        let (h, n) = (name.as_bytes(), needle.as_bytes());
        if h.len() < n.len() {
            return None;
        }
        let mut found = None;
        for at in 0..=h.len() - n.len() {
            if h[at..at + n.len()].eq_ignore_ascii_case(n) {
                if at == 0 {
                    return Some(Quality::Start);
                }
                if starts_word(h[at - 1] as char, h[at] as char) {
                    return Some(Quality::WordStart);
                }
                found = Some(Quality::Inside);
            }
        }
        return found;
    }
    // Other scripts: matched on the lower-cased name, so a word starts only after a separator.
    let lowered = name.to_lowercase();
    let mut found = None;
    for (at, _) in lowered.match_indices(needle) {
        let Some(prev) = lowered[..at].chars().next_back() else {
            return Some(Quality::Start);
        };
        if !prev.is_alphanumeric() {
            return Some(Quality::WordStart);
        }
        found = Some(Quality::Inside);
    }
    found
}

/// Keeps the [`LIMIT`] best of `keys`, unordered.
fn keep_best(keys: &mut Vec<Key>) {
    if keys.len() > LIMIT {
        keys.select_nth_unstable(LIMIT - 1);
        keys.truncate(LIMIT);
    }
}

fn row(index: &Index, id: EntryId) -> Match {
    let volume = index.volume_of(id).and_then(|v| index.volume(v));
    let remote = volume.is_some_and(|v| v.is_remote());
    Match {
        kind: if index.is_dir(id) { Kind::Folder } else { Kind::File },
        name: index.name(id).to_owned(),
        path: index.path(id),
        offline: volume.is_some_and(|v| !v.online) && !remote,
        remote,
        launch: None,
    }
}

fn program_row(program: &Program) -> Match {
    let launch = program.launch.clone();
    Match {
        kind: Kind::Program,
        name: program.name.clone(),
        path: match &launch {
            Some(Launch::DesktopEntry(path)) => path.clone(),
            _ => PathBuf::new(),
        },
        offline: false,
        remote: false,
        launch,
    }
}

impl Engine {
    /// `launch.match`: at most [`LIMIT`] Entries and launchable programs whose names hold
    /// `text`, best first (see the module). Blank text matches nothing. `Err` while the Index is
    /// still loading.
    pub fn launch_match(&self, text: &str) -> Result<Vec<Match>, String> {
        let needle = text.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        if self.is_loading_index() {
            return Err(crate::socket::search::LOADING.into());
        }
        // Read before the Index lock below: reading the sources takes it itself.
        let programs = self.apps.programs(&self.shared);
        let index = self.shared.index.read();
        // Each thread keeps only its best, so a one-letter text over millions of Entries never
        // holds them all.
        let mut best = (0..index.capacity() as u32)
            .into_par_iter()
            .fold(Vec::new, |mut keys: Vec<Key>, n| {
                let id = EntryId(n);
                if index.is_live(id) {
                    let name = index.name(id);
                    if let Some(q) = quality(name, &needle) {
                        keys.push((q, true, Reverse(index.modified(id)), name.len(), Found::Entry(n)));
                        if keys.len() >= 2 * LIMIT {
                            keep_best(&mut keys);
                        }
                    }
                }
                keys
            })
            .reduce(Vec::new, |mut a, b| {
                a.extend(b);
                keep_best(&mut a);
                a
            });
        for (n, program) in programs.iter().enumerate() {
            if program.launch.is_some()
                && let Some(q) = quality(&program.name, &needle)
            {
                // Installed in seconds; Entries are modified in nanoseconds.
                let when = program.installed.map_or(i64::MIN, |s| s.saturating_mul(1_000_000_000));
                best.push((q, false, Reverse(when), program.name.len(), Found::Program(n as u32)));
            }
        }
        keep_best(&mut best);
        best.sort_unstable();
        Ok(best
            .into_iter()
            .map(|(.., found)| match found {
                Found::Entry(n) => row(&index, EntryId(n)),
                Found::Program(n) => program_row(&programs[n as usize]),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(exec: &str) -> Option<Start> {
        let text = format!("[Desktop Entry]\nName=My App\nIcon=my-app\nPath=/srv\nExec={exec}\n[Desktop Action new]\nExec=other\n");
        desktop_start(&text, Path::new("/apps/my.desktop"))
    }

    fn argv(exec: &str) -> Vec<String> {
        match run(exec) {
            Some(Start::Run { argv, .. }) => argv.into_iter().map(|a| a.into_string().unwrap()).collect(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn exec_drops_file_codes_expands_the_rest_and_honours_quotes() {
        assert_eq!(argv("firefox %u"), ["firefox"]);
        assert_eq!(
            argv("app --name=%c %i -f %k 100%%"),
            ["app", "--name=My App", "--icon", "my-app", "-f", "/apps/my.desktop", "100%"]
        );
        assert_eq!(
            argv(r#""/opt/My App/run" --say "a \"b\" \\\\c" %F"#),
            ["/opt/My App/run", "--say", r#"a "b" \c"#]
        );
        assert_eq!(argv(r"sh\s-c"), ["sh", "-c"]);
        assert_eq!(
            run("x"),
            Some(Start::Run {
                argv: vec!["x".into()],
                dir: Some("/srv".into())
            })
        );
        assert_eq!(run(r#""unterminated"#), None);
        assert_eq!(run("%U"), None);
    }
}
