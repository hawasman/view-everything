//! The process table of a sample (#50): every process by its [`ProcessId`] with its CPU share,
//! resident memory and, while a client watches, its I/O. The live table lists every process;
//! a sample keeps the top [`RETAINED`] of it.

use super::{Reading, delta};
use std::collections::HashMap;
use std::sync::Arc;

/// Processes a sample keeps: the busiest by CPU and the largest by resident memory.
pub const RETAINED: usize = 200;

/// A process for as long as it runs: its PID and its start time (clock ticks after boot). A
/// recycled PID has another start time, so it is another process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessId {
    pub pid: u32,
    pub start: u64,
}

/// Bytes a process read from and wrote to storage: cumulative in [`ProcessCounters`], per
/// second in [`Process`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Io {
    pub read: u64,
    pub write: u64,
}

/// One process as a backend reads it: cumulative CPU time and I/O, resident memory now.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessCounters {
    pub id: ProcessId,
    pub ppid: u32,
    /// One allocation per name, shared by every reading that has it.
    pub name: Arc<str>,
    /// User plus system time, in clock ticks.
    pub cpu_ticks: u64,
    pub rss: Option<u64>,
    /// Only read when asked for, and only where readable (processes of other users are not).
    pub io: Option<Io>,
}

/// The processes running at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessCountersTable {
    pub processes: Vec<ProcessCounters>,
    /// When the machine started, in Unix seconds; process start times count from it.
    pub boot_time: Option<i64>,
    /// Clock ticks per second, the unit of start and CPU times.
    pub ticks_per_second: u64,
}

/// One process in a sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Process {
    pub id: ProcessId,
    pub ppid: u32,
    pub name: Arc<str>,
    /// Share of the whole machine's CPU time since the previous table, 0 to 1; unknown for a
    /// process that was not running then.
    pub cpu: Reading<f32>,
    pub rss: Reading<u64>,
    /// CPU time used since it started, in clock ticks.
    pub cpu_ticks: u64,
    /// Bytes per second; unknown while no client watches, or where not readable.
    pub io: Reading<Io>,
}

/// The process table of a sample: read every two seconds, so a second between two readings
/// shows the one before it (see `time`).
#[derive(Clone, Debug, PartialEq)]
pub struct Processes {
    /// The second it was read, Unix seconds.
    pub time: i64,
    pub boot_time: Option<i64>,
    pub ticks_per_second: u64,
    /// Sorted by id.
    pub list: Vec<Process>,
}

impl Processes {
    pub fn get(&self, id: ProcessId) -> Option<&Process> {
        self.list.binary_search_by_key(&id, |p| p.id).ok().map(|i| &self.list[i])
    }

    /// The running process with PID `pid`, such as a process's parent.
    pub fn by_pid(&self, pid: u32) -> Option<&Process> {
        let i = self.list.partition_point(|p| p.id.pid < pid);
        self.list.get(i).filter(|p| p.id.pid == pid)
    }

    /// When `process` started, in Unix seconds.
    pub fn started(&self, process: &Process) -> Option<i64> {
        let ticks = self.ticks_per_second.max(1);
        self.boot_time.map(|boot| boot + (process.id.start / ticks) as i64)
    }

    /// The CPU time `process` used since it started, in seconds.
    pub fn cpu_seconds(&self, process: &Process) -> f64 {
        process.cpu_ticks as f64 / self.ticks_per_second.max(1) as f64
    }

