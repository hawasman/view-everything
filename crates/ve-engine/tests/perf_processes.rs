//! The process table through the `Perf` handle (#50): the Linux backend over fixture `/proc`
//! trees, identity across a recycled PID, I/O only while watched, the kept top of each sample
//! and the details the Inspector shows.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use ve_engine::net::InterfaceCounters;
use ve_engine::perf::{
    Backend, Counters, CpuCounters, CpuTimes, Io, LinuxBackend, Perf, ProcessCounters, ProcessCountersTable, ProcessId, RETAINED, Reading,
};
use ve_engine::timeline::Moment;

fn put(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// `/proc/stat` `step` seconds in: the machine's CPUs count 200 ticks a second.
fn machine(root: &Path, step: u64) {
    let busy = 1000 + 100 * step;
    put(
        root,
        "proc/stat",
        &format!("cpu  {busy} 0 0 {busy} 0 0 0 0 0 0\ncpu0 {busy} 0 0 {busy} 0 0 0 0 0 0\nbtime 1700000000\n"),
    );
}

/// A process's identity in the fixture: PID, name, parent and start time in ticks after boot.
struct Proc<'a>(u32, &'a str, u32, u64);

/// `/proc/<pid>` of `p` having used `ticks` of CPU, with `pages` resident and the given
/// storage I/O.
fn process(root: &Path, p: &Proc, ticks: u64, pages: u64, io: (u64, u64)) {
    let Proc(pid, name, ppid, start) = *p;
    let (utime, stime) = (ticks / 2, ticks - ticks / 2);
    put(
        root,
        &format!("proc/{pid}/stat"),
        &format!("{pid} ({name}) S {ppid} 1 1 0 -1 0 0 0 0 0 {utime} {stime} 0 0 20 0 1 0 {start} 1000 {pages}\n"),
    );
    put(root, &format!("proc/{pid}/statm"), &format!("2000 {pages} 100 1 0 50 0\n"));
    put(
        root,
        &format!("proc/{pid}/io"),
        &format!(
            "rchar: 1\nwchar: 1\nsyscr: 1\nsyscw: 1\nread_bytes: {}\nwrite_bytes: {}\ncancelled_write_bytes: 0\n",
            io.0, io.1
        ),
    );
}

fn no_repaint() -> ve_engine::Repaint {
    Arc::new(|| {})
}

fn linux(root: &Path) -> Perf {
    Perf::manual(Box::new(LinuxBackend::new(root)), no_repaint())
}

fn page_size() -> u64 {
    // SAFETY: sysconf only reads a system constant.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap()
}

#[test]
fn a_process_reads_its_cpu_share_memory_and_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    machine(root, 0);
    process(root, &Proc(1, "systemd", 0, 5), 300, 100, (0, 0));
    // A name holding spaces and parentheses runs to the last `)`.
    let web = Proc(4242, "Web Content (x)", 1, 9000);
    process(root, &web, 1000, 2500, (0, 0));
    let perf = linux(root);
    perf.tick(100);
    machine(root, 1);
    process(root, &web, 1050, 2500, (0, 0));
    perf.tick(101);

    let table = perf.processes(Moment::Live).expect("a live table");
    let p = table.get(ProcessId { pid: 4242, start: 9000 }).unwrap();
    assert_eq!(&*p.name, "Web Content (x)");
    // 50 of the machine's 200 ticks.
    assert_eq!(p.cpu, Reading::Value(0.25));
    assert_eq!(p.rss, Reading::Value(2500 * page_size()));
    assert_eq!(p.ppid, 1);
    assert_eq!(table.by_pid(p.ppid).map(|parent| &*parent.name), Some("systemd"));
    assert_eq!(table.cpu_seconds(p), 10.5);
    let ticks = table.ticks_per_second as i64;
    assert_eq!(table.started(p), Some(1_700_000_000 + 9000 / ticks));
    let idle = table.get(ProcessId { pid: 1, start: 5 }).unwrap();
    assert_eq!(idle.cpu, Reading::Value(0.));
}

