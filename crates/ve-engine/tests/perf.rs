//! Drives the Performance recorder through the `Perf` handle: the Linux backend over fixture
//! `/proc` and `/sys` trees, and the one-hour Timeline behind `at` and `live`.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use ve_engine::perf::{Backend, CoreKind, Counters, CpuCounters, CpuTimes, DiskCounters, LinuxBackend, Perf, Reading, Sample, Swap};
use ve_engine::timeline::Moment;

fn put(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// `/proc/stat` with the given (busy, idle) ticks for the machine and each CPU. Busy time is
/// split over user and system, idle over idle and iowait, so both count.
fn stat(total: (u64, u64), cores: &[(u64, u64)]) -> String {
    let line = |label: String, (busy, idle): (u64, u64)| {
        format!(
            "{label} {} 0 {} {} {} 0 0 0 0 0\n",
            busy / 2,
            busy - busy / 2,
            idle / 2,
            idle - idle / 2
        )
    };
    let mut text = line("cpu ".into(), total);
    for (i, &c) in cores.iter().enumerate() {
        text += &line(format!("cpu{i}"), c);
    }
    text + "intr 1 2 3\nctxt 42\nbtime 1700000000\n"
}

/// A `/proc/diskstats` line: sectors read in field 6, written in field 10.
fn disk(major: u32, minor: u32, name: &str, read: u64, written: u64) -> String {
    format!("{major:4} {minor:7} {name} 10 0 {read} 5 20 0 {written} 7 0 9 12 0 0 0 0 0 0\n")
}

const MEMINFO: &str = "MemTotal:       16000000 kB
MemFree:         2000000 kB
MemAvailable:    9000000 kB
Buffers:          500000 kB
Cached:          6000000 kB
SwapCached:            0 kB
Shmem:            700000 kB
SReclaimable:     200000 kB
SwapTotal:       8000000 kB
SwapFree:        7000000 kB
";

/// A machine with two CPUs, a SATA disk with a partition, an NVMe disk and a loop device.
fn machine(root: &Path) {
    put(root, "proc/meminfo", MEMINFO);
    for name in ["sda", "nvme0n1", "loop0"] {
        fs::create_dir_all(root.join("sys/block").join(name)).unwrap();
    }
    counters(root, 0, 0);
}

/// The counters `step` seconds in: each second CPU0 is busy 25 of 100 ticks, CPU1 75, sda
/// reads 2048 sectors (1 MiB), nvme0n1 writes 4096 (2 MiB); the partition and the loop device
/// move too and must not count.
fn counters(root: &Path, step: u64, base: u64) {
    put(
        root,
        "proc/stat",
        &stat(
            (1000 + 100 * step, 1000 + 100 * step),
            &[(500 + 25 * step, 500 + 75 * step), (500 + 75 * step, 500 + 25 * step)],
        ),
    );
    let text = disk(8, 0, "sda", base + 2048 * step, 0)
        + &disk(8, 1, "sda1", base + 2048 * step, 0)
        + &disk(259, 0, "nvme0n1", 0, base + 4096 * step)
        + &disk(7, 0, "loop0", 999 * step, 999 * step);
    put(root, "proc/diskstats", &text);
}

fn no_repaint() -> ve_engine::Repaint {
    Arc::new(|| {})
}

fn linux(root: &Path) -> Perf {
    Perf::manual(Box::new(LinuxBackend::new(root)), no_repaint())
}

fn at(perf: &Perf, time: i64) -> Arc<Sample> {
    perf.at(Moment::At(time)).unwrap_or_else(|| panic!("no sample at {time}"))
}

fn rate(disks: &Reading<Vec<ve_engine::perf::Disk>>, name: &str) -> (Reading<u64>, Reading<u64>) {
    let d = disks.value().unwrap().iter().find(|d| &*d.name == name).unwrap();
    (d.read.clone(), d.write.clone())
}

#[test]
fn a_second_reads_cpu_memory_and_each_whole_disk() {
    let dir = tempfile::tempdir().unwrap();
    machine(dir.path());
    let perf = linux(dir.path());
    perf.tick(100);
    counters(dir.path(), 1, 0);
    perf.tick(101);

    let s = at(&perf, 101);
    assert_eq!(s.time, 101);
    let cpu = s.cpu.value().unwrap();
    assert_eq!(cpu.busy, Reading::Value(0.5));
    let busy: Vec<_> = cpu.cores.iter().map(|c| (c.id, c.busy.clone())).collect();
    assert_eq!(busy, vec![(0, Reading::Value(0.25)), (1, Reading::Value(0.75))]);

    let m = s.memory.value().unwrap();
    assert_eq!(m.total, 16_000_000 * 1024);
    // Cache: buffers + cached + reclaimable slab - shared memory.
    assert_eq!(m.cache, 6_000_000 * 1024);
    assert_eq!(m.used, 8_000_000 * 1024);
    assert_eq!(m.free, 2_000_000 * 1024);
    assert_eq!(m.available, Reading::Value(9_000_000 * 1024));
    assert_eq!(
        m.swap,
        Reading::Value(Swap {
            total: 8_000_000 * 1024,
            used: 1_000_000 * 1024
        })
    );

    let names: Vec<&str> = s.disks.value().unwrap().iter().map(|d| &*d.name).collect();
    assert_eq!(names, ["sda", "nvme0n1"], "partitions and loop devices are not disks");
    assert_eq!(rate(&s.disks, "sda"), (Reading::Value(1 << 20), Reading::Value(0)));
    assert_eq!(rate(&s.disks, "nvme0n1"), (Reading::Value(0), Reading::Value(2 << 20)));
}

#[test]
fn hybrid_intel_cores_are_told_apart() {
    let dir = tempfile::tempdir().unwrap();
    machine(dir.path());
    put(dir.path(), "sys/devices/cpu_core/cpus", "0\n");
    put(dir.path(), "sys/devices/cpu_atom/cpus", "1\n");
    let perf = linux(dir.path());
    perf.tick(1);
    counters(dir.path(), 1, 0);
    perf.tick(2);
    let kinds: Vec<_> = at(&perf, 2).cpu.value().unwrap().cores.iter().map(|c| c.kind).collect();
    assert_eq!(kinds, [CoreKind::Performance, CoreKind::Efficiency]);
}

#[test]
fn a_hybrid_kernel_on_a_part_with_no_efficiency_cores_tells_nothing_apart() {
    // An i5-12400F: the hybrid driver lists every CPU under cpu_core and none under cpu_atom.
    let dir = tempfile::tempdir().unwrap();
    machine(dir.path());
    put(dir.path(), "sys/devices/cpu_core/cpus", "0-1\n");
    put(dir.path(), "sys/devices/cpu_atom/cpus", "\n");
    let perf = linux(dir.path());
    perf.tick(1);
    counters(dir.path(), 1, 0);
    perf.tick(2);
    let kinds: Vec<_> = at(&perf, 2).cpu.value().unwrap().cores.iter().map(|c| c.kind).collect();
    assert_eq!(kinds, [CoreKind::Unknown, CoreKind::Unknown]);
}

#[test]
fn big_and_little_cores_are_told_apart_by_capacity() {
    let dir = tempfile::tempdir().unwrap();
    machine(dir.path());
    put(dir.path(), "sys/devices/system/cpu/cpu0/cpu_capacity", "446\n");
    put(dir.path(), "sys/devices/system/cpu/cpu1/cpu_capacity", "1024\n");
    let perf = linux(dir.path());
    perf.tick(1);
    counters(dir.path(), 1, 0);
    perf.tick(2);
    let kinds: Vec<_> = at(&perf, 2).cpu.value().unwrap().cores.iter().map(|c| c.kind).collect();
    assert_eq!(kinds, [CoreKind::Efficiency, CoreKind::Performance]);
}

#[test]
fn cores_the_kernel_does_not_tell_apart_are_unknown() {
    let dir = tempfile::tempdir().unwrap();
    machine(dir.path());
    // Equal capacities say nothing about kinds.
    put(dir.path(), "sys/devices/system/cpu/cpu0/cpu_capacity", "1024\n");
    put(dir.path(), "sys/devices/system/cpu/cpu1/cpu_capacity", "1024\n");
    let perf = linux(dir.path());
    perf.tick(1);
    counters(dir.path(), 1, 0);
    perf.tick(2);
    let kinds: Vec<_> = at(&perf, 2).cpu.value().unwrap().cores.iter().map(|c| c.kind).collect();
    assert_eq!(kinds, [CoreKind::Unknown, CoreKind::Unknown]);
}

#[test]
fn what_cannot_be_read_is_unsupported_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "proc/stat", &stat((100, 100), &[]));
    // An old kernel: no MemAvailable, no swap fields. No diskstats at all. A thermal zone the
    // app does not read.
    put(
        dir.path(),
        "proc/meminfo",
        "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 10 kB\nCached: 300 kB\n",
    );
    put(dir.path(), "sys/class/thermal/thermal_zone0/temp", "45000\n");
    let perf = linux(dir.path());
    perf.tick(1);
    put(dir.path(), "proc/stat", &stat((150, 150), &[]));
    perf.tick(2);

    let s = at(&perf, 2);
    assert_eq!(s.cpu.value().unwrap().busy, Reading::Value(0.5));
    let m = s.memory.value().unwrap();
    assert_eq!((m.available.clone(), m.swap.clone()), (Reading::Unsupported, Reading::Unsupported));
    assert_eq!((m.cache, m.used), (310 * 1024, 590 * 1024));
    assert_eq!(s.disks, Reading::Unsupported);
    assert_eq!(
        (s.gpu.clone(), s.temperature.clone(), s.power.clone()),
        (Reading::Unsupported, Reading::Unsupported, Reading::Unsupported)
    );

    // Nothing readable at all.
    let empty = tempfile::tempdir().unwrap();
    let perf = linux(empty.path());
    perf.tick(1);
    perf.tick(2);
    let s = at(&perf, 2);
    assert_eq!((s.cpu.value(), s.memory.value(), s.disks.value()), (None, None, None));
}

