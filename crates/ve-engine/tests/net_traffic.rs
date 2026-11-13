//! TCP traffic per Process through the `Net` handle (#71, #72): each socket's kernel byte
//! counts, read with the connections every two seconds, summed per Process holding it, and
//! kept on the Performance Timeline.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use ve_engine::net::{Net, ProcessTraffic, Protocol, RETAINED, Socket, SocketHolder, State, TcpBytes, Traffic};
use ve_engine::perf::{Backend, Counters, Perf, ProcessCounters, ProcessCountersTable, ProcessId, Reading};
use ve_engine::timeline::Moment;

/// A TCP socket `inode` held by `holder`, its cumulative bytes received and sent.
fn tcp(inode: u64, holder: SocketHolder, received: u64, sent: u64) -> Socket {
    Socket {
        protocol: Protocol::Tcp,
        local: SocketAddr::from(([10, 0, 0, 2], 40_000 + (inode % 20_000) as u16)),
        remote: SocketAddr::from(([10, 0, 0, 9], 443)),
        state: State::Established,
        holder,
        inode,
        bytes: Some(TcpBytes { received, sent }),
    }
}

fn pid(pid: u32) -> SocketHolder {
    SocketHolder::Pid(pid)
}

/// A backend whose readings of the sockets are `readings`, one per reading (the last repeats),
/// and whose process table is `processes` (PID and name; each started at tick PID).
struct Scripted {
    readings: VecDeque<Vec<Socket>>,
    processes: Vec<(u32, &'static str)>,
}

impl Backend for Scripted {
    fn read(&mut self) -> Counters {
        Counters {
            cpu: Reading::Unsupported,
            core_kinds: Arc::from([]),
            memory: Reading::Unsupported,
            disks: Reading::Unsupported,
            network: Reading::Unsupported,
        }
    }

    fn processes(&mut self, _: bool) -> Reading<ProcessCountersTable> {
        Reading::Value(ProcessCountersTable {
            processes: self
                .processes
                .iter()
                .map(|&(pid, name)| ProcessCounters {
                    id: id(pid),
                    ppid: 1,
                    name: name.into(),
                    cpu_ticks: 0,
                    rss: None,
                    io: None,
                })
                .collect(),
            boot_time: None,
            ticks_per_second: 100,
        })
    }

