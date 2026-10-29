//! Which Entries are selected in an Explorer list: click, Ctrl-click and Shift-click.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashSet;
use ve_core::EntryId;

/// The highlight of a selected row. It is a child of the row, so the Table's own faint
/// selected-row background (it paints the last plainly clicked row, whether or not that row is
/// still in the selection) never hides it; the last-clicked row also gets a ring.
pub fn highlight(last: bool, cx: &App) -> Div {
    let p = crate::theme::palette(cx);
    div()
        .absolute()
        .inset_0()
        .rounded(px(8.))
        .bg(p.active)
        .when(last, |d| d.border_1().border_color(p.control))
}

#[derive(Default)]
pub struct Selection {
    ids: HashSet<EntryId>,
    /// The Entry a Shift-click extends from; its row is looked up per click, as rows move when
    /// the tree expands or the list re-sorts.
    anchor: Option<EntryId>,
    /// The Entry last clicked, which the Selection panel shows.
    last: Option<EntryId>,
    /// The Entry last right-clicked, which the context menu acts on.
    pub context: Option<EntryId>,
}

impl Selection {
    pub fn contains(&self, id: EntryId) -> bool {
        self.ids.contains(&id)
    }

    pub fn ids(&self) -> Vec<EntryId> {
        self.ids.iter().copied().collect()
    }

    pub fn last(&self) -> Option<EntryId> {
        self.last.filter(|id| self.ids.contains(id))
    }

    pub fn clear(&mut self) {
        self.ids.clear();
        self.anchor = None;
        self.last = None;
    }

    /// Selects every row `0..rows`; the anchor and last-clicked Entry stay where they were.
    pub fn select_all(&mut self, rows: usize, entry_at: impl Fn(usize) -> Option<EntryId>) {
        self.ids = (0..rows).filter_map(entry_at).collect();
    }

    /// Keeps only the Entries `keep` accepts, e.g. those a new View still shows.
    pub fn retain(&mut self, keep: impl Fn(EntryId) -> bool) {
        self.ids.retain(|&id| keep(id));
    }

    pub fn only(&mut self, id: EntryId) {
        self.ids.clear();
        self.ids.insert(id);
        self.anchor = Some(id);
        self.last = Some(id);
    }

    /// Moves the focus one row up (`down` false) or down, as the arrow keys do: the new row
    /// alone is selected, or with `extend` the range from the anchor. The focus is the
    /// last-clicked Entry (the anchor if none), re-resolved to its current row; with neither,
    /// Down starts at the first row and Up at the last. Rows without an Entry are skipped.
    /// Returns the row now focused, None if there is nowhere to go.
    pub fn step(&mut self, down: bool, extend: bool, rows: usize, entry_at: impl Fn(usize) -> Option<EntryId>) -> Option<usize> {
        let find = |id: EntryId| (0..rows).find(|&r| entry_at(r) == Some(id));
        let from = self.last.or(self.anchor).and_then(find);
        let target = match (from, down) {
            (Some(r), true) => (r + 1..rows).find(|&r| entry_at(r).is_some()),
            (Some(r), false) => (0..r).rev().find(|&r| entry_at(r).is_some()),
            (None, true) => (0..rows).find(|&r| entry_at(r).is_some()),
            (None, false) => (0..rows).rev().find(|&r| entry_at(r).is_some()),
        }
        // At either end the focus stays where it is.
        .or(from)?;
        let modifiers = Modifiers {
            shift: extend,
            ..Modifiers::default()
        };
        self.click(target, modifiers, rows, entry_at);
        Some(target)
    }