#[test]
fn a_32_bit_counter_that_wraps_still_gives_the_rate() {
    let dir = tempfile::tempdir().unwrap();
    machine(dir.path());
    // sda's sector count sits 1024 short of the 32-bit limit, then wraps past it.
    let near = u64::from(u32::MAX) + 1 - 1024;
    counters(dir.path(), 0, near);
    let perf = linux(dir.path());
    perf.tick(1);
    put(dir.path(), "proc/diskstats", &disk(8, 0, "sda", 1024, 0));
    perf.tick(2);
    assert_eq!(rate(&at(&perf, 2).disks, "sda").0, Reading::Value(2048 * 512));
}

#[test]
fn a_counter_reset_is_unsupported_for_that_second() {
    let dir = tempfile::tempdir().unwrap();
    machine(dir.path());
    counters(dir.path(), 0, 1 << 40);
    let perf = linux(dir.path());
    perf.tick(1);
    // The disk came back with fresh counters: the step back is no wrap.
    put(dir.path(), "proc/diskstats", &disk(8, 0, "sda", 10, 0));
    perf.tick(2);
    assert_eq!(rate(&at(&perf, 2).disks, "sda").0, Reading::Unsupported);
}

#[test]
fn a_reset_of_a_small_counter_is_no_wrap() {
    let dir = tempfile::tempdir().unwrap();
    machine(dir.path());
    // About 500 MB read, then the disk came back counting from zero again.
    counters(dir.path(), 0, 1_000_000);
    let perf = linux(dir.path());
    perf.tick(1);
    put(dir.path(), "proc/diskstats", &disk(8, 0, "sda", 10, 0));
    perf.tick(2);
    assert_eq!(rate(&at(&perf, 2).disks, "sda").0, Reading::Unsupported);
}

