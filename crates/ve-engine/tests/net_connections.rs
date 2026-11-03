//! Open connections through the `Net` handle (#52): the Linux backend over fixture
//! `/proc/net/{tcp,tcp6,udp,udp6}` tables and `/proc/<pid>/fd` links, recorded every two
//! seconds on the Performance Timeline, the top 200 kept and every one live.
#![cfg(unix)]

use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use ve_engine::net::{Connection, Holder, Net, Protocol, RETAINED, Socket, SocketHolder, State};
use ve_engine::perf::{Backend, Counters, LinuxBackend, Perf, ProcessCountersTable, ProcessId, Reading};
use ve_engine::timeline::Moment;

const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

fn put(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn user() -> u32 {
    // SAFETY: getuid only reads the calling process's user id.
    unsafe { libc::getuid() }
}

/// An address as the kernel prints it: each 32-bit word of the IP in the machine's byte order,
/// then the port, all in hex.
fn hex(addr: &str) -> String {
    let addr: SocketAddr = addr.parse().unwrap();
    let words: Vec<u32> = match addr.ip() {
        IpAddr::V4(ip) => vec![u32::from_ne_bytes(ip.octets())],
        IpAddr::V6(ip) => ip.octets().chunks(4).map(|c| u32::from_ne_bytes(c.try_into().unwrap())).collect(),
    };
    let ip: String = words.iter().map(|w| format!("{w:08X}")).collect();
    format!("{ip}:{:04X}", addr.port())
}

/// One socket of a table: local and remote address, state code, owning user, inode.
struct Row<'a>(&'a str, &'a str, u8, u32, u64);

/// `root/proc/net/<file>` holding `rows`.
fn table(root: &Path, file: &str, rows: &[Row]) {
    let text = rows
        .iter()
        .enumerate()
        .fold(HEADER.to_owned(), |t, (i, Row(local, remote, state, uid, inode))| {
            t + &format!(
                "{i:4}: {} {} {state:02X} 00000000:00000000 00:00000000 00000000 {uid:5} 0 {inode} 1 0000000000000000 20 4 30 10 -1\n",
                hex(local),
                hex(remote)
            )
        });
    put(root, &format!("proc/net/{file}"), &text);
}

/// `/proc/<pid>` of a process named `name` that started `start` ticks after boot, holding the
/// sockets `inodes` (between other descriptors).
fn process(root: &Path, pid: u32, name: &str, start: u64, inodes: &[u64]) {
    put(
        root,
        &format!("proc/{pid}/stat"),
        &format!("{pid} ({name}) S 1 1 1 0 -1 0 0 0 0 0 5 5 0 0 20 0 1 0 {start} 1000 100\n"),
    );
    let fd = root.join(format!("proc/{pid}/fd"));
    fs::create_dir_all(&fd).unwrap();
    std::os::unix::fs::symlink("/dev/null", fd.join("0")).unwrap();
    std::os::unix::fs::symlink("pipe:[77]", fd.join("1")).unwrap();
    for (n, inode) in inodes.iter().enumerate() {
        std::os::unix::fs::symlink(format!("socket:[{inode}]"), fd.join((n + 3).to_string())).unwrap();
    }
}

fn linux(root: &Path) -> (Perf, Net) {
    put(root, "proc/stat", "cpu  1 0 1 1 0 0 0 0 0 0\nbtime 1700000000\n");
    let perf = Perf::manual(Box::new(LinuxBackend::new(root)), Arc::new(|| {}));
    let net = Net::new(&perf);
    (perf, net)
}

fn connections(net: &Net, moment: Moment) -> Arc<ve_engine::net::Connections> {
    let sample = net.at(moment).expect("a sample");
    sample.connections.value().expect("connections").clone()
}

fn find<'a>(list: &'a [Arc<Connection>], local: &str) -> &'a Connection {
    let local: SocketAddr = local.parse().unwrap();
    list.iter()
        .find(|c| c.local == local)
        .unwrap_or_else(|| panic!("no connection from {local}"))
}

