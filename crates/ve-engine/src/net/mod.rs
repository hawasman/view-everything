//! The Network Module's interfaces (#38): each interface's bytes received and sent, read with
//! the Performance recorder's counters every second and kept in the same samples, so they share
//! its one-hour Timeline and the Moment. An interface that goes away keeps its history: the
//! samples taken while it was there still hold it. Per-app traffic needs administrator rights
//! and is not read.

mod linux;

pub(crate) use linux::interface_counters;

use crate::perf::{self, Perf, Reading, Sample, delta};
use crate::timeline::Moment;
use std::sync::Arc;

/// One interface's cumulative byte counts as a backend reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceCounters {
    /// The same `Arc` in every reading, so samples share one copy of each name.
    pub name: Arc<str>,
    pub received: u64,
    pub sent: u64,
}

/// One interface in a sample: the bytes it moved since the previous reading, `seconds` before.
#[derive(Clone, Debug, PartialEq)]
pub struct Interface {
    pub name: Arc<str>,
    /// Unknown for an interface that just appeared, or whose counter was reset.
    pub received: Reading<u64>,
    pub sent: Reading<u64>,
    pub seconds: u32,
}

impl Interface {
    /// Bytes received per second.
    pub fn receive_rate(&self) -> Reading<u64> {
        self.per_second(&self.received)
    }

    /// Bytes sent per second.
    pub fn send_rate(&self) -> Reading<u64> {
        self.per_second(&self.sent)
    }

    fn per_second(&self, bytes: &Reading<u64>) -> Reading<u64> {
        bytes.value().map(|b| b / u64::from(self.seconds.max(1))).into()
    }
}

/// The interfaces of a sample from the readings `prev` and `now`, `seconds` apart. An
/// interface only `prev` had is gone: it is not in this sample.
pub(crate) fn interfaces(seconds: i64, prev: &[InterfaceCounters], now: &[InterfaceCounters]) -> Vec<Interface> {
    now.iter()
        .map(|n| {
            let before = prev.iter().find(|p| p.name == n.name);
            Interface {
                name: n.name.clone(),
                received: before.and_then(|p| delta(p.received, n.received)).into(),
                sent: before.and_then(|p| delta(p.sent, n.sent)).into(),
                seconds: u32::try_from(seconds.max(1)).unwrap_or(u32::MAX),
            }
        })
        .collect()
}

/// The network at one second.
#[derive(Clone, Debug, PartialEq)]
pub struct NetSample {
    /// Unix seconds.
    pub time: i64,
    pub interfaces: Reading<Vec<Interface>>,
}

/// One interface over a stretch of the Timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct InterfaceSummary {
    pub name: Arc<str>,
    /// Bytes moved in the seconds that were recorded with a known count (not an interface's
    /// first second, nor one whose counter was reset); `None` while no second had one.
    pub received: Option<u64>,
    pub sent: Option<u64>,
    /// The last second it was there.
    pub last_seen: i64,
}

/// Interfaces over a stretch of the Timeline, including those that went away during it.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// Seconds in the stretch with a network reading.
    pub seconds: usize,
    /// By name.
    pub interfaces: Vec<InterfaceSummary>,
}

impl Summary {
    /// The interface that moved the most bytes, either way.
    pub fn busiest(&self) -> Option<&InterfaceSummary> {
        self.interfaces.iter().filter(|i| i.moved() > 0).max_by_key(|i| i.moved())
    }
}

impl InterfaceSummary {
    /// Bytes received plus sent, counting an unknown total as none: for ranking, not showing.
    pub fn moved(&self) -> u64 {
        self.received.unwrap_or(0) + self.sent.unwrap_or(0)
    }
}

/// The engine's handle on Network samples (`net.live`, `net.at`, `net.summary`): a view of the
/// Performance recorder's Timeline, which records them.
pub struct Net {
    store: Arc<perf::Store>,
}

impl Net {
    /// The Network view of `perf`'s samples.
    pub fn new(perf: &Perf) -> Net {
        Net { store: perf.store() }
    }

    /// The newest second's network, if any second of the last hour was recorded.
    pub fn live(&self) -> Option<NetSample> {
        self.at(Moment::Live)
    }

    /// The network shown at `moment`: the newest for Live, that second's for a time. `None`
    /// is a gap.
    pub fn at(&self, moment: Moment) -> Option<NetSample> {
        self.store.timeline(|timeline| {
            let sample = match moment {
                Moment::Live => timeline.latest()?.1,
                Moment::At(time) => timeline.at(time)?,
            };
            Some(NetSample {
                time: sample.time,
                interfaces: sample.network.clone(),
            })
        })
    }

    /// Every interface seen from `from` to `to` (inclusive), with what it moved then.
    pub fn summary(&self, from: i64, to: i64) -> Summary {
        self.store.timeline(|timeline| {
            let mut summary = Summary {
                seconds: 0,
                interfaces: Vec::new(),
            };
            for (time, sample) in timeline.range(from, to) {
                let Reading::Value(interfaces) = &sample.network else { continue };
                summary.seconds += 1;
                for i in interfaces {
                    let at = match summary.interfaces.binary_search_by(|s| s.name.cmp(&i.name)) {
                        Ok(at) => at,
                        Err(at) => {
                            summary.interfaces.insert(
                                at,
                                InterfaceSummary {
                                    name: i.name.clone(),
                                    received: None,
                                    sent: None,
                                    last_seen: time,
                                },
                            );
                            at
                        }
                    };
                    let s = &mut summary.interfaces[at];
                    add(&mut s.received, i.received.value());
                    add(&mut s.sent, i.sent.value());
                    s.last_seen = time;
                }
            }
            summary
        })
    }

    /// Whether any recorded second has a network reading: the rail shows Network only then.
    pub fn has_samples(&self) -> bool {
        self.store
            .timeline(|t| t.latest().is_some_and(|(_, s)| matches!(s.network, Reading::Value(_))))
    }

    /// `from` to `to` (inclusive) cut into `buckets` equal slices, each with the highest total
    /// rate (received plus sent, bytes per second) of its samples, or `None` where no sample
    /// has one: a gap.
    pub fn peaks(&self, from: i64, to: i64, buckets: usize) -> Vec<Option<u64>> {
        self.store.peaks(from, to, buckets, total_rate)
    }

    /// How many seconds from `from` to `to` (inclusive) have a network reading.
    pub fn count(&self, from: i64, to: i64) -> usize {
        self.store
            .timeline(|t| t.range(from, to).filter(|(_, s)| matches!(s.network, Reading::Value(_))).count())
    }
}

/// Received plus sent over every interface, in bytes per second; unknown if none is known.
fn total_rate(sample: &Sample) -> Option<u64> {
    let interfaces = sample.network.value()?;
    interfaces
        .iter()
        .flat_map(|i| [i.receive_rate(), i.send_rate()])
        .filter_map(|r| r.value().copied())
        .reduce(|a, b| a + b)
}

/// Adds a known count to a total, which becomes known with it.
fn add(total: &mut Option<u64>, count: Option<&u64>) {
    if let Some(&count) = count {
        *total = Some(total.unwrap_or(0) + count);
    }
}
