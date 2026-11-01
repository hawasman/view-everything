//! The Performance recorder (#37): once a second it reads the system's counters through a
//! [`Backend`], turns two readings into a [`Sample`] and keeps the last hour of them on a
//! [`Timeline`]. Every two seconds it also reads the process table (#50). Readings a backend
//! cannot take are [`Reading::Unsupported`], never zero. Recording runs only while a client
//! asks for it (the window is open, or keep-in-tray is on); the seconds it did not run are
//! gaps.

mod linux;
mod process;

pub use linux::LinuxBackend;
use process::PrevProcesses;
pub use process::{Details, Io, Process, ProcessCounters, ProcessCountersTable, ProcessId, Processes, RETAINED};

use crate::Repaint;
use crate::net::{Interface, InterfaceCounters};
use crate::timeline::{Moment, Timeline};
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long samples are kept: one hour of seconds.
pub const SPAN: i64 = 3600;

/// The longest step between two readings still turned into rates. A longer one means the
/// recorder stalled or was paused; that stretch is a gap, not an average over it.
const MAX_STEP: i64 = 3;

/// Seconds between two readings of the process table.
const PROCESS_STEP: i64 = 2;

/// The bytes one sector of `/proc/diskstats` stands for, whatever the device's own sector size.
const SECTOR: u64 = 512;

/// A reading a backend can take, or `Unsupported` where it cannot (no reader on this
/// platform, a missing file): distinct from zero.
#[derive(Clone, Debug, PartialEq)]
pub enum Reading<T> {
    Value(T),
    Unsupported,
}

impl<T> Reading<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Reading::Value(v) => Some(v),
            Reading::Unsupported => None,
        }
    }
}

impl<T> From<Option<T>> for Reading<T> {
    fn from(value: Option<T>) -> Self {
        value.map_or(Reading::Unsupported, Reading::Value)
    }
}

/// Which kind of core a logical CPU is, where the kernel says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreKind {
    Performance,
    Efficiency,
    /// The kernel does not tell the kinds apart (all cores alike, or not reported).
    Unknown,
}

/// Cumulative CPU time in clock ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// The CPU counters of one reading: the whole machine and each online logical CPU by id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuCounters {
    pub total: CpuTimes,
    /// Sorted by id, as `/proc/stat` lists them.
    pub cores: Vec<(u32, CpuTimes)>,
}

/// One block device's cumulative sector counts.
#[derive(Clone, Debug, PartialEq)]
pub struct DiskCounters {
    /// The same `Arc` in every reading, so samples share one copy of each name.
    pub name: Arc<str>,
    pub sectors_read: u64,
    pub sectors_written: u64,
}

/// Memory composition, in bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    pub total: u64,
    /// Taken by programs: neither free nor cache.
    pub used: u64,
    /// Page cache and buffers the kernel can give back.
    pub cache: u64,
    pub free: u64,
    /// What programs could still get without swapping, as the kernel estimates it.
    pub available: Reading<u64>,
    pub swap: Reading<Swap>,
}

/// Swap space, in bytes; `total` 0 is a system without swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Swap {
    pub total: u64,
    pub used: u64,
}

/// What a backend reads at one instant: cumulative counters plus instantaneous memory.
#[derive(Clone, Debug, PartialEq)]
pub struct Counters {
    pub cpu: Reading<CpuCounters>,
    /// The kind of each logical CPU, sorted by id; unlisted ids are [`CoreKind::Unknown`].
    pub core_kinds: Arc<[(u32, CoreKind)]>,
    pub memory: Reading<Memory>,
    pub disks: Reading<Vec<DiskCounters>>,
    /// The network interfaces' byte counts (the Network Module's, recorded here).
    pub network: Reading<Vec<InterfaceCounters>>,
}

/// Reads a platform's counters. A Windows backend is the next implementation.
pub trait Backend: Send + 'static {
    fn read(&mut self) -> Counters;

    /// Every running process; `io` asks for each one's I/O counters as well, which costs a
    /// read per process.
    fn processes(&mut self, io: bool) -> Reading<ProcessCountersTable> {
        let _ = io;
        Reading::Unsupported
    }

    /// The command line and user of the running process `id`; `None` once it has exited
    /// (its PID gone, or now another process's).
    fn details(&mut self, id: ProcessId) -> Option<Details> {
        let _ = id;
        None
    }
}