    /// Applies a click on `row` of a list with `rows` rows. `entry_at` maps a row to its Entry.
    /// Plain: only that row. Ctrl/Cmd: toggles it. Shift: the range from the anchor replaces the
    /// selection, or joins it with Ctrl/Cmd held too.
    pub fn click(&mut self, row: usize, modifiers: Modifiers, rows: usize, entry_at: impl Fn(usize) -> Option<EntryId>) {
        let Some(id) = entry_at(row) else { return };
        let additive = modifiers.control || modifiers.platform;
        if modifiers.shift {
            let from = self.anchor.and_then(|a| (0..rows).find(|&r| entry_at(r) == Some(a))).unwrap_or(row);
            let range = (from.min(row)..=from.max(row)).filter_map(entry_at);
            if additive {
                self.ids.extend(range);
            } else {
                self.ids = range.collect();
            }
            self.anchor.get_or_insert(id);
            self.last = Some(id);
        } else if additive {
            if !self.ids.remove(&id) {
                self.ids.insert(id);
            }
            self.anchor = Some(id);
            self.last = Some(id);
        } else {
            self.only(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(row: usize) -> Option<EntryId> {
        Some(EntryId(row as u32 + 100))
    }

    const ROWS: usize = 10;

    fn click(s: &mut Selection, row: usize, m: Modifiers) {
        s.click(row, m, ROWS, entry);
    }

    fn sorted(s: &Selection) -> Vec<u32> {
        let mut v: Vec<u32> = s.ids().into_iter().map(|e| e.0).collect();
        v.sort();
        v
    }

    #[::core::prelude::v1::test]
    fn click_ctrl_and_shift() {
        let mut s = Selection::default();
        click(&mut s, 2, Modifiers::default());
        click(&mut s, 5, Modifiers::control());
        assert_eq!(sorted(&s), vec![102, 105]);
        click(&mut s, 2, Modifiers::control());
        assert_eq!(sorted(&s), vec![105]);
        // Shift extends from the last Ctrl-clicked row (2) to 7.
        click(&mut s, 7, Modifiers::shift());
        assert_eq!(sorted(&s), vec![102, 103, 104, 105, 106, 107]);
        click(&mut s, 0, Modifiers::default());
        assert_eq!(sorted(&s), vec![100]);
    }

    #[::core::prelude::v1::test]
    fn double_click_a_click_b_ctrl_click_c() {
        let mut s = Selection::default();
        // A double-click arrives as two plain clicks on the same row.
        click(&mut s, 1, Modifiers::default());
        click(&mut s, 1, Modifiers::default());
        assert_eq!(sorted(&s), vec![101]);
        click(&mut s, 4, Modifiers::default());
        assert_eq!(sorted(&s), vec![104]);
        click(&mut s, 6, Modifiers::control());
        assert_eq!(sorted(&s), vec![104, 106]);
        assert_eq!(s.last(), Some(EntryId(106)));
    }

    #[::core::prelude::v1::test]
    fn click_a_then_b_leaves_only_b() {
        let mut s = Selection::default();
        click(&mut s, 3, Modifiers::default());
        click(&mut s, 5, Modifiers::default());
        assert_eq!(sorted(&s), vec![105]);
        assert_eq!(s.last(), Some(EntryId(105)));
    }

    #[::core::prelude::v1::test]
    fn ctrl_shift_adds_the_range() {
        let mut s = Selection::default();
        click(&mut s, 1, Modifiers::default());
        click(&mut s, 8, Modifiers::control());
        let both = Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        };
        click(&mut s, 5, both);
        assert_eq!(sorted(&s), vec![101, 105, 106, 107, 108]);
    }

    #[::core::prelude::v1::test]
    fn anchor_follows_its_entry_when_rows_move() {
        let mut s = Selection::default();
        click(&mut s, 2, Modifiers::default());
        // Two rows were inserted above: Entry 102 is now row 4.
        let moved = |r: usize| if r < 2 { entry(r + 50) } else { entry(r - 2) };
        s.click(6, Modifiers::shift(), ROWS, moved);
        assert_eq!(sorted(&s), vec![102, 103, 104]);
    }

    #[::core::prelude::v1::test]
    fn survives_retain_and_drops_vanished() {
        let mut s = Selection::default();
        click(&mut s, 1, Modifiers::default());
        click(&mut s, 2, Modifiers::control());
        s.retain(|id| id != EntryId(101));
        assert_eq!(sorted(&s), vec![102]);
        assert_eq!(s.last(), Some(EntryId(102)));
    }

    #[::core::prelude::v1::test]
    fn arrows_move_from_the_last_entry_not_a_stale_row() {
        let mut s = Selection::default();
        click(&mut s, 3, Modifiers::default());
        click(&mut s, 6, Modifiers::control());
        // Rows shifted by two: Entry 106 is row 8 now; Down goes to row 9 alone.
        let shifted = |r: usize| if r < 2 { entry(r + 50) } else { entry(r - 2) };
        assert_eq!(s.step(true, false, ROWS, shifted), Some(9));
        assert_eq!(sorted(&s), vec![107]);
        assert_eq!(s.last(), Some(EntryId(107)));
        assert_eq!(s.step(false, false, ROWS, shifted), Some(8));
        assert_eq!(sorted(&s), vec![106]);
    }

    #[::core::prelude::v1::test]
    fn arrows_start_at_the_ends_and_stop_there() {
        let mut s = Selection::default();
        assert_eq!(s.step(true, false, ROWS, entry), Some(0));
        s.clear();
        assert_eq!(s.step(false, false, ROWS, entry), Some(9));
        assert_eq!(s.step(true, false, ROWS, entry), Some(9));
        assert_eq!(sorted(&s), vec![109]);
        assert_eq!(s.step(true, false, 0, |_| None), None);
    }

    #[::core::prelude::v1::test]
    fn shift_arrow_extends_from_the_anchor() {
        let mut s = Selection::default();
        click(&mut s, 4, Modifiers::default());
        s.step(true, true, ROWS, entry);
        s.step(true, true, ROWS, entry);
        assert_eq!(sorted(&s), vec![104, 105, 106]);
        s.step(false, true, ROWS, entry);
        s.step(false, true, ROWS, entry);
        s.step(false, true, ROWS, entry);
        assert_eq!(sorted(&s), vec![103, 104]);
        s.step(true, false, ROWS, entry);
        assert_eq!(sorted(&s), vec![104]);
    }

    #[::core::prelude::v1::test]
    fn select_all_and_last() {
        let mut s = Selection::default();
        click(&mut s, 3, Modifiers::default());
        assert_eq!(s.last(), Some(EntryId(103)));
        s.select_all(5, entry);
        assert_eq!(s.ids().len(), 5);
        assert_eq!(s.last(), Some(EntryId(103)));
        // Shift from the anchor kept by select_all.
        click(&mut s, 1, Modifiers::shift());
        assert_eq!(sorted(&s), vec![101, 102, 103]);
        assert_eq!(s.last(), Some(EntryId(101)));
        // Ctrl-click off the last-clicked row leaves no last Entry.
        click(&mut s, 1, Modifiers::control());
        assert_eq!(s.last(), None);
        assert_eq!(s.ids().len(), 2);
        s.clear();
        assert!(s.ids().is_empty());
    }
}
