//! `perf.summary` through the `Perf` handle (#59): what the Overview's "what is slowing the
//! machine" answer reads, from a scripted backend.

use std::sync::Arc;
use ve_engine::perf::{Backend, Counters, CpuCounters, CpuTimes, Memory, Perf, ProcessCounters, ProcessCountersTable, ProcessId, Reading};

/// A machine whose CPU is busy `busy` ticks of every 100 a reading, with `used` of 1000 bytes
/// of memory taken, running `processes`: (PID, name, CPU ticks per reading, readings it runs).
struct Script {
    readings: u64,
    busy: u64,
    memory: Option<u64>,
    processes: Vec<(u32, &'static str, u64, std::ops::Range<u64>)>,
}

impl Backend for Script {
    fn read(&mut self) -> Counters {
        self.readings += 1;
        let n = self.readings;
        Counters {
            cpu: Reading::Value(CpuCounters {
                total: CpuTimes {
                    busy: self.busy * n,
                    total: 100 * n,
                },
                cores: Vec::new(),
            }),
            core_kinds: Arc::from([]),
            memory: match self.memory {
                Some(used) => Reading::Value(Memory {
                    total: 1000,
                    used,
                    cache: 0,
                    free: 1000 - used,
                    available: Reading::Unsupported,
                    swap: Reading::Unsupported,
                }),
                None => Reading::Unsupported,
            },
            disks: Reading::Unsupported,
            network: Reading::Unsupported,
        }
    }

    fn processes(&mut self, _io: bool) -> Reading<ProcessCountersTable> {
        let n = self.readings;
        Reading::Value(ProcessCountersTable {
            processes: self
                .processes
                .iter()
                .filter(|(.., runs)| runs.contains(&n))
                .map(|&(pid, name, ticks, _)| ProcessCounters {
                    id: ProcessId { pid, start: 1 },
                    ppid: 1,
                    name: Arc::from(name),
                    cpu_ticks: ticks * n,
                    rss: None,
                    io: None,
                })
                .collect(),
            boot_time: None,
            ticks_per_second: 100,
        })
    }
}

fn perf(script: Script) -> Perf {
    Perf::manual(Box::new(script), Arc::new(|| {}))
}

#[test]
fn the_summary_averages_cpu_and_memory_and_names_the_busiest_process_over_the_stretch() {
    let perf = perf(Script {
        readings: 0,
        busy: 50,
        memory: Some(250),
        processes: vec![
            // Steady at a fifth of the machine for the whole stretch.
            (10, "steady", 20, 0..u64::MAX),
            // Nine tenths of the machine, but only for two of the stretch's ten process tables.
            (20, "spike", 90, 2..7),
        ],
    });
    for t in 100..=120 {
        perf.tick(t);
    }
    let summary = perf.summary(0, 200);
    assert_eq!(summary.seconds, 20);
    assert_eq!(summary.cpu, Some(0.5));
    assert_eq!(summary.memory, Some(0.25));
    let top = summary.top.expect("a busiest process");
    assert_eq!(&*top.name, "steady");
    assert_eq!(top.id, ProcessId { pid: 10, start: 1 });
    assert!((top.cpu - 0.2).abs() < 1e-6, "mean share {}", top.cpu);
}

#[test]
fn a_summary_covers_only_its_range() {
    let perf = perf(Script {
        readings: 0,
        busy: 50,
        memory: Some(250),
        processes: Vec::new(),
    });
    for t in 100..=120 {
        perf.tick(t);
    }
    assert_eq!(perf.summary(111, 115).seconds, 5);
    assert_eq!(perf.summary(200, 300).seconds, 0);
}

#[test]
fn what_was_not_recorded_is_unknown_not_zero() {
    let nothing = perf(Script {
        readings: 0,
        busy: 50,
        memory: Some(250),
        processes: Vec::new(),
    })
    .summary(0, 200);
    assert_eq!(nothing.seconds, 0);
    assert_eq!((nothing.cpu, nothing.memory), (None, None));
    assert!(nothing.top.is_none());

    let no_memory = perf(Script {
        readings: 0,
        busy: 50,
        memory: None,
        processes: Vec::new(),
    });
    for t in 100..=105 {
        no_memory.tick(t);
    }
    let summary = no_memory.summary(0, 200);
    assert_eq!(summary.cpu, Some(0.5));
    assert_eq!(summary.memory, None);
    // An idle process table names no busiest process.
    assert!(summary.top.is_none());
}
