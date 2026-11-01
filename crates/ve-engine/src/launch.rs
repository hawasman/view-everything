//! `launch.match`: the Launcher's matches of Entry names in the Index, best first. A name that
//! starts with the text comes first, then one with a word starting with it (after a separator
//! such as a space, `-`, `_` or `.`, or at a capital after a small letter, as in `MyReport`), then
//! one holding it anywhere; within each, the most recently modified first. Matching ignores case.

use crate::{Engine, Phase};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
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
}

/// One Entry the Launcher lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    pub kind: Kind,
    pub name: String,
    /// Full path, lossily made UTF-8.
    pub path: String,
    /// On an Offline Volume: the path is not there to open.
    pub offline: bool,
    /// On a Remote: the path is the server's, not one this machine can open.
    pub remote: bool,
}

/// How well a name matches, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Quality {
    Start,
    WordStart,
    Inside,
}

/// A candidate's sort key: quality, newest first, shorter name first, then Entry id so the
/// order is stable.
type Key = (Quality, Reverse<i64>, usize, u32);

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
        path: index.path(id).to_string_lossy().into_owned(),
        offline: volume.is_some_and(|v| !v.online) && !remote,
        remote,
    }
}

impl Engine {
    /// `launch.match`: at most [`LIMIT`] Entries whose names hold `text`, best first (see the
    /// module). Blank text matches nothing. `Err` while the Index is still loading.
    pub fn launch_match(&self, text: &str) -> Result<Vec<Match>, String> {
        let needle = text.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        if matches!(self.shared.status.lock().phase, Phase::LoadingIndex) {
            return Err("the app is still loading its Index; try again in a moment".into());
        }
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
                        keys.push((q, Reverse(index.modified(id)), name.len(), n));
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
        keep_best(&mut best);
        best.sort_unstable();
        Ok(best.into_iter().map(|(.., n)| row(&index, EntryId(n))).collect())
    }
}
