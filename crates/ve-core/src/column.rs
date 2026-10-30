//! Compact columns for the Index: 64-bit values that nearly always fit in 32 bits (sizes,
//! times in seconds) at 4 bytes per Entry, a bitset that can count its members below a row,
//! and a column that stores only the rows whose value differs from a predictable one.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Marks a row whose value is in the side table.
const WIDE: u32 = u32::MAX;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct U64Column {
    narrow: Vec<u32>,
    /// `(row, value)` for each value of at least [`WIDE`], sorted by row.
    wide: Vec<(u32, u64)>,
}

impl U64Column {
    #[inline]
    pub fn get(&self, row: usize) -> u64 {
        match self.narrow[row] {
            WIDE => self.wide[self.wide_position(row).expect("wide value missing")].1,
            v => u64::from(v),
        }
    }

    pub fn push(&mut self, value: u64) {
        let row = u32::try_from(self.narrow.len()).expect("more than 4 billion rows");
        if value < u64::from(WIDE) {
            self.narrow.push(value as u32);
        } else {
            self.narrow.push(WIDE);
            // Rows only grow, so pushing keeps the table sorted.
            self.wide.push((row, value));
        }
    }

    pub fn set(&mut self, row: usize, value: u64) {
        let position = self.wide_position(row);
        if value < u64::from(WIDE) {
            self.narrow[row] = value as u32;
            if let Ok(i) = position {
                self.wide.remove(i);
            }
        } else {
            self.narrow[row] = WIDE;
            match position {
                Ok(i) => self.wide[i].1 = value,
                Err(i) => self.wide.insert(i, (row as u32, value)),
            }
        }
    }

    fn wide_position(&self, row: usize) -> Result<usize, usize> {
        self.wide.binary_search_by_key(&(row as u32), |&(r, _)| r)
    }

    /// The column of `rows` (old row numbers, in their new order), for saving.
    pub fn reordered<'a>(&'a self, rows: &'a [u32]) -> Reordered<'a> {
        Reordered { column: self, rows }
    }

    pub fn shrink_to_fit(&mut self) {
        self.narrow.shrink_to_fit();
        self.wide.shrink_to_fit();
    }
}

/// Rows per cumulative count in a [`Rank`]: 8 words of 64 bits.
const BLOCK: usize = 512;

/// A bitset over rows that answers "how many set rows come before this one" in constant time.
/// Rows are appended; a bit never changes once pushed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rank {
    words: Vec<u64>,
    /// Set bits in the rows before each block of [`BLOCK`] rows.
    blocks: Vec<u32>,
    len: u32,
    total: u32,
}

impl Rank {
    /// Set rows in total.
    pub fn total(&self) -> usize {
        self.total as usize
    }

    pub fn shrink_to_fit(&mut self) {
        self.words.shrink_to_fit();
        self.blocks.shrink_to_fit();
    }

    pub fn push(&mut self, set: bool) {
        let row = self.len as usize;
        if row.is_multiple_of(BLOCK) {
            self.blocks.push(self.total);
        }
        if row.is_multiple_of(64) {
            self.words.push(0);
        }
        if set {
            self.words[row / 64] |= 1 << (row % 64);
            self.total += 1;
        }
        self.len += 1;
    }

    #[inline]
    pub fn contains(&self, row: usize) -> bool {
        self.words[row / 64] & (1 << (row % 64)) != 0
    }

    /// Set rows before `row`.
    #[inline]
    pub fn rank(&self, row: usize) -> usize {
        let block = row / BLOCK;
        let word = row / 64;
        let mut count = self.blocks[block] as usize;
        for w in &self.words[block * (BLOCK / 64)..word] {
            count += w.count_ones() as usize;
        }
        count + (self.words[word] & ((1u64 << (row % 64)) - 1)).count_ones() as usize
    }
}

/// A column whose value at most rows is predictable from other columns (On-disk Size from
/// Logical Size, creation time from modification time). Only the exceptions are stored:
/// one bit per row and a value per exception. Changes after loading go to an overlay until
/// the Index is saved and loaded again, so an exception can appear or vanish without moving
/// every value after it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct SparseColumn {
    present: Rank,
    /// The exceptions, in row order.
    values: U64Column,
    /// Rows whose current value is in `changed`.
    #[serde(skip)]
    changed_bits: Vec<u64>,
    /// `None` means the row now has the predicted value.
    #[serde(skip)]
    changed: HashMap<u32, Option<u64>>,
}

impl SparseColumn {
    pub fn shrink_to_fit(&mut self) {
        self.present.shrink_to_fit();
        self.values.shrink_to_fit();
        self.changed.shrink_to_fit();
    }

    /// `None` where the row holds the predicted value.
    #[inline]
    pub fn get(&self, row: usize) -> Option<u64> {
        if let Some(word) = self.changed_bits.get(row / 64)
            && word & (1 << (row % 64)) != 0
        {
            return self.changed[&(row as u32)];
        }
        self.present.contains(row).then(|| self.values.get(self.present.rank(row)))
    }

    pub fn push(&mut self, value: Option<u64>) {
        self.present.push(value.is_some());
        if let Some(v) = value {
            self.values.push(v);
        }
    }

    pub fn set(&mut self, row: usize, value: Option<u64>) {
        if self.get(row) == value {
            return;
        }
        let present = self.present.contains(row);
        let word = row / 64;
        let bit = 1 << (row % 64);
        match value {
            // A stored exception takes its new value in place; anything else is a change to overlay.
            Some(v) if present => {
                self.values.set(self.present.rank(row), v);
                if self.changed.remove(&(row as u32)).is_some() {
                    self.changed_bits[word] &= !bit;
                }
            }
            None if !present => {
                if self.changed.remove(&(row as u32)).is_some() {
                    self.changed_bits[word] &= !bit;
                }
            }
            _ => {
                if self.changed_bits.len() <= word {
                    self.changed_bits.resize(word + 1, 0);
                }
                self.changed_bits[word] |= bit;
                self.changed.insert(row as u32, value);
            }
        }
    }