/// Counters that move by a fixed amount each reading, for driving the Timeline fast.
struct Steady {
    readings: u64,
    cores: u32,
    disks: Vec<Arc<str>>,
}

impl Steady {
    fn new(cores: u32, disks: usize) -> Self {
        Self {
            readings: 0,
            cores,
            disks: (0..disks).map(|i| Arc::from(format!("nvme{i}n1"))).collect(),
        }
    }
}

impl Backend for Steady {
    fn read(&mut self) -> Counters {
        self.readings += 1;
        let n = self.readings;
        let times = CpuTimes {
            busy: 30 * n,
            total: 100 * n,
        };
        Counters {
            cpu: Reading::Value(CpuCounters {
                total: times,
                cores: (0..self.cores).map(|id| (id, times)).collect(),
            }),
            core_kinds: Arc::from([]),
            memory: Reading::Unsupported,
            disks: Reading::Value(
                self.disks
                    .iter()
                    .map(|name| DiskCounters {
                        name: name.clone(),
                        sectors_read: n,
                        sectors_written: n,
                    })
                    .collect(),
            ),
        }
    }
}

fn steady() -> Perf {
    Perf::manual(Box::new(Steady::new(2, 1)), no_repaint())
}

#[test]
fn the_first_reading_only_primes_the_next() {
    let perf = steady();
    perf.tick(10);
    assert!(!perf.has_samples());
    assert!(perf.live().is_none());
    perf.tick(11);
    assert!(perf.has_samples());
    assert_eq!(perf.live().unwrap().time, 11);
}