#[test]
fn v4_and_v6_sockets_read_their_addresses_state_and_process() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let me = user();
    table(root, "tcp", &[Row("192.168.1.10:51000", "93.184.216.34:443", 0x01, me, 1001)]);
    table(root, "tcp6", &[Row("[::]:22", "[::]:0", 0x0A, me, 1002)]);
    table(root, "udp", &[Row("127.0.0.53:53", "0.0.0.0:0", 0x07, me, 1003)]);
    table(root, "udp6", &[Row("[2001:db8::1]:5353", "[2001:db8::2]:443", 0x01, me, 1004)]);
    process(root, 500, "curl", 9000, &[1001, 1003]);
    process(root, 600, "sshd", 40, &[1004, 1002]);
    let (perf, net) = linux(root);
    perf.tick(100);
    perf.tick(101);

    let live = connections(&net, Moment::Live);
    let curl = Holder::Process {
        id: ProcessId { pid: 500, start: 9000 },
        name: "curl".into(),
    };
    let sshd = Holder::Process {
        id: ProcessId { pid: 600, start: 40 },
        name: "sshd".into(),
    };

    let web = find(&live.list, "192.168.1.10:51000");
    assert_eq!(
        (web.protocol, web.remote, web.state, &web.holder),
        (Protocol::Tcp, "93.184.216.34:443".parse().unwrap(), State::Established, &curl)
    );
    assert!(web.has_remote());
    let ssh = find(&live.list, "[::]:22");
    assert_eq!((ssh.protocol, ssh.state, &ssh.holder), (Protocol::Tcp, State::Listen, &sshd));
    assert!(!ssh.has_remote());
    assert_eq!(ssh.state.label(ssh.protocol), "listening");
    let dns = find(&live.list, "127.0.0.53:53");
    assert_eq!(
        (dns.protocol, dns.state.label(dns.protocol), &dns.holder),
        (Protocol::Udp, "open", &curl)
    );
    let mdns = find(&live.list, "[2001:db8::1]:5353");
    assert_eq!(
        (mdns.remote, mdns.state.label(mdns.protocol), &mdns.holder),
        ("[2001:db8::2]:443".parse().unwrap(), "connected", &sshd)
    );
    // Open conversations first.
    assert_eq!(live.list.len(), 4);
    assert!(live.list[..2].iter().all(|c| c.state == State::Established));
    assert_eq!(net.at(Moment::At(101)).unwrap().connections, Reading::Value(live.clone()));
}

#[test]
fn a_process_that_cannot_be_read_holds_an_unknown_socket() {
    let me = user();
    if me == 0 {
        // Root reads every descriptor: nothing is unreadable to it.
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    table(
        root,
        "tcp",
        &[
            // Held by a process whose descriptors cannot be read.
            Row("10.0.0.2:40000", "10.0.0.9:443", 0x01, me, 2001),
            // Another user's socket: their process, not searched.
            Row("10.0.0.2:40001", "10.0.0.9:443", 0x01, me + 1, 2002),
            // Held by no process found.
            Row("10.0.0.2:40002", "10.0.0.9:443", 0x01, me, 2003),
            // Closed, the kernel finishing it: no inode, no process.
            Row("10.0.0.2:40003", "10.0.0.9:443", 0x06, me, 0),
        ],
    );
    process(root, 700, "locked", 10, &[2001, 2002]);
    let fd = root.join("proc/700/fd");
    fs::set_permissions(&fd, fs::Permissions::from_mode(0o000)).unwrap();
    let (perf, net) = linux(root);
    perf.tick(100);
    perf.tick(101);
    fs::set_permissions(&fd, fs::Permissions::from_mode(0o755)).unwrap();

    let live = connections(&net, Moment::Live);
    let holder = |local: &str| find(&live.list, local).holder.clone();
    assert_eq!(holder("10.0.0.2:40000"), Holder::Unknown);
    assert_eq!(holder("10.0.0.2:40001"), Holder::Unknown);
    assert_eq!(holder("10.0.0.2:40002"), Holder::Unknown);
    assert_eq!(holder("10.0.0.2:40003"), Holder::Nobody);
    assert_eq!(find(&live.list, "10.0.0.2:40003").state.label(Protocol::Tcp), "time wait");
}

#[test]
fn a_sample_keeps_the_top_200_and_live_lists_every_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let me = user();
    // 230 listening ports first in the table, then 30 open conversations.
    let listening = (0..230).map(|i| (format!("0.0.0.0:{}", 20000 + i), "0.0.0.0:0".to_owned(), 0x0A));
    let talking = (0..30).map(|i| (format!("10.0.0.2:{}", 40000 + i), "10.0.0.9:443".to_owned(), 0x01));
    let rows: Vec<(String, String, u8)> = listening.chain(talking).collect();
    let rows: Vec<Row> = rows.iter().map(|(l, r, s)| Row(l, r, *s, me, 0)).collect();
    table(root, "tcp", &rows);
    let (perf, net) = linux(root);
    perf.tick(100);
    perf.tick(101);
    perf.tick(102);

    assert_eq!(connections(&net, Moment::Live).list.len(), 260, "the live view is not capped");
    let kept = connections(&net, Moment::At(101));
    assert_eq!(kept.list.len(), RETAINED);
    assert_eq!(
        kept.list.iter().filter(|c| c.state == State::Established).count(),
        30,
        "every open conversation is kept before a listening port"
    );
    // The second between two readings shows the one before it, the same table.
    assert!(Arc::ptr_eq(&kept, &connections(&net, Moment::At(102))));
    assert_eq!(net.summary(0, 200).connections, Some(200));
}

