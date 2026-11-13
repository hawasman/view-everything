//! The Network Module (#38): each interface's bytes received and sent, read with the
//! Performance recorder's counters every second and kept in the same samples, so they share
//! its one-hour Timeline and the Moment. An interface that goes away keeps its history: the
//! samples taken while it was there still hold it. Open TCP and UDP connections (#52) are read
//! with the process table every two seconds, each with the process holding it where that can
//! be read; a sample keeps the top [`RETAINED`], the live view lists them all.
//!
//! Traffic per Process (#71, #72) comes from the same reading, without privileges: on Linux
//! the kernel counts each TCP socket's bytes and tells any user (`sock_diag`, what `ss -ti`
//! shows), and the user's own Processes are found holding them. So it is TCP only, and only
//! the user's own Processes: UDP and other users' Processes need administrator rights and are
//! not read. A sample keeps the Processes that moved bytes, the top [`RETAINED`]; the live
//! view lists every one. Agents read it through [`Net::traffic`] (a stretch) and
//! [`NetSample::traffic`] (one Moment).

mod diag;
mod linux;

pub(crate) use linux::{interface_counters, sockets};

use crate::perf::{self, Perf, ProcessCountersTable, ProcessId, Reading, Sample, delta};
use crate::timeline::{Moment, Timeline};
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
    /// TCP traffic per Process as of this second (read every two seconds): every Process
    /// holding a counted socket for Live, the kept top [`RETAINED`] that moved bytes for a
    /// past second. Unsupported where per-socket counts cannot be read, and for a run's first
    /// reading, which has nothing to count from.
    pub traffic: Reading<Arc<Traffic>>,
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
    /// What each Process moved over TCP in the stretch, the most first; `None` when none of
    /// its seconds has a traffic reading.
    pub processes: Option<Vec<ProcessTraffic>>,
}

impl Summary {
    /// The interface that moved the most bytes, either way.
    pub fn busiest(&self) -> Option<&InterfaceSummary> {
        self.interfaces.iter().filter(|i| i.moved() > 0).max_by_key(|i| i.moved())
    }

    /// The Process that moved the most bytes over TCP, either way.
    pub fn top_process(&self) -> Option<&ProcessTraffic> {
        self.processes.as_deref()?.first().filter(|p| p.moved() > 0)
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
        self.store.timeline_live(|timeline, live, live_traffic| {
            let (sample, connections, traffic) = match moment {
                Moment::Live => {
                    let sample = timeline.latest()?.1;
                    (sample, live.cloned().into(), live_traffic.cloned().into())
                }
                Moment::At(time) => {
                    let sample = timeline.at(time)?;
                    (sample, sample.connections.clone(), sample.traffic.clone())
                }
            };
            Some(NetSample {
                time: sample.time,
                interfaces: sample.network.clone(),
                connections,
                traffic,
            })
        })
    }