/// The backend for this platform, if there is one.
pub fn platform_backend() -> Option<Box<dyn Backend>> {
    if cfg!(target_os = "linux") {
        Some(Box::new(LinuxBackend::new("/")))
    } else {
        None
    }
}

/// One logical CPU in a sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Core {
    pub id: u32,
    pub kind: CoreKind,
    /// Busy share of the second, 0 to 1; unsupported when the CPU reported no time.
    pub busy: Reading<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cpu {
    /// Busy share of the whole machine, 0 to 1.
    pub busy: Reading<f32>,
    pub cores: Vec<Core>,
}

/// One block device's throughput over the second, in bytes per second.
#[derive(Clone, Debug, PartialEq)]
pub struct Disk {
    /// One allocation per device, shared by every sample that names it.
    pub name: Arc<str>,
    pub read: Reading<u64>,
    pub write: Reading<u64>,
}

/// The system at one second.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// Unix seconds.
    pub time: i64,
    pub cpu: Reading<Cpu>,
    pub memory: Reading<Memory>,
    pub disks: Reading<Vec<Disk>>,
    /// GPU busy share, temperature (°C) and power draw (W): no reader yet, so unsupported.
    pub gpu: Reading<f32>,
    pub temperature: Reading<f32>,
    pub power: Reading<f32>,
    /// The process table as of this second (read every two seconds), the top [`RETAINED`].
    pub processes: Reading<Arc<Processes>>,
    /// Each network interface's bytes received and sent since the previous reading.
    pub network: Reading<Vec<Interface>>,
}

impl Sample {
    /// Bytes this sample holds, for the memory budget. Disk and interface names are shared by
    /// every sample (see [`Disk::name`]), and the process table by the seconds it stands for,
    /// so none is counted here.
    pub fn heap_size(&self) -> usize {
        let cores = self.cpu.value().map_or(0, |c| c.cores.capacity() * size_of::<Core>());
        let disks = self.disks.value().map_or(0, |d| d.capacity() * size_of::<Disk>());
        let interfaces = self.network.value().map_or(0, |i| i.capacity() * size_of::<Interface>());
        size_of::<Self>() + cores + disks + interfaces
    }
}

/// How far a cumulative counter moved from `prev` to `now`. A 32-bit counter (a 32-bit
/// kernel's) that passed its limit wrapped, which shows as a small step past it; any other
/// step back is a reset (the device came back), which says nothing about the second.
pub(crate) fn delta(prev: u64, now: u64) -> Option<u64> {
    if now >= prev {
        return Some(now - prev);
    }
    let wrapped = (now + (1 << 32)).checked_sub(prev)?;
    (prev <= u64::from(u32::MAX) && wrapped < 1 << 31).then_some(wrapped)
}

fn busy_share(prev: CpuTimes, now: CpuTimes) -> Reading<f32> {
    let (Some(busy), Some(total)) = (delta(prev.busy, now.busy), delta(prev.total, now.total)) else {
        return Reading::Unsupported;
    };
    if total == 0 {
        return Reading::Unsupported;
    }
    Reading::Value((busy as f32 / total as f32).clamp(0., 1.))
}

/// The sample of second `time` from the readings `prev` (taken `seconds` earlier) and `now`.
fn sample(time: i64, seconds: i64, prev: &Counters, now: &Counters, processes: Reading<Arc<Processes>>) -> Sample {
    let kind_of = |id: u32| match now.core_kinds.binary_search_by_key(&id, |&(i, _)| i) {
        Ok(i) => now.core_kinds[i].1,
        Err(_) => CoreKind::Unknown,
    };
    let cpu = match (&prev.cpu, &now.cpu) {
        (Reading::Value(p), Reading::Value(n)) => Reading::Value(Cpu {
            busy: busy_share(p.total, n.total),
            cores: n
                .cores
                .iter()
                .map(|&(id, times)| Core {
                    id,
                    kind: kind_of(id),
                    busy: match p.cores.binary_search_by_key(&id, |&(i, _)| i) {
                        Ok(i) => busy_share(p.cores[i].1, times),
                        Err(_) => Reading::Unsupported,
                    },
                })
                .collect(),
        }),
        _ => Reading::Unsupported,
    };
    let seconds = seconds.max(1) as u64;
    let rate = |before: u64, after: u64| -> Reading<u64> { delta(before, after).map(|d| d * SECTOR / seconds).into() };
    let disks = match (&prev.disks, &now.disks) {
        (Reading::Value(p), Reading::Value(n)) => Reading::Value(
            n.iter()
                .map(|d| match p.iter().find(|b| b.name == d.name) {
                    Some(b) => Disk {
                        name: d.name.clone(),
                        read: rate(b.sectors_read, d.sectors_read),
                        write: rate(b.sectors_written, d.sectors_written),
                    },
                    None => Disk {
                        name: d.name.clone(),
                        read: Reading::Unsupported,
                        write: Reading::Unsupported,
                    },
                })
                .collect(),
        ),
        _ => Reading::Unsupported,
    };
    let network = match (&prev.network, &now.network) {
        (Reading::Value(p), Reading::Value(n)) => Reading::Value(crate::net::interfaces(seconds as i64, p, n)),
        _ => Reading::Unsupported,
    };
    Sample {
        time,
        cpu,
        memory: now.memory.clone(),
        disks,
        gpu: Reading::Unsupported,
        temperature: Reading::Unsupported,
        power: Reading::Unsupported,
        processes,
        network,
    }
}

