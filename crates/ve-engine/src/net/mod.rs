//! The Network Module (#38): each interface's bytes received and sent, read with the
//! Performance recorder's counters every second and kept in the same samples, so they share
//! its one-hour Timeline and the Moment. An interface that goes away keeps its history: the
//! samples taken while it was there still hold it. Open TCP and UDP connections (#52) are read
//! with the process table every two seconds, each with the process holding it where that can
//! be read; a sample keeps the top [`RETAINED`], the live view lists them all. Per-app traffic
//! needs administrator rights and is not read.

mod linux;

pub(crate) use linux::{interface_counters, sockets};

use crate::perf::{self, Perf, ProcessCountersTable, ProcessId, Reading, Sample, delta};
use crate::timeline::Moment;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
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
    /// The open connections as of this second (read every two seconds): every one for Live,
    /// the kept top [`RETAINED`] for a past second.
    pub connections: Reading<Arc<Connections>>,
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
    /// How many distinct connections (protocol, local and remote address) the kept samples of
    /// the stretch hold; `None` when none of its seconds has a connection reading.
    pub connections: Option<u32>,
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
        self.store.timeline_live(|timeline, live| {
            let (sample, connections) = match moment {
                Moment::Live => {
                    let sample = timeline.latest()?.1;
                    (sample, live.cloned().into())
                }
                Moment::At(time) => {
                    let sample = timeline.at(time)?;
                    (sample, sample.connections.clone())
                }
            };
            Some(NetSample {
                time: sample.time,
                interfaces: sample.network.clone(),
                connections,
            })
        })
    }

    /// Every interface seen from `from` to `to` (inclusive), with what it moved then, and how
    /// many connections were kept then.
    pub fn summary(&self, from: i64, to: i64) -> Summary {
        self.store.timeline(|timeline| {
            let mut summary = Summary {
                seconds: 0,
                interfaces: Vec::new(),
                connections: None,
            };
            let mut seen: HashSet<(Protocol, SocketAddr, SocketAddr)> = HashSet::new();
            let mut last: Option<&Arc<Connections>> = None;
            for (_, sample) in timeline.range(from, to) {
                // Consecutive seconds share one table until the next is read.
                if let Reading::Value(table) = &sample.connections
                    && last.is_none_or(|l| !Arc::ptr_eq(l, table))
                {
                    seen.extend(table.list.iter().map(|c| c.key()));
                    last = Some(table);
                }
            }
            if last.is_some() {
                summary.connections = Some(u32::try_from(seen.len()).unwrap_or(u32::MAX));
            }
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

/// Connections a sample keeps: the most active by state (see [`State`]). The bytes each
/// connection moves are not readable without administrator rights, so they cannot rank them.
pub const RETAINED: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Tcp => "TCP",
            Protocol::Udp => "UDP",
        }
    }
}

/// A socket's state, as the kernel numbers it in `/proc/net/tcp`. UDP has no handshake: a
/// UDP socket is [`State::Established`] once it has a remote end, [`State::Close`] otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    Established,
    SynSent,
    SynReceived,
    FinWait1,
    FinWait2,
    TimeWait,
    Close,
    CloseWait,
    LastAck,
    Listen,
    Closing,
    NewSynReceived,
    /// A number this reader does not know.
    Other(u8),
}

impl State {
    /// The state the kernel numbers `code`.
    pub fn from_code(code: u8) -> State {
        match code {
            0x01 => State::Established,
            0x02 => State::SynSent,
            0x03 => State::SynReceived,
            0x04 => State::FinWait1,
            0x05 => State::FinWait2,
            0x06 => State::TimeWait,
            0x07 => State::Close,
            0x08 => State::CloseWait,
            0x09 => State::LastAck,
            0x0A => State::Listen,
            0x0B => State::Closing,
            0x0C => State::NewSynReceived,
            other => State::Other(other),
        }
    }

    /// What the state means for a socket of `protocol`, in a few words.
    pub fn label(self, protocol: Protocol) -> &'static str {
        match (protocol, self) {
            (Protocol::Udp, State::Established) => "connected",
            (Protocol::Udp, State::Close) => "open",
            (_, State::Established) => "established",
            (_, State::SynSent) => "connecting",
            (_, State::SynReceived | State::NewSynReceived) => "accepting",
            (_, State::Listen) => "listening",
            (_, State::FinWait1 | State::FinWait2 | State::Closing | State::LastAck | State::CloseWait) => "closing",
            (_, State::TimeWait) => "time wait",
            (_, State::Close) => "closed",
            (_, State::Other(_)) => "unknown state",
        }
    }

    /// How much a connection in this state is worth keeping, lowest first: open
    /// conversations, then ones starting, then ones ending, then sockets waiting for one.
    fn rank(self) -> u8 {
        match self {
            State::Established => 0,
            State::SynSent | State::SynReceived | State::NewSynReceived => 1,
            State::FinWait1 | State::FinWait2 | State::Closing | State::LastAck | State::CloseWait => 2,
            State::Listen | State::Close => 3,
            State::TimeWait => 4,
            State::Other(_) => 5,
        }
    }
}

