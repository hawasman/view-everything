//! Which Entries are selected in an Explorer list: click, Ctrl-click and Shift-click.

use gpui_kit::*;
use std::collections::HashSet;
use ve_core::EntryId;

#[derive(Default)]
pub struct Selection {
    ids: HashSet<EntryId>,
    /// The row a Shift-click extends from.
    anchor: Option<usize>,
}

impl Selection {
    pub fn contains(&self, id: EntryId) -> bool {
        self.ids.contains(&id)
    }

    pub fn ids(&self) -> Vec<EntryId> {
        self.ids.iter().copied().collect()
    }

    pub fn clear(&mut self) {
        self.ids.clear();
        self.anchor = None;
    }

    /// Keeps only the Entries `keep` accepts, e.g. those a new View still shows.
    pub fn retain(&mut self, keep: impl Fn(EntryId) -> bool) {
        self.ids.retain(|&id| keep(id));
    }

    pub fn only(&mut self, row: usize, id: EntryId) {
        self.ids.clear();
        self.ids.insert(id);
        self.anchor = Some(row);
    }

    /// Applies a click on `row`. `entry_at` maps a row to its Entry.
    pub fn click(
        &mut self,
        row: usize,
        modifiers: Modifiers,
        entry_at: impl Fn(usize) -> Option<EntryId>,
    ) {
        let Some(id) = entry_at(row) else { return };
        if modifiers.shift {
            let from = self.anchor.unwrap_or(row);
            self.ids = (from.min(row)..=from.max(row)).filter_map(entry_at).collect();
        } else if modifiers.control || modifiers.platform {
            if !self.ids.remove(&id) {
                self.ids.insert(id);
            }
            self.anchor = Some(row);
        } else {
            self.only(row, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(row: usize) -> Option<EntryId> {
        Some(EntryId(row as u32 + 100))
    }

    #[::core::prelude::v1::test]
    fn click_ctrl_and_shift() {
        let mut s = Selection::default();
        s.click(2, Modifiers::default(), entry);
        s.click(5, Modifiers::control(), entry);
        assert_eq!(s.ids().len(), 2);
        s.click(2, Modifiers::control(), entry);
        assert_eq!(s.ids(), vec![EntryId(105)]);
        // Shift extends from the last clicked row (2) to 7.
        s.click(7, Modifiers::shift(), entry);
        assert_eq!(s.ids().len(), 6);
        s.click(0, Modifiers::default(), entry);
        assert_eq!(s.ids(), vec![EntryId(100)]);
    }
}