    /// Every interface seen from `from` to `to` (inclusive), with what it moved then, how
    /// many connections were kept then and what each Process moved over TCP.
    pub fn summary(&self, from: i64, to: i64) -> Summary {
        // The connection tables are taken from under the lock, so counting them never holds up
        // the recorder.
        let (mut summary, tables, traffic) = self.store.timeline(|timeline| {
            let mut summary = Summary {
                seconds: 0,
                interfaces: Vec::new(),
                connections: None,
                processes: None,
            };
            let mut tables: Vec<Arc<Connections>> = Vec::new();
            for (time, sample) in timeline.range(from, to) {
                // Consecutive seconds share one table until the next is read.
                if let Reading::Value(table) = &sample.connections
                    && tables.last().is_none_or(|l| !Arc::ptr_eq(l, table))
                {
                    tables.push(table.clone());
                }
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
            (summary, tables, traffic_tables(timeline, from, to))
        });
        if !tables.is_empty() {
            summary.connections = Some(u32::try_from(distinct(&tables)).unwrap_or(u32::MAX));
        }
        summary.processes = totals(&traffic);
        summary
    }

    /// What each of the user's own Processes moved over TCP from `from` to `to` (inclusive),
    /// received and sent bytes, the most first; `None` when no second then has a traffic
    /// reading (not recorded, or not readable on this platform). Each recorded second keeps
    /// the top [`RETAINED`] Processes, so a Process that was never among them is missing.
    /// For the rates at one Moment, read [`NetSample::traffic`] from [`Net::at`].
    pub fn traffic(&self, from: i64, to: i64) -> Option<Vec<ProcessTraffic>> {
        totals(&self.store.timeline(|timeline| traffic_tables(timeline, from, to)))
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

/// How many distinct connections (protocol, local and remote address) `tables` hold. A
/// connection unchanged since the table before is the same `Arc` there, at the same place in
/// the order (see [`connections`]), so only the ones new to a table are hashed.
fn distinct(tables: &[Arc<Connections>]) -> usize {
    let mut seen: HashSet<(Protocol, SocketAddr, SocketAddr)> = HashSet::new();
    let mut before: &[Arc<Connection>] = &[];
    for table in tables {
        let mut at = 0;
        for c in &table.list {
            while before.get(at).is_some_and(|b| !Arc::ptr_eq(b, c) && order(b, c).is_lt()) {
                at += 1;
            }
            if before.get(at).is_some_and(|b| Arc::ptr_eq(b, c)) {
                at += 1;
            } else {
                seen.insert(c.key());
            }
        }
        before = &table.list;
    }
    seen.len()
}

/// The order of a connection table: most worth keeping first (see [`State`]), then by
/// protocol and address.
fn order(a: &Connection, b: &Connection) -> std::cmp::Ordering {
    a.state.rank().cmp(&b.state.rank()).then_with(|| a.key().cmp(&b.key()))
}

/// Connections a sample keeps: the most active by state (see [`State`]); and the Processes a
/// sample's traffic keeps, the most bytes first. A UDP socket's bytes, and other users', are
/// not readable without administrator rights, so bytes cannot rank connections.
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
    /// The kernel's number for the socket, 0 for one without (closed, the kernel finishing it).
    pub inode: u64,
    /// A TCP socket's cumulative bytes, where the kernel's count can be read.
    pub bytes: Option<TcpBytes>,
}

/// One TCP socket's cumulative bytes as the kernel counts them since it opened: received, and
/// sent that the other end acknowledged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpBytes {
    pub received: u64,
    pub sent: u64,
}

/// What one Process moved over its TCP sockets in a stretch: bytes received and sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessTraffic {
    pub id: ProcessId,
    /// Shared with the process table.
    pub name: Arc<str>,
    pub received: u64,
    pub sent: u64,
}

impl ProcessTraffic {
    /// Bytes received plus sent.
    pub fn moved(&self) -> u64 {
        self.received + self.sent
    }
}

/// TCP traffic per Process between two readings `seconds` apart: what each of the user's own
/// Processes holding a counted TCP socket moved in that time, the most first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Traffic {
    pub seconds: u32,
    pub processes: Vec<ProcessTraffic>,
}

impl Traffic {
    /// What `process` received, in bytes per second.
    pub fn receive_rate(&self, process: &ProcessTraffic) -> u64 {
        process.received / u64::from(self.seconds.max(1))
    }

    /// What `process` sent, in bytes per second.
    pub fn send_rate(&self, process: &ProcessTraffic) -> u64 {
        process.sent / u64::from(self.seconds.max(1))
    }

    /// The traffic of Process `id`, if it holds a counted socket.
    pub fn get(&self, id: ProcessId) -> Option<&ProcessTraffic> {
        self.processes.iter().find(|p| p.id == id)
    }

    /// Bytes it holds, for the memory budget; process names are shared, so not counted.
    pub fn heap_size(&self) -> usize {
        size_of::<Self>() + self.processes.capacity() * size_of::<ProcessTraffic>()
    }
}

/// Each counted TCP socket's bytes at one reading, by inode: what the next reading counts from.
pub(crate) struct TcpCounters {
    /// The second it was read.
    pub(crate) time: i64,
    by_inode: HashMap<u64, TcpBytes>,
}

