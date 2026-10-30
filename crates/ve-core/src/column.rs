//! Columns of 64-bit values that nearly always fit in 32 bits (sizes, creation times in
//! seconds): 4 bytes per Entry, with the rare wide value in a side table.

use serde::{Deserialize, Serialize};

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