#[test]
fn a_recycled_pid_is_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    machine(root, 0);
    let old_process = Proc(77, "old", 1, 1000);
    process(root, &old_process, 5000, 10, (0, 0));
    let perf = linux(root);
    perf.tick(100);
    machine(root, 1);
    process(root, &old_process, 5100, 10, (0, 0));
    perf.tick(101);
    // PID 77 exits and a new process takes it, having used fewer ticks than the old one.
    machine(root, 3);
    process(root, &Proc(77, "new", 1, 8000), 40, 10, (0, 0));
    perf.tick(103);

    let old = ProcessId { pid: 77, start: 1000 };
    let new = ProcessId { pid: 77, start: 8000 };
    let then = perf.processes(Moment::At(101)).unwrap();
    assert_eq!(then.get(old).unwrap().cpu, Reading::Value(0.5));
    assert!(then.get(new).is_none());

    let now = perf.processes(Moment::At(103)).unwrap();
    assert!(now.get(old).is_none(), "the old process is gone");
    let p = now.get(new).unwrap();
    assert_eq!(&*p.name, "new");
    // Not a share taken against the old process's ticks: it was not running before.
    assert_eq!(p.cpu, Reading::Unsupported);
    assert!(perf.details(old).is_none(), "the old process has exited");
}

#[test]
fn io_is_read_only_while_a_client_watches() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    machine(root, 0);
    let writer = Proc(10, "writer", 1, 50);
    process(root, &writer, 0, 1, (0, 0));
    let perf = linux(root);
    let io = |perf: &Perf, t| {
        perf.processes(Moment::At(t))
            .unwrap()
            .get(ProcessId { pid: 10, start: 50 })
            .unwrap()
            .io
            .clone()
    };

    perf.tick(100);
    machine(root, 1);
    process(root, &writer, 0, 1, (4096, 8192));
    perf.tick(101);
    assert_eq!(io(&perf, 101), Reading::Unsupported, "nobody watches");

    perf.set_process_io(true);
    machine(root, 3);
    process(root, &writer, 0, 1, (8192, 16384));
    perf.tick(103);
    // The reading before was taken unwatched: there is nothing to take a rate against yet.
    assert_eq!(io(&perf, 103), Reading::Unsupported);
    machine(root, 5);
    process(root, &writer, 0, 1, (12288, 32768));
    perf.tick(105);
    assert_eq!(io(&perf, 105), Reading::Value(Io { read: 2048, write: 8192 }));

    perf.set_process_io(false);
    machine(root, 7);
    perf.tick(107);
    assert_eq!(io(&perf, 107), Reading::Unsupported, "watching stopped");
}

#[test]
fn the_process_table_is_read_every_two_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    machine(root, 0);
    process(root, &Proc(10, "a", 1, 50), 0, 1, (0, 0));
    let perf = linux(root);
    for step in 0..=4 {
        machine(root, step);
        perf.tick(100 + step as i64);
    }
    let times: Vec<i64> = (101..=104).map(|t| perf.processes(Moment::At(t)).unwrap().time).collect();
    // Primed at 100; the first table at 101, then every two seconds, shown until the next.
    assert_eq!(times, vec![101, 101, 103, 103]);
}

