use crate::bits::BitSet;
use crate::index::{EntryId, Index};
use crate::query::{Candidate, Query};
use rayon::prelude::*;
use std::collections::HashSet;

/// Sizes and file count summed over counted Entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub logical: u64,
    pub on_disk: u64,
    pub files: u64,
}

impl Totals {
    pub fn add(&mut self, other: Totals) {
        self.logical += other.logical;
        self.on_disk += other.on_disk;
        self.files += other.files;
    }
}

/// The result of applying a Query (or none) to an Index: which Entries match, and every
/// folder's Filtered Size.
///
/// A file counts when it matches the Query, or when it sits inside a matching folder and is not
/// excluded by one of the Query's negated terms. With no Query every file counts, so Filtered
/// Size equals Folder Size. A hard-linked file counts once per Volume.
pub struct Filter {
    generation: u64,
    matched: BitSet,
    counted: BitSet,
    shown: BitSet,
    folders: Vec<Totals>,
    /// Hard links to a file already counted through another link.
    repeated_links: BitSet,
}

impl Filter {
    pub fn compute(index: &Index, query: Option<&Query>) -> Filter {
        let n = index.capacity();
        let matched = match query {
            None => BitSet::full(n),
            Some(query) => match_all(index, query),
        };
        let mut counted = BitSet::new(n);
        let mut shown = BitSet::new(n);
        let mut repeated_links = BitSet::new(n);
        let mut folders = vec![Totals::default(); index.folder_slots()];
        let mut scratch = (String::new(), Vec::new());

        for (_, volume) in index.volumes() {
            let mut seen_links = HashSet::new();
            let mut folders_preorder = Vec::new();
            let mut stack = vec![(volume.root, false)];
            while let Some((dir, inherited)) = stack.pop() {
                folders_preorder.push(dir);
                let captured = inherited || (query.is_some() && matched.contains(dir.index()));
                let dir_slot = index.folder_slot(dir).expect("only folders are stacked");
                for &child in index.children(dir) {
                    if index.is_dir(child) {
                        stack.push((child, captured));
                        continue;
                    }
                    let counts = matched.contains(child.index())
                        || (captured && query.is_none_or(|q| q.passes_exclusions(&candidate(index, q, child, &mut scratch))));
                    if !counts {
                        continue;
                    }
                    counted.insert(child.index());
                    shown.insert(child.index());
                    if index.is_hardlinked(child) && !seen_links.insert(index.key(child)) {
                        repeated_links.insert(child.index());
                        continue;
                    }
                    folders[dir_slot].add(Totals {
                        logical: index.logical(child),
                        on_disk: index.on_disk(child),
                        files: 1,
                    });
                }
            }
            for &dir in folders_preorder.iter().rev() {
                let slot = index.folder_slot(dir).expect("folder");
                if matched.contains(dir.index()) || folders[slot].files > 0 {
                    shown.insert(dir.index());
                }
                if let Some(parent) = index.parent(dir) {
                    let parent_slot = index.folder_slot(parent).expect("folder");
                    let totals = folders[slot];
                    folders[parent_slot].add(totals);
                    if shown.contains(dir.index()) {
                        shown.insert(parent.index());
                    }
                }
            }
            if query.is_none() {
                shown.insert(volume.root.index());
            }
        }

        Filter {
            generation: index.generation(),
            matched,
            counted,
            shown,
            folders,
            repeated_links,
        }
    }

    /// The Index generation this Filter was computed from.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Filtered Size of a folder, or the counted size of a file.
    pub fn totals(&self, index: &Index, id: EntryId) -> Totals {
        match index.folder_slot(id) {
            Some(slot) => self.folders.get(slot).copied().unwrap_or_default(),
            None if self.counted.contains(id.index()) => Totals {
                logical: index.logical(id),
                on_disk: index.on_disk(id),
                files: 1,
            },
            None => Totals::default(),
        }
    }

    /// Whether the Entry belongs in the folder tree under this Filter.
    pub fn is_shown(&self, id: EntryId) -> bool {
        self.shown.contains(id.index())
    }

    /// Whether the Entry itself matches the Query.
    pub fn is_matched(&self, id: EntryId) -> bool {
        self.matched.contains(id.index())
    }

    /// Whether the file counts toward Filtered Sizes.
    pub fn is_counted(&self, id: EntryId) -> bool {
        self.counted.contains(id.index())
    }

    /// Whether the file is a hard link to a file already counted through another link, so it
    /// adds nothing to Filtered Sizes.
    pub fn is_repeated_link(&self, id: EntryId) -> bool {
        self.repeated_links.contains(id.index())
    }

    /// Entries that match the Query themselves, live or not; callers skip removed ones.
    pub fn matched(&self) -> &BitSet {
        &self.matched
    }
}

fn candidate<'a>(index: &'a Index, query: &Query, id: EntryId, scratch: &'a mut (String, Vec<EntryId>)) -> Candidate<'a> {
    let path = if query.needs_path() {
        index.write_match_path(id, &mut scratch.0, &mut scratch.1);
        Some(scratch.0.as_str())
    } else {
        None
    };
    Candidate {
        name: index.name(id),
        path,
        is_dir: index.is_dir(id),
        logical: index.logical(id),
        modified: index.modified(id),
        depth: if query.needs_depth() { index.depth(id) } else { 0 },
        children: if query.needs_children() && index.is_dir(id) {
            index.children(id).iter().map(|&c| index.name(c)).collect()
        } else {
            Vec::new()
        },
    }
}

fn match_all(index: &Index, query: &Query) -> BitSet {
    let n = index.capacity();
    const WORDS_PER_TASK: usize = 64;
    let mut words = vec![0u64; n.div_ceil(64)];
    words.par_chunks_mut(WORDS_PER_TASK).enumerate().for_each_init(
        || (String::new(), Vec::new()),
        |scratch, (task, chunk)| {
            let first = task * WORDS_PER_TASK * 64;
            for (w, word) in chunk.iter_mut().enumerate() {
                for bit in 0..64 {
                    let i = first + w * 64 + bit;
                    if i >= n {
                        return;
                    }
                    let id = EntryId(i as u32);
                    if index.is_live(id) && query.matches(&candidate(index, query, id, scratch)) {
                        *word |= 1 << bit;
                    }
                }
            }
        },
    );
    BitSet::from_words(words, n)
}