/// Which process holds a socket, as a backend finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketHolder {
    Pid(u32),
    /// No process: the kernel is finishing a closed connection (a socket without an inode).
    Nobody,
    /// A process that cannot be read (another user's) holds it, or it could not be found.
    Unknown,
}

/// One socket as a backend reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct Socket {
    pub protocol: Protocol,
    pub local: SocketAddr,
    /// Unspecified (`0.0.0.0:0`, `[::]:0`) while it has no remote end.
    pub remote: SocketAddr,
    pub state: State,
    pub holder: SocketHolder,
}

/// The process holding a connection (not an Owner: that is what a folder belongs to).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holder {
    Process {
        id: ProcessId,
        /// Shared with the process table.
        name: Arc<str>,
    },
    /// No process: the kernel is finishing a closed connection.
    Nobody,
    /// A process that cannot be read holds it: never "nobody".
    Unknown,
}

/// One open connection or listening socket.
#[derive(Clone, Debug, PartialEq)]
pub struct Connection {
    pub protocol: Protocol,
    pub local: SocketAddr,
    /// Unspecified (`0.0.0.0:0`, `[::]:0`) while it has no remote end.
    pub remote: SocketAddr,
    pub state: State,
    pub holder: Holder,
}

impl Connection {
    /// Whether it has a remote end.
    pub fn has_remote(&self) -> bool {
        !self.remote.ip().is_unspecified() || self.remote.port() != 0
    }

    /// What tells it apart from every other connection open at the same time.
    fn key(&self) -> (Protocol, SocketAddr, SocketAddr) {
        (self.protocol, self.local, self.remote)
    }
}

/// The connections open at one reading, most worth keeping first (see [`State`]), then by
/// protocol and address. A connection still open at the next reading is the same `Arc` there,
/// so the hour's tables share one copy of each.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Connections {
    pub list: Vec<Arc<Connection>>,
}

impl Connections {
    /// Bytes the table holds, for the memory budget, counting only the connections not in
    /// `counted` (which collects them); process names are shared, so not counted.
    pub fn heap_size(&self, counted: &mut HashSet<*const Connection>) -> usize {
        let own = self.list.iter().filter(|c| counted.insert(Arc::as_ptr(c))).count();
        size_of::<Self>() + self.list.capacity() * size_of::<Arc<Connection>>() + own * (size_of::<Connection>() + 2 * size_of::<usize>())
    }
}

/// The connections of `sockets`, each with its holder found in the process table `table` read
/// with it; a holder missing from the table (it started or ended between the two) is unknown.
/// A connection unchanged since `prev` (the previous reading) is its `Arc` there.
pub(crate) fn connections(sockets: Vec<Socket>, table: Option<&ProcessCountersTable>, prev: Option<&Connections>) -> Connections {
    let by_pid: HashMap<u32, (ProcessId, &Arc<str>)> = table
        .map(|t| t.processes.iter().map(|p| (p.id.pid, (p.id, &p.name))).collect())
        .unwrap_or_default();
    let before: HashMap<_, &Arc<Connection>> = prev.map_or_else(HashMap::new, |p| p.list.iter().map(|c| (c.key(), c)).collect());
    let mut list: Vec<Arc<Connection>> = sockets
        .into_iter()
        .map(|s| {
            let connection = Connection {
                protocol: s.protocol,
                local: s.local,
                remote: s.remote,
                state: s.state,
                holder: match s.holder {
                    SocketHolder::Pid(pid) => by_pid
                        .get(&pid)
                        .map_or(Holder::Unknown, |&(id, name)| Holder::Process { id, name: name.clone() }),
                    SocketHolder::Nobody => Holder::Nobody,
                    SocketHolder::Unknown => Holder::Unknown,
                },
            };
            match before.get(&connection.key()) {
                Some(&same) if **same == connection => same.clone(),
                _ => Arc::new(connection),
            }
        })
        .collect();
    list.sort_by(|a, b| a.state.rank().cmp(&b.state.rank()).then_with(|| a.key().cmp(&b.key())));
    Connections { list }
}

/// The live table and the kept top [`RETAINED`] of a new reading `now`, reusing the previous
/// tables (`live`, `kept`) where nothing changed, so unchanged seconds share one copy.
pub(crate) fn keep(
    now: Connections,
    live: Option<&Arc<Connections>>,
    kept: Option<&Arc<Connections>>,
) -> (Arc<Connections>, Arc<Connections>) {
    let same = |a: &[Arc<Connection>], b: &[Arc<Connection>]| a.len() == b.len() && a.iter().zip(b).all(|(a, b)| Arc::ptr_eq(a, b));
    let reuse = |prev: Option<&Arc<Connections>>, list: &[Arc<Connection>]| prev.filter(|p| same(&p.list, list)).cloned();
    let retained = &now.list[..now.list.len().min(RETAINED)];
    let kept = reuse(kept, retained).unwrap_or_else(|| Arc::new(Connections { list: retained.to_vec() }));
    let live = reuse(live, &now.list).unwrap_or_else(|| {
        if now.list.len() == kept.list.len() {
            kept.clone()
        } else {
            Arc::new(now)
        }
    });
    (live, kept)
}