/// The backend and its previous reading, locked apart from the Timeline so a slow read never
/// holds up a client looking at samples.
struct Reader {
    backend: Box<dyn Backend>,
    /// The previous reading and its second; dropped when recording pauses, so a resumed
    /// recorder starts a new run instead of averaging over the pause.
    prev: Option<(i64, Counters)>,
    /// The previous process reading, dropped with `prev`.
    prev_processes: Option<PrevProcesses>,
    /// The kept part of the newest process table, which the seconds until the next one show.
    retained: Reading<Arc<Processes>>,
}

impl Reader {
    fn restart(&mut self) {
        self.prev = None;
        self.prev_processes = None;
        self.retained = Reading::Unsupported;
    }
}

struct State {
    timeline: Timeline<Sample>,
    /// The newest full process table: the live view lists every process.
    live_processes: Option<Arc<Processes>>,
    recording: bool,
    stopping: bool,
}

/// The recorder's state, shared by the [`Perf`] handle, its thread and the
/// [`Net`](crate::net::Net) handle, which reads the same Timeline.
pub(crate) struct Store {
    reader: Mutex<Reader>,
    state: Mutex<State>,
    /// A client shows live readings: every sample repaints, not just the first.
    watched: AtomicBool,
    /// A client shows the process table: each process's I/O is read.
    process_io: AtomicBool,
    wake: Condvar,
    repaint: Repaint,
}

/// The engine's handle on Performance recording. Dropping it stops the recorder thread.
pub struct Perf {
    inner: Arc<Store>,
}

impl Perf {
    /// A recorder reading `backend`, with no thread: samples are taken by [`Perf::tick`].
    pub fn manual(backend: Box<dyn Backend>, repaint: Repaint) -> Perf {
        Perf {
            inner: Arc::new(Store {
                reader: Mutex::new(Reader {
                    backend,
                    prev: None,
                    prev_processes: None,
                    retained: Reading::Unsupported,
                }),
                state: Mutex::new(State {
                    timeline: Timeline::new(SPAN),
                    live_processes: None,
                    recording: false,
                    stopping: false,
                }),
                watched: AtomicBool::new(false),
                process_io: AtomicBool::new(false),
                wake: Condvar::new(),
                repaint,
            }),
        }
    }

    /// A recorder that takes a sample at the start of every second while recording is on,
    /// or one that never records when the platform has no backend.
    pub fn start(backend: Option<Box<dyn Backend>>, repaint: Repaint) -> Perf {
        let Some(backend) = backend else {
            return Perf::manual(Box::new(NoBackend), repaint);
        };
        let perf = Perf::manual(backend, repaint);
        let inner = perf.inner.clone();
        thread::Builder::new()
            .name("ve-perf".into())
            .spawn(move || record(&inner))
            .expect("spawn perf thread");
        perf
    }

    /// Turns recording on or off. Off parks the recorder; the seconds until it is turned on
    /// again are a gap.
    pub fn set_recording(&self, on: bool) {
        {
            let mut state = self.inner.state.lock();
            if state.recording == on {
                return;
            }
            state.recording = on;
            self.inner.wake.notify_all();
        }
        self.inner.reader.lock().restart();
    }

    /// Whether a client shows live readings (a Performance or Network page is open): then
    /// every sample repaints. Otherwise only the first sample of a run does (so the rail can
    /// show the Modules).
    pub fn set_watched(&self, on: bool) {
        self.inner.watched.store(on, Ordering::Relaxed);
    }