    fn connections(&mut self) -> Reading<Vec<Socket>> {
        let reading = if self.readings.len() > 1 {
            self.readings.pop_front().unwrap()
        } else {
            self.readings[0].clone()
        };
        Reading::Value(reading)
    }
}

fn id(pid: u32) -> ProcessId {
    ProcessId {
        pid,
        start: u64::from(pid),
    }
}

/// A recorder over `readings`. Connections are read at the first two ticks, then every two
/// seconds: ticks 100, 101, 103, 105, ... take readings 0, 1, 2, 3, ...
fn recorder(processes: Vec<(u32, &'static str)>, readings: Vec<Vec<Socket>>) -> (Perf, Net) {
    let perf = Perf::manual(
        Box::new(Scripted {
            readings: readings.into(),
            processes,
        }),
        Arc::new(|| {}),
    );
    let net = Net::new(&perf);
    (perf, net)
}

fn traffic(net: &Net, moment: Moment) -> Arc<Traffic> {
    net.at(moment).expect("a sample").traffic.value().expect("traffic").clone()
}

/// Each Process's name, bytes received and bytes sent, in the table's order.
fn rows(traffic: &Traffic) -> Vec<(&str, u64, u64)> {
    traffic.processes.iter().map(|p| (&*p.name, p.received, p.sent)).collect()
}

#[test]
fn a_processs_rates_sum_its_sockets_and_a_closed_socket_never_goes_back() {
    let (perf, net) = recorder(
        vec![(10, "firefox"), (20, "curl")],
        vec![
            vec![tcp(1, pid(10), 1_000, 100), tcp(2, pid(10), 0, 0), tcp(3, pid(20), 50_000, 50_000)],
            // curl's socket closed and a new one opened.
            vec![tcp(1, pid(10), 3_000, 300), tcp(2, pid(10), 400, 0), tcp(4, pid(20), 10, 0)],
            vec![tcp(1, pid(10), 5_000, 300), tcp(4, pid(20), 10, 0)],
        ],
    );
    for t in [100, 101, 102, 103] {
        perf.tick(t);
    }

    // What each moved from the reading at 100 to the one at 101: both of firefox's sockets,
    // all of curl's new one and nothing of its closed one.
    let at_101 = traffic(&net, Moment::At(101));
    assert_eq!(at_101.seconds, 1);
    assert_eq!(rows(&at_101), vec![("firefox", 2_400, 200), ("curl", 10, 0)]);
    // Shown until the next reading.
    assert!(Arc::ptr_eq(&at_101, &traffic(&net, Moment::At(102))));

    // From 101 to 103: two seconds, firefox received 2 000 bytes.
    let live = traffic(&net, Moment::Live);
    assert_eq!(rows(&live), vec![("firefox", 2_000, 0), ("curl", 0, 0)]);
    let firefox = live.get(id(10)).unwrap();
    assert_eq!((live.receive_rate(firefox), live.send_rate(firefox)), (1_000, 0));
}

#[test]
fn a_socket_that_moves_to_another_process_counts_for_the_new_one_from_then() {
    let (perf, net) = recorder(
        vec![(10, "server"), (11, "worker")],
        vec![
            vec![tcp(7, pid(10), 1_000, 1_000)],
            // Handed over: the worker holds it now and moved 600 more bytes on it.
            vec![tcp(7, pid(11), 1_600, 1_000)],
        ],
    );
    for t in [100, 101] {
        perf.tick(t);
    }
    let live = traffic(&net, Moment::Live);
    assert_eq!(rows(&live), vec![("worker", 600, 0)], "the server holds no socket now");
}

#[test]
fn a_socket_of_an_unreadable_process_counts_for_no_one() {
    let (perf, net) = recorder(
        vec![(10, "firefox")],
        vec![
            vec![tcp(1, SocketHolder::Unknown, 0, 0), tcp(2, pid(10), 0, 0), tcp(3, pid(99), 0, 0)],
            // 99 has ended: it is not in the process table read with the sockets.
            vec![tcp(1, SocketHolder::Unknown, 9_000, 9_000), tcp(2, pid(10), 500, 5), tcp(3, pid(99), 7, 7)],
        ],
    );
    for t in [100, 101] {
        perf.tick(t);
    }
    assert_eq!(rows(&traffic(&net, Moment::Live)), vec![("firefox", 500, 5)]);
}

#[test]
fn no_socket_counts_are_unsupported_not_zero() {
    let mut uncounted = tcp(1, pid(10), 0, 0);
    uncounted.bytes = None;
    let (perf, net) = recorder(vec![(10, "firefox")], vec![vec![uncounted]]);
    for t in [100, 101, 102, 103] {
        perf.tick(t);
    }
    assert_eq!(net.live().unwrap().traffic, Reading::Unsupported);
    assert_eq!(net.at(Moment::At(103)).unwrap().traffic, Reading::Unsupported);
    assert_eq!(net.traffic(0, 200), None);
    assert_eq!(net.summary(0, 200).top_process(), None);
}

#[test]
fn a_sample_keeps_the_processes_that_moved_the_most_and_live_lists_every_one() {
    // 230 Processes, the higher the PID the more bytes, and one idle.
    let mut processes: Vec<(u32, &'static str)> = (1..=230).map(|p| (p, "busy")).collect();
    processes.push((1_000, "idle"));
    let reading = |scale: u64| -> Vec<Socket> {
        let mut sockets: Vec<Socket> = (1..=230u32).map(|p| tcp(u64::from(p), pid(p), u64::from(p) * scale, 0)).collect();
        sockets.push(tcp(5_000, pid(1_000), 0, 0));
        sockets
    };
    let (perf, net) = recorder(processes, vec![reading(0), reading(10)]);
    for t in [100, 101] {
        perf.tick(t);
    }

    let live = traffic(&net, Moment::Live);
    assert_eq!(live.processes.len(), 231, "the live view is not capped");
    let kept = traffic(&net, Moment::At(101));
    assert_eq!(kept.processes.len(), RETAINED);
    assert_eq!(kept.processes.first().map(|p| p.id.pid), Some(230));
    assert_eq!(kept.processes.last().map(|p| p.id.pid), Some(31), "the top by bytes");
}

#[test]
fn the_stretch_names_the_process_that_moved_the_most_bytes() {
    let (perf, net) = recorder(
        vec![(10, "firefox"), (20, "rsync")],
        vec![
            vec![tcp(1, pid(10), 0, 0), tcp(2, pid(20), 0, 0)],
            // firefox bursts once; rsync moves steadily and moves more over the stretch.
            vec![tcp(1, pid(10), 5_000, 0), tcp(2, pid(20), 0, 2_000)],
            vec![tcp(1, pid(10), 5_000, 0), tcp(2, pid(20), 0, 4_000)],
            vec![tcp(1, pid(10), 5_000, 0), tcp(2, pid(20), 0, 6_000)],
        ],
    );
    for t in 100..=105 {
        perf.tick(t);
    }

    let firefox = ProcessTraffic {
        id: id(10),
        name: "firefox".into(),
        received: 5_000,
        sent: 0,
    };
    let rsync = ProcessTraffic {
        id: id(20),
        name: "rsync".into(),
        received: 0,
        sent: 6_000,
    };
    assert_eq!(net.traffic(0, 200), Some(vec![rsync.clone(), firefox.clone()]));
    assert_eq!(net.summary(0, 200).top_process(), Some(&rsync));
    // Up to 102, only firefox's burst and rsync's first 2 000 bytes were recorded.
    assert_eq!(net.summary(0, 102).top_process(), Some(&firefox));
    // A past Moment shows that second's traffic.
    assert_eq!(rows(&traffic(&net, Moment::At(104))), vec![("rsync", 0, 2_000)], "a sample keeps the Processes that moved bytes");
}

#[test]
fn a_connection_to_the_machine_itself_is_no_network_traffic() {
    let to = |mut socket: Socket, remote: &str| {
        socket.remote = remote.parse().unwrap();
        socket
    };
    let (perf, net) = recorder(
        vec![(10, "firefox"), (20, "steam")],
        vec![
            vec![
                tcp(1, pid(10), 0, 0),
                to(tcp(2, pid(20), 0, 0), "127.0.0.1:4000"),
                to(tcp(3, pid(20), 0, 0), "[::ffff:127.0.0.1]:4000"),
            ],
            vec![
                tcp(1, pid(10), 300, 30),
                to(tcp(2, pid(20), 9_000, 9_000), "127.0.0.1:4000"),
                to(tcp(3, pid(20), 9_000, 9_000), "[::ffff:127.0.0.1]:4000"),
            ],
        ],
    );
    for t in [100, 101] {
        perf.tick(t);
    }
    assert_eq!(rows(&traffic(&net, Moment::Live)), vec![("firefox", 300, 30)]);
}
