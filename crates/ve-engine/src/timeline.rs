//! The Timeline (CONTEXT.md): measurements recorded once a second and kept for a fixed span,
//! looked up at a Moment. A second with no sample is a gap, never a zero.

use std::collections::VecDeque;
use std::sync::Arc;

/// The one point in time the whole window shows: Live, or a chosen time (Unix seconds).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Moment {
    #[default]
    Live,
    At(i64),
}

/// Per-second samples of the last `span` seconds, oldest first. Seconds with no sample are
/// gaps; nothing is filled in for them.
pub struct Timeline<T> {
    span: i64,
    samples: VecDeque<(i64, Arc<T>)>,
}

impl<T> Timeline<T> {
    /// A Timeline keeping the samples of the last `span` seconds.
    pub fn new(span: i64) -> Self {
        Self {
            span: span.max(1),
            samples: VecDeque::new(),
        }
    }

    /// Records the sample taken at second `time` and drops those older than the span. A time
    /// at or before the newest sample (the clock was set back) replaces everything from it on.
    pub fn push(&mut self, time: i64, sample: Arc<T>) {
        while self.samples.back().is_some_and(|&(t, _)| t >= time) {
            self.samples.pop_back();
        }
        self.samples.push_back((time, sample));
        let oldest_kept = time - self.span + 1;
        while self.samples.front().is_some_and(|&(t, _)| t < oldest_kept) {
            self.samples.pop_front();
        }
    }

    /// The sample of second `time`, or `None` for a gap or a second outside the span.
    pub fn at(&self, time: i64) -> Option<&Arc<T>> {
        let i = self.samples.binary_search_by_key(&time, |&(t, _)| t).ok()?;
        Some(&self.samples[i].1)
    }

    /// The newest sample and its second.
    pub fn latest(&self) -> Option<(i64, &Arc<T>)> {
        self.samples.back().map(|(t, s)| (*t, s))
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The samples taken from `from` to `to`, both inclusive, oldest first.
    pub fn range(&self, from: i64, to: i64) -> impl Iterator<Item = (i64, &Arc<T>)> {
        let start = self.samples.partition_point(|&(t, _)| t < from);
        self.samples
            .range(start..)
            .take_while(move |&&(t, _)| t <= to)
            .map(|(t, s)| (*t, s))
    }

    /// The gaps from `from` to `to`: each run of seconds with no sample, as inclusive
    /// (first, last) seconds, oldest first.
    pub fn gaps(&self, from: i64, to: i64) -> Vec<(i64, i64)> {
        let mut gaps = Vec::new();
        let mut next = from;
        for (t, _) in self.range(from, to) {
            if t > next {
                gaps.push((next, t - 1));
            }
            next = t + 1;
        }
        if next <= to {
            gaps.push((next, to));
        }
        gaps
    }
}