    /// Whether a client shows the process table (the Performance page is open): then each
    /// process's I/O is read with it, which costs a read per process. Otherwise it is not.
    pub fn set_process_io(&self, on: bool) {
        self.inner.process_io.store(on, Ordering::Relaxed);
    }

    /// Reads the counters as of second `now` and records that second's sample (the recorder
    /// thread calls this every second). The first reading of a run, and one after a step of
    /// more than a few seconds, only primes the next. Samples older than an hour before `now`
    /// are dropped.
    pub fn tick(&self, now: i64) {
        self.inner.tick(now);
    }

    /// The newest sample, if any second of the last hour was recorded.
    pub fn live(&self) -> Option<Arc<Sample>> {
        self.inner.state.lock().timeline.latest().map(|(_, s)| s.clone())
    }

    /// The sample shown at `moment`: the newest for Live, that second's for a time. `None`
    /// is a gap (nothing was recorded then, or it is older than an hour).
    pub fn at(&self, moment: Moment) -> Option<Arc<Sample>> {
        match moment {
            Moment::Live => self.live(),
            Moment::At(time) => self.inner.state.lock().timeline.at(time).cloned(),
        }
    }

    /// The process table shown at `moment`: every process for Live, the kept top
    /// [`RETAINED`] of that second's table for a time. `None` is a gap, or a platform that
    /// cannot read processes.
    pub fn processes(&self, moment: Moment) -> Option<Arc<Processes>> {
        let state = self.inner.state.lock();
        match moment {
            Moment::Live => state.timeline.latest().and(state.live_processes.clone()),
            Moment::At(time) => state.timeline.at(time)?.processes.value().cloned(),
        }
    }

    /// The command line and user of running process `id`, read now; `None` once it has exited.
    pub fn details(&self, id: ProcessId) -> Option<Details> {
        self.inner.reader.lock().backend.details(id)
    }

    /// Whether at least one second is recorded.
    pub fn has_samples(&self) -> bool {
        !self.inner.state.lock().timeline.is_empty()
    }

    /// How many seconds from `from` to `to` (inclusive) have a sample.
    pub fn count(&self, from: i64, to: i64) -> usize {
        self.inner.state.lock().timeline.range(from, to).count()
    }

    /// `from` to `to` (inclusive) cut into `buckets` equal slices, each with the highest
    /// `value` of its samples, or `None` where no sample has one: a gap.
    pub fn peaks(&self, from: i64, to: i64, buckets: usize, value: impl Fn(&Sample) -> Option<f32>) -> Vec<Option<f32>> {
        self.inner.peaks(from, to, buckets, value)
    }

    /// The store behind this handle, for the Network handle.
    pub(crate) fn store(&self) -> Arc<Store> {
        self.inner.clone()
    }

    /// The runs of seconds from `from` to `to` with no sample, as inclusive (first, last).
    pub fn gaps(&self, from: i64, to: i64) -> Vec<(i64, i64)> {
        self.inner.state.lock().timeline.gaps(from, to)
    }

    /// Bytes the kept samples take in memory, each process table counted once, the live full
    /// table included.
    pub fn memory_bytes(&self) -> usize {
        let state = self.inner.state.lock();
        let mut last: Option<&Arc<Processes>> = None;
        let mut bytes = 0;
        for (_, s) in state.timeline.range(i64::MIN, i64::MAX) {
            bytes += s.heap_size() + size_of::<(i64, Arc<Sample>)>() + 2 * size_of::<usize>();
            if let Reading::Value(table) = &s.processes
                && last.is_none_or(|l| !Arc::ptr_eq(l, table))
            {
                bytes += table.heap_size() + 2 * size_of::<usize>();
                last = Some(table);
            }
        }
        if let Some(live) = &state.live_processes
            && last.is_none_or(|l| !Arc::ptr_eq(l, live))
        {
            bytes += live.heap_size();
        }
        bytes
    }
}

impl Drop for Perf {
    fn drop(&mut self) {
        self.inner.state.lock().stopping = true;
        self.inner.wake.notify_all();
    }
}