    /// The kept part of a full table: the [`RETAINED`] busiest by CPU and largest by memory,
    /// taken in turn, or the table itself when it is no longer.
    pub fn retained(self: &Arc<Self>) -> Arc<Self> {
        if self.list.len() <= RETAINED {
            return self.clone();
        }
        let rank = |key: &dyn Fn(&Process) -> Option<u64>| {
            let mut order: Vec<usize> = (0..self.list.len()).collect();
            // Unknown last; ties by id, so the pick is stable.
            order.sort_by(|&a, &b| {
                let (a, b) = (&self.list[a], &self.list[b]);
                key(b).cmp(&key(a)).then(a.id.cmp(&b.id))
            });
            order
        };
        let by_cpu = rank(&|p| p.cpu.value().map(|c| u64::from(c.to_bits())));
        let by_rss = rank(&|p| p.rss.value().copied());
        let mut keep = vec![false; self.list.len()];
        let mut kept = 0;
        let (mut cpu, mut rss) = (by_cpu.into_iter(), by_rss.into_iter());
        while kept < RETAINED {
            let mut took = false;
            for next in [cpu.next(), rss.next()].into_iter().flatten() {
                took = true;
                if kept < RETAINED && !keep[next] {
                    keep[next] = true;
                    kept += 1;
                }
            }
            if !took {
                break;
            }
        }
        Arc::new(Processes {
            time: self.time,
            boot_time: self.boot_time,
            ticks_per_second: self.ticks_per_second,
            list: self.list.iter().zip(keep).filter(|&(_, k)| k).map(|(p, _)| p.clone()).collect(),
        })
    }

    /// Bytes the table holds, for the memory budget; names are shared, so not counted.
    pub fn heap_size(&self) -> usize {
        size_of::<Self>() + self.list.capacity() * size_of::<Process>()
    }
}

/// The previous process reading, which the next one's rates are taken against.
pub(super) struct PrevProcesses {
    pub time: i64,
    /// The machine's total CPU ticks at that reading.
    pub machine_ticks: Option<u64>,
    pub counters: HashMap<ProcessId, (u64, Option<Io>)>,
}

/// The process table of second `time` from `now` (with the machine's total CPU ticks then)
/// and the previous reading.
pub(super) fn table(time: i64, now: &ProcessCountersTable, machine_ticks: Option<u64>, prev: &PrevProcesses) -> Processes {
    let seconds = (time - prev.time).max(1) as u64;
    let machine = prev
        .machine_ticks
        .zip(machine_ticks)
        .and_then(|(p, n)| delta(p, n))
        .filter(|&d| d > 0);
    let mut list: Vec<Process> = now
        .processes
        .iter()
        .map(|c| {
            let before = prev.counters.get(&c.id);
            let cpu = match (before, machine) {
                (Some(&(ticks, _)), Some(machine)) => c
                    .cpu_ticks
                    .checked_sub(ticks)
                    .map(|d| (d as f32 / machine as f32).clamp(0., 1.))
                    .into(),
                _ => Reading::Unsupported,
            };
            let io = match (before.and_then(|&(_, io)| io), c.io) {
                (Some(b), Some(n)) => match (n.read.checked_sub(b.read), n.write.checked_sub(b.write)) {
                    (Some(r), Some(w)) => Reading::Value(Io {
                        read: r / seconds,
                        write: w / seconds,
                    }),
                    _ => Reading::Unsupported,
                },
                _ => Reading::Unsupported,
            };
            Process {
                id: c.id,
                ppid: c.ppid,
                name: c.name.clone(),
                cpu,
                rss: c.rss.into(),
                cpu_ticks: c.cpu_ticks,
                io,
            }
        })
        .collect();
    list.sort_unstable_by_key(|p| p.id);
    Processes {
        time,
        boot_time: now.boot_time,
        ticks_per_second: now.ticks_per_second,
        list,
    }
}

impl PrevProcesses {
    pub fn of(time: i64, now: &ProcessCountersTable, machine_ticks: Option<u64>) -> Self {
        Self {
            time,
            machine_ticks,
            counters: now.processes.iter().map(|c| (c.id, (c.cpu_ticks, c.io))).collect(),
        }
    }
}

/// What can be read about a running process beyond its table row, on request.
#[derive(Clone, Debug, PartialEq)]
pub struct Details {
    /// Its command line; unknown for a kernel thread or a process that cannot be read.
    pub command: Reading<String>,
    /// The name of the user it runs as.
    pub user: Reading<String>,
}