#[test]
fn samples_older_than_an_hour_are_evicted() {
    let perf = steady();
    for t in 0..=4000 {
        perf.tick(t);
    }
    assert!(perf.at(Moment::At(400)).is_none());
    assert!(perf.at(Moment::At(401)).is_some(), "the last 3600 seconds are kept");
    assert!(perf.at(Moment::At(4000)).is_some());
    assert_eq!(perf.count(0, 4000), 3600);
}

#[test]
fn seconds_not_recorded_are_gaps() {
    let perf = steady();
    for t in 10..=12 {
        perf.tick(t);
    }
    // Recording paused, then on again: the next reading starts a new run.
    perf.set_recording(false);
    perf.set_recording(true);
    for t in 20..=22 {
        perf.tick(t);
    }
    // The recorder stalled for eight seconds: that step is no sample either.
    perf.tick(30);
    perf.tick(31);

    assert!(perf.at(Moment::At(15)).is_none());
    assert!(perf.at(Moment::At(20)).is_none(), "the first reading of a run only primes");
    assert!(perf.at(Moment::At(30)).is_none());
    assert_eq!(perf.gaps(10, 31), vec![(10, 10), (13, 20), (23, 30)]);
    assert_eq!(perf.count(10, 31), 5);
    // Drawn two seconds to a column: a column with no sample stays empty, never zero.
    let s = Some(0.3);
    assert_eq!(
        perf.peaks(10, 31, 11, |s| s.cpu.value().and_then(|c| c.busy.value().copied())),
        [s, s, None, None, None, s, s, None, None, None, s]
    );
}

#[test]
fn a_resumed_recorder_drops_what_is_older_than_an_hour_at_once() {
    let perf = steady();
    perf.tick(100);
    perf.tick(101);
    perf.set_recording(false);
    perf.set_recording(true);
    // Two hours later the first reading only primes, yet the old run is gone already.
    perf.tick(7300);
    assert!(!perf.has_samples());
    assert!(perf.at(Moment::Live).is_none());
}

#[test]
fn live_is_the_newest_sample_and_a_moment_its_second() {
    let perf = steady();
    for t in 100..=105 {
        perf.tick(t);
    }
    assert_eq!(perf.at(Moment::Live).unwrap().time, 105);
    assert_eq!(perf.at(Moment::At(103)).unwrap().time, 103);
    let cpu = perf.at(Moment::At(103)).unwrap().cpu.value().unwrap().busy.clone();
    assert_eq!(cpu, Reading::Value(0.3));
}

#[test]
fn the_first_sample_repaints_and_later_ones_only_while_watched() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let perf = Perf::manual(
        Box::new(Steady::new(1, 0)),
        Arc::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
        }),
    );
    for t in 1..=3 {
        perf.tick(t);
    }
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "only the first sample, which brings Performance into the rail"
    );
    perf.set_watched(true);
    perf.tick(4);
    perf.tick(5);
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[test]
fn an_hour_of_samples_fits_the_memory_budget() {
    // A large desktop: 32 logical CPUs, 8 disks.
    let perf = Perf::manual(Box::new(Steady::new(32, 8)), no_repaint());
    for t in 0..=3600 {
        perf.tick(t);
    }
    let bytes = perf.memory_bytes();
    assert!(bytes < 60 * 1024 * 1024, "an hour takes {bytes} bytes");
}