/// The recorder thread: parked while recording is off, otherwise one sample at the start of
/// each second.
fn record(inner: &Arc<Store>) {
    loop {
        {
            let mut state = inner.state.lock();
            while !state.recording && !state.stopping {
                inner.wake.wait(&mut state);
            }
            if state.stopping {
                return;
            }
        }
        let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let into_second = Duration::from_nanos(u64::from(since_epoch.subsec_nanos()));
        // Just past the next second's start, so a sample never lands in the second it left.
        let deadline = Instant::now() + Duration::from_secs(1) - into_second + Duration::from_millis(5);
        {
            let mut state = inner.state.lock();
            // A toggle of recording wakes this early; only the deadline ends the wait.
            while state.recording && !state.stopping && Instant::now() < deadline {
                inner.wake.wait_until(&mut state, deadline);
            }
            if state.stopping {
                return;
            }
            if !state.recording {
                continue;
            }
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        inner.tick(now);
    }
}

impl Store {
    /// Runs `f` on the Timeline, under its lock.
    pub(crate) fn timeline<R>(&self, f: impl FnOnce(&Timeline<Sample>) -> R) -> R {
        f(&self.state.lock().timeline)
    }

    /// `from` to `to` (inclusive) cut into `buckets` equal slices, each with the highest
    /// `value` of its samples, or `None` where no sample has one: a gap.
    pub(crate) fn peaks<T: PartialOrd + Copy>(
        &self,
        from: i64,
        to: i64,
        buckets: usize,
        value: impl Fn(&Sample) -> Option<T>,
    ) -> Vec<Option<T>> {
        let mut peaks = vec![None; buckets];
        let span = (to - from + 1).max(1) as usize;
        let state = self.state.lock();
        for (t, s) in state.timeline.range(from, to) {
            if let (Some(v), Some(peak)) = (value(s), peaks.get_mut((t - from) as usize * buckets / span)) {
                *peak = Some(match *peak {
                    Some(p) if p >= v => p,
                    _ => v,
                });
            }
        }
        peaks
    }

    fn tick(&self, now: i64) {
        let (sample, live_processes) = {
            let mut reader = self.reader.lock();
            let counters = reader.backend.read();
            let live = self.read_processes(&mut reader, now, &counters);
            let sample = match reader.prev.take() {
                Some((then, prev)) if now > then && now - then <= MAX_STEP => {
                    Some(sample(now, now - then, &prev, &counters, reader.retained.clone()))
                }
                _ => None,
            };
            reader.prev = Some((now, counters));
            (sample, live)
        };
        let first = {
            let mut state = self.state.lock();
            state.timeline.expire(now);
            if let Some(live) = live_processes {
                state.live_processes = live;
            }
            let Some(sample) = sample else { return };
            let first = state.timeline.is_empty();
            state.timeline.push(now, Arc::new(sample));
            first
        };
        if first || self.watched.load(Ordering::Relaxed) {
            (self.repaint)();
        }
    }

    /// Reads the process table when one is due (none yet, or the last is two seconds old),
    /// keeps its top part in `reader.retained` for the samples and returns the full table for
    /// the live view: `Some(None)` when there is none (it cannot be read, or this reading only
    /// primes the next). `None` when no table was due.
    fn read_processes(&self, reader: &mut Reader, now: i64, counters: &Counters) -> Option<Option<Arc<Processes>>> {
        let due = match &reader.prev_processes {
            Some(p) => now - p.time >= PROCESS_STEP || now <= p.time,
            None => true,
        } || matches!(reader.retained, Reading::Unsupported);
        if !due {
            return None;
        }
        let io = self.process_io.load(Ordering::Relaxed);
        let machine_ticks = counters.cpu.value().map(|c| c.total.total);
        let read = reader.backend.processes(io);
        let prev = reader.prev_processes.take().filter(|p| now > p.time && now - p.time <= MAX_STEP);
        reader.retained = Reading::Unsupported;
        let Reading::Value(read) = read else {
            return Some(None);
        };
        reader.prev_processes = Some(PrevProcesses::of(now, &read, machine_ticks));
        // The first reading of a run only primes the next: it has no CPU shares.
        let Some(prev) = prev else {
            return Some(None);
        };
        let full = Arc::new(process::table(now, &read, machine_ticks, &prev));
        reader.retained = Reading::Value(full.retained());
        Some(Some(full))
    }
}

/// A platform with no reader: nothing is ever recorded.
struct NoBackend;

impl Backend for NoBackend {
    fn read(&mut self) -> Counters {
        Counters {
            cpu: Reading::Unsupported,
            core_kinds: Arc::from([]),
            memory: Reading::Unsupported,
            disks: Reading::Unsupported,
            network: Reading::Unsupported,
        }
    }
}