#[test]
fn connections_are_read_every_two_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let me = user();
    let (perf, net) = linux(root);
    let mut opened = Vec::new();
    for step in 0..=4u16 {
        opened.push(format!("10.0.0.2:{}", 40000 + step));
        let rows: Vec<Row> = opened.iter().map(|l| Row(l, "10.0.0.9:443", 0x01, me, 0)).collect();
        table(root, "tcp", &rows);
        perf.tick(100 + i64::from(step));
    }
    let counts: Vec<usize> = (101..=104).map(|t| connections(&net, Moment::At(t)).list.len()).collect();
    // Read at 100 and 101, then every two seconds, shown until the next.
    assert_eq!(counts, vec![2, 2, 4, 4]);
    assert_eq!(connections(&net, Moment::Live).list.len(), 4);
    // Every connection open at some kept second, each once.
    assert_eq!(net.summary(0, 200).connections, Some(4));
    assert_eq!(net.summary(101, 102).connections, Some(2));
}

#[test]
fn no_socket_tables_are_unsupported_not_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (perf, net) = linux(root);
    perf.tick(100);
    perf.tick(101);
    assert_eq!(net.at(Moment::At(101)).unwrap().connections, Reading::Unsupported);
    assert_eq!(net.live().unwrap().connections, Reading::Unsupported);
    assert_eq!(net.summary(0, 200).connections, None);
}

/// A backend with `count` open connections, `churn` of which are replaced at every reading.
struct Busy {
    readings: u16,
    count: u16,
    churn: u16,
}

impl Backend for Busy {
    fn read(&mut self) -> Counters {
        Counters {
            cpu: Reading::Unsupported,
            core_kinds: Arc::from([]),
            memory: Reading::Unsupported,
            disks: Reading::Unsupported,
            network: Reading::Unsupported,
        }
    }

    /// An empty table: connections are read with the process table, every two seconds.
    fn processes(&mut self, _: bool) -> Reading<ProcessCountersTable> {
        Reading::Value(ProcessCountersTable {
            processes: Vec::new(),
            boot_time: None,
            ticks_per_second: 100,
        })
    }

    fn connections(&mut self) -> Reading<Vec<Socket>> {
        self.readings = self.readings.wrapping_add(1);
        let first = self.readings.wrapping_mul(self.churn);
        let socket = |port: u16| Socket {
            protocol: Protocol::Tcp,
            local: SocketAddr::from(([10, 0, 0, 2], port)),
            remote: SocketAddr::from(([10, 0, 0, 9], 443)),
            state: State::Established,
            holder: SocketHolder::Unknown,
        };
        // Long-lived ones, then the newest of a stream of short ones, which sort first: every
        // kept table differs from the one before.
        let lasting = (0..self.count - self.churn).map(|i| socket(40_000 + i));
        let short = (0..self.churn).map(|i| socket(1 + first.wrapping_add(i) % 30_000));
        Reading::Value(lasting.chain(short).collect())
    }
}

#[test]
fn an_hour_of_busy_connections_fits_the_memory_budget() {
    let perf = Perf::manual(
        Box::new(Busy {
            readings: 0,
            count: 600,
            churn: 20,
        }),
        Arc::new(|| {}),
    );
    let net = Net::new(&perf);
    for t in 0..=3600 {
        perf.tick(t);
    }
    assert_eq!(connections(&net, Moment::At(3000)).list.len(), RETAINED);
    let bytes = perf.memory_bytes();
    assert!(bytes < 12 * 1024 * 1024, "an hour takes {bytes} bytes");
}