    /// The column `rows` (old row numbers, in their new order) would make if pushed in that
    /// order: compact, without the overlay.
    pub fn reordered(&self, rows: &[u32]) -> SparseColumn {
        let mut out = SparseColumn::default();
        for &r in rows {
            out.push(self.get(r as usize));
        }
        out
    }
}

/// Serialises exactly as a [`U64Column`] holding `column`'s values at `rows` would, without
/// building it.
pub(crate) struct Reordered<'a> {
    column: &'a U64Column,
    rows: &'a [u32],
}

impl Serialize for Reordered<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let narrow = Seq::new(self.rows.len(), self.rows.iter().map(|&r| self.column.narrow[r as usize]));
        let wide: Vec<(u32, u64)> = self
            .rows
            .iter()
            .enumerate()
            .filter(|&(_, &r)| self.column.narrow[r as usize] == WIDE)
            .map(|(new, &r)| (new as u32, self.column.get(r as usize)))
            .collect();
        let mut s = serializer.serialize_struct("U64Column", 2)?;
        s.serialize_field("narrow", &narrow)?;
        s.serialize_field("wide", &wide)?;
        s.end()
    }
}

/// A sequence of `len` items serialised straight from an iterator, without collecting it.
pub(crate) struct Seq<I> {
    len: usize,
    items: I,
}

impl<I> Seq<I> {
    pub fn new(len: usize, items: I) -> Self {
        Seq { len, items }
    }
}

impl<I, T> Serialize for Seq<I>
where
    I: Iterator<Item = T> + Clone,
    T: Serialize,
{
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.len))?;
        let mut count = 0;
        for item in self.items.clone() {
            seq.serialize_element(&item)?;
            count += 1;
        }
        debug_assert_eq!(count, self.len, "Seq length does not match its items");
        seq.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_counts_set_rows_across_word_and_block_edges() {
        let mut r = Rank::default();
        let set = |row: usize| row.is_multiple_of(3) || row == 511 || row == 512 || row == 1023;
        for row in 0..1500 {
            r.push(set(row));
        }
        let mut expected = 0;
        for row in 0..1500 {
            assert_eq!((r.contains(row), r.rank(row)), (set(row), expected), "row {row}");
            expected += usize::from(set(row));
        }
        assert_eq!(r.total(), expected);
    }

    #[test]
    fn a_sparse_column_keeps_exceptions_in_place_and_changes_in_the_overlay() {
        let mut c = SparseColumn::default();
        for row in 0..700 {
            c.push((row % 100 == 50).then_some(row as u64 * 10));
        }
        assert_eq!((c.get(49), c.get(50), c.get(650)), (None, Some(500), Some(6500)));
        c.set(50, Some(7)); // stored exception: changed in place
        c.set(150, None); // stored exception, now predicted: overlaid
        c.set(600, Some(u64::MAX)); // predicted, now an exception: overlaid, and wide
        c.set(601, None); // no change
        c.set(650, Some(1)); // overlaid? no: in place
        assert_eq!(
            (c.get(50), c.get(150), c.get(600), c.get(601), c.get(650)),
            (Some(7), None, Some(u64::MAX), None, Some(1))
        );
        assert_eq!(c.changed.len(), 2);
        c.set(600, None); // back to predicted: the overlay entry goes
        c.set(150, Some(1500)); // back to its stored value: in place, the overlay entry goes
        assert_eq!((c.get(600), c.get(150), c.changed.len()), (None, Some(1500), 0));
        c.set(600, Some(3));
        // A compact copy serialises as the column pushed in that order would.
        let rows: Vec<u32> = (0..700).rev().collect();
        let mut built = SparseColumn::default();
        for &r in &rows {
            built.push(c.get(r as usize));
        }
        let copy = c.reordered(&rows);
        assert_eq!(bincode::serialize(&copy).unwrap(), bincode::serialize(&built).unwrap());
        assert_eq!((copy.get(699 - 600), copy.get(699 - 50)), (Some(3), Some(7)));
    }

    #[test]
    fn values_round_trip_across_the_32_bit_boundary_and_through_set() {
        let values = [0, 1, u64::from(WIDE) - 1, u64::from(WIDE), u64::MAX, 7];
        let mut c = U64Column::default();
        for v in values {
            c.push(v);
        }
        assert_eq!((0..values.len()).map(|r| c.get(r)).collect::<Vec<_>>(), values);
        c.set(3, 5); // wide -> narrow drops the side entry
        c.set(0, u64::MAX - 1); // narrow -> wide inserts in order
        c.set(4, 9 << 40); // wide -> wide
        assert_eq!([c.get(0), c.get(3), c.get(4), c.get(5)], [u64::MAX - 1, 5, 9 << 40, 7]);
        assert_eq!(c.wide, vec![(0, u64::MAX - 1), (4, 9 << 40)]);
    }

    #[test]
    fn a_reordered_column_serialises_as_the_column_built_in_that_order() {
        let mut c = U64Column::default();
        for v in [10, u64::MAX, 30, 1 << 33] {
            c.push(v);
        }
        let rows = [3, 0, 1];
        let mut built = U64Column::default();
        for &r in &rows {
            built.push(c.get(r as usize));
        }
        assert_eq!(
            bincode::serialize(&c.reordered(&rows)).unwrap(),
            bincode::serialize(&built).unwrap()
        );
    }
}