/// The TCP traffic per Process of `sockets`, read at second `now` with the process table
/// `table`, against `prev` (the previous reading's counters, `None` at a run's first): each
/// socket's bytes since `prev`, summed per Process holding it now. A socket that opened since
/// counts all its bytes; one that closed counts none, so no rate ever goes back. A socket
/// whose holder is unknown (another user's, or a Process that cannot be read) counts for no
/// one. Unsupported when no socket has a count, or at a run's first reading; returns the
/// counters for the next reading as well.
pub(crate) fn traffic(
    now: i64,
    sockets: &[Socket],
    table: Option<&ProcessCountersTable>,
    prev: Option<&TcpCounters>,
) -> (Reading<Traffic>, Option<TcpCounters>) {
    let counters = TcpCounters {
        time: now,
        by_inode: sockets
            .iter()
            .filter(|s| s.inode != 0)
            .filter_map(|s| Some((s.inode, s.bytes?)))
            .collect(),
    };
    if counters.by_inode.is_empty() {
        return (Reading::Unsupported, None);
    }
    let Some(prev) = prev.filter(|p| p.time < now) else {
        return (Reading::Unsupported, Some(counters));
    };
    let by_pid = holders(table);
    let mut processes: Vec<ProcessTraffic> = Vec::new();
    let mut at: HashMap<ProcessId, usize> = HashMap::new();
    for s in sockets {
        let (SocketHolder::Pid(pid), Some(bytes)) = (s.holder, s.bytes) else { continue };
        let Some(&(id, name)) = by_pid.get(&pid) else { continue };
        let moved = match prev.by_inode.get(&s.inode) {
            Some(before) if bytes.received >= before.received && bytes.sent >= before.sent => TcpBytes {
                received: bytes.received - before.received,
                sent: bytes.sent - before.sent,
            },
            // New since the previous reading, or a new socket under a reused inode.
            _ => bytes,
        };
        let i = *at.entry(id).or_insert_with(|| {
            processes.push(ProcessTraffic {
                id,
                name: name.clone(),
                received: 0,
                sent: 0,
            });
            processes.len() - 1
        });
        processes[i].received += moved.received;
        processes[i].sent += moved.sent;
    }
    processes.sort_by(|a, b| b.moved().cmp(&a.moved()).then(a.id.pid.cmp(&b.id.pid)));
    let seconds = u32::try_from(now - prev.time).unwrap_or(u32::MAX);
    (Reading::Value(Traffic { seconds, processes }), Some(counters))
}

/// The live table and the kept part of a new traffic reading `now`: the Processes that moved
/// bytes, the top [`RETAINED`]. Both are one `Arc` when nothing is left out.
pub(crate) fn keep_traffic(now: Traffic) -> (Arc<Traffic>, Arc<Traffic>) {
    let moving = now.processes.iter().take_while(|p| p.moved() > 0).count().min(RETAINED);
    if moving == now.processes.len() {
        let all = Arc::new(now);
        return (all.clone(), all);
    }
    let kept = Arc::new(Traffic {
        seconds: now.seconds,
        processes: now.processes[..moving].to_vec(),
    });
    (Arc::new(now), kept)
}

/// The traffic tables kept from `from` to `to` (inclusive), each once: consecutive seconds
/// share one until the next reading.
fn traffic_tables(timeline: &Timeline<Sample>, from: i64, to: i64) -> Vec<Arc<Traffic>> {
    let mut tables: Vec<Arc<Traffic>> = Vec::new();
    for (_, sample) in timeline.range(from, to) {
        if let Reading::Value(table) = &sample.traffic
            && tables.last().is_none_or(|l| !Arc::ptr_eq(l, table))
        {
            tables.push(table.clone());
        }
    }
    tables
}

/// What each Process moved over `tables`, the most first; `None` when there are none.
fn totals(tables: &[Arc<Traffic>]) -> Option<Vec<ProcessTraffic>> {
    if tables.is_empty() {
        return None;
    }
    let mut by_id: HashMap<ProcessId, ProcessTraffic> = HashMap::new();
    for p in tables.iter().flat_map(|t| &t.processes) {
        let total = by_id.entry(p.id).or_insert_with(|| ProcessTraffic {
            received: 0,
            sent: 0,
            ..p.clone()
        });
        total.received += p.received;
        total.sent += p.sent;
    }
    let mut totals: Vec<ProcessTraffic> = by_id.into_values().collect();
    totals.sort_by(|a, b| b.moved().cmp(&a.moved()).then(a.id.pid.cmp(&b.id.pid)));
    Some(totals)
}

/// Each process of `table` by PID, with its name.
fn holders(table: Option<&ProcessCountersTable>) -> HashMap<u32, (ProcessId, &Arc<str>)> {
    table
        .map(|t| t.processes.iter().map(|p| (p.id.pid, (p.id, &p.name))).collect())
        .unwrap_or_default()
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
    let by_pid = holders(table);
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
    list.sort_by(|a, b| order(a, b));
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
