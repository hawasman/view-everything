/// A fixed-size set of small integers, one bit per member.
#[derive(Clone, Debug, Default)]
pub struct BitSet {
    words: Vec<u64>,
    len: usize,
}

impl BitSet {
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    pub fn full(len: usize) -> Self {
        let mut set = Self {
            words: vec![u64::MAX; len.div_ceil(64)],
            len,
        };
        if !len.is_multiple_of(64)
            && let Some(last) = set.words.last_mut()
        {
            *last = (1u64 << (len % 64)) - 1;
        }
        set
    }

    pub(crate) fn from_words(words: Vec<u64>, len: usize) -> Self {
        debug_assert_eq!(words.len(), len.div_ceil(64));
        Self { words, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn contains(&self, i: usize) -> bool {
        i < self.len && self.words[i / 64] & (1 << (i % 64)) != 0
    }

    #[inline]
    pub fn insert(&mut self, i: usize) {
        self.words[i / 64] |= 1 << (i % 64);
    }

    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, &word)| {
            let mut w = word;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let bit = w.trailing_zeros() as usize;
                w &= w - 1;
                Some(wi * 64 + bit)
            })
        })
    }
}