#[test]
fn details_are_the_command_line_and_user_or_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    machine(root, 0);
    process(root, &Proc(300, "editor", 1, 70), 0, 1, (0, 0));
    put(root, "proc/300/cmdline", "/usr/bin/editor\0--new-window\0notes.txt\0");
    process(root, &Proc(2, "kthreadd", 0, 1), 0, 0, (0, 0));
    put(root, "proc/2/cmdline", "");
    // SAFETY: getuid only reads the calling process's user id.
    let uid = unsafe { libc::getuid() };
    put(
        root,
        "etc/passwd",
        &format!("root:x:0:0::/root:/bin/sh\nalice:x:{uid}:{uid}::/home/alice:/bin/sh\n"),
    );
    let perf = linux(root);

    let editor = perf.details(ProcessId { pid: 300, start: 70 }).unwrap();
    assert_eq!(editor.command, Reading::Value("/usr/bin/editor --new-window notes.txt".into()));
    assert_eq!(editor.user, Reading::Value("alice".into()));
    // A kernel thread has no command line: unknown, not empty.
    let kernel = perf.details(ProcessId { pid: 2, start: 1 }).unwrap();
    assert_eq!(kernel.command, Reading::Unsupported);
    // Another start time at that PID: not this process.
    assert!(perf.details(ProcessId { pid: 300, start: 71 }).is_none());

    fs::remove_file(root.join("etc/passwd")).unwrap();
    let unnamed = perf.details(ProcessId { pid: 300, start: 70 }).unwrap();
    assert_eq!(unnamed.user, Reading::Value(format!("uid {uid}")));
}

/// A backend with `count` processes, each a little busier and larger than the one before.
struct Many {
    readings: u64,
    count: u32,
    names: Vec<Arc<str>>,
    interfaces: Vec<Arc<str>>,
}

impl Many {
    fn new(count: u32) -> Self {
        Self {
            readings: 0,
            count,
            names: (0..count).map(|i| Arc::from(format!("worker-{i}"))).collect(),
            interfaces: (0..8).map(|i| Arc::from(format!("veth{i}"))).collect(),
        }
    }
}

impl Backend for Many {
    fn read(&mut self) -> Counters {
        self.readings += 1;
        let n = self.readings;
        let times = CpuTimes {
            busy: 30 * n,
            total: 1_000_000 * n,
        };
        Counters {
            cpu: Reading::Value(CpuCounters {
                total: times,
                cores: (0..32).map(|id| (id, times)).collect(),
            }),
            core_kinds: Arc::from([]),
            memory: Reading::Unsupported,
            disks: Reading::Unsupported,
            // A busy machine's worth of interfaces, for the memory budget.
            network: Reading::Value(
                self.interfaces
                    .iter()
                    .map(|name| InterfaceCounters {
                        name: name.clone(),
                        received: 1000 * n,
                        sent: 100 * n,
                    })
                    .collect(),
            ),
        }
    }

    fn processes(&mut self, _: bool) -> Reading<ProcessCountersTable> {
        let n = self.readings;
        Reading::Value(ProcessCountersTable {
            processes: (0..self.count)
                .map(|i| ProcessCounters {
                    id: ProcessId {
                        pid: i + 1,
                        start: u64::from(i),
                    },
                    ppid: 1,
                    name: self.names[i as usize].clone(),
                    // CPU grows with the PID; memory shrinks with it.
                    cpu_ticks: u64::from(i) * n,
                    rss: Some(u64::from(self.count - i) * 4096),
                    io: None,
                })
                .collect(),
            boot_time: Some(1_700_000_000),
            ticks_per_second: 100,
        })
    }
}

#[test]
fn a_sample_keeps_the_top_processes_and_live_lists_every_one() {
    let perf = Perf::manual(Box::new(Many::new(450)), no_repaint());
    perf.tick(10);
    perf.tick(11);

    assert_eq!(perf.processes(Moment::Live).unwrap().list.len(), 450, "the live view is not capped");
    let kept = perf.processes(Moment::At(11)).unwrap();
    assert_eq!(kept.list.len(), RETAINED);
    let pids: Vec<u32> = kept.list.iter().map(|p| p.id.pid).collect();
    // The 100 busiest (the highest PIDs) and the 100 largest (the lowest).
    let expected: Vec<u32> = (1..=100).chain(351..=450).collect();
    assert_eq!(pids, expected);
}

#[test]
fn an_hour_with_processes_fits_the_memory_budget() {
    let perf = Perf::manual(Box::new(Many::new(600)), no_repaint());
    for t in 0..=3600 {
        perf.tick(t);
    }
    let bytes = perf.memory_bytes();
    assert!(bytes < 60 * 1024 * 1024, "an hour takes {bytes} bytes");
}
