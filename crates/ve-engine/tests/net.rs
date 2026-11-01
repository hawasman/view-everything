//! The Network Module through its `Net` handle (#51): the Linux backend over fixture
//! `/proc/net/dev` files, recorded on the Performance Timeline and read at a Moment.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use ve_engine::net::Net;
use ve_engine::perf::{LinuxBackend, Perf, Reading};
use ve_engine::timeline::Moment;

const HEADER: &str = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
";

/// A `/proc/net/dev` line: bytes received in the first field, sent in the ninth.
fn line(name: &str, received: u64, sent: u64) -> String {
    format!("{name:>6}: {received} 10 0 0 0 0 0 0 {sent} 20 0 0 0 0 0 0\n")
}

fn dev(root: &Path, lines: &[(&str, u64, u64)]) {
    let text = lines.iter().fold(HEADER.to_owned(), |t, &(n, r, s)| t + &line(n, r, s));
    let path = root.join("proc/net/dev");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn linux(root: &Path) -> (Perf, Net) {
    let perf = Perf::manual(Box::new(LinuxBackend::new(root)), Arc::new(|| {}));
    let net = Net::new(&perf);
    (perf, net)
}

/// (name, receive rate, send rate) of each interface at `moment`.
fn rates(net: &Net, moment: Moment) -> Vec<(String, Reading<u64>, Reading<u64>)> {
    let sample = net.at(moment).expect("a sample");
    let interfaces = sample.interfaces.value().expect("interfaces");
    interfaces
        .iter()
        .map(|i| (i.name.to_string(), i.receive_rate(), i.send_rate()))
        .collect()
}

#[test]
fn each_interface_reads_its_throughput_and_loopback_is_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    dev(root, &[("lo", 0, 0), ("eth0", 1000, 500), ("wlp3s0", 0, 0)]);
    let (perf, net) = linux(root);
    perf.tick(100);
    dev(root, &[("lo", 9999, 9999), ("eth0", 1000 + 2048, 500 + 512), ("wlp3s0", 0, 0)]);
    perf.tick(101);

    assert_eq!(
        rates(&net, Moment::At(101)),
        vec![
            ("eth0".to_owned(), Reading::Value(2048), Reading::Value(512)),
            ("wlp3s0".to_owned(), Reading::Value(0), Reading::Value(0)),
        ]
    );
    assert_eq!(net.live().unwrap().time, 101);
    assert!(net.has_samples());
}

#[test]
fn a_step_of_two_seconds_is_a_rate_per_second() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    dev(root, &[("eth0", 0, 0)]);
    let (perf, net) = linux(root);
    perf.tick(100);
    dev(root, &[("eth0", 4000, 1000)]);
    perf.tick(102);
    assert_eq!(
        rates(&net, Moment::At(102)),
        vec![("eth0".to_owned(), Reading::Value(2000), Reading::Value(500))]
    );
    // The hour's totals are the bytes moved, not the rate.
    let summary = net.summary(0, 200);
    assert_eq!((summary.interfaces[0].received, summary.interfaces[0].sent), (4000, 1000));
}

#[test]
fn a_32_bit_counter_that_wraps_still_gives_the_rate() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let limit = u64::from(u32::MAX);
    dev(root, &[("eth0", limit - 99, limit - 9)]);
    let (perf, net) = linux(root);
    perf.tick(100);
    dev(root, &[("eth0", 100, 10)]);
    perf.tick(101);
    assert_eq!(
        rates(&net, Moment::At(101)),
        vec![("eth0".to_owned(), Reading::Value(200), Reading::Value(20))]
    );
}

#[test]
fn a_counter_reset_is_unknown_for_that_second() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A 64-bit counter well past 32 bits going back is a reset (the interface came back), not a wrap.
    dev(root, &[("eth0", 1 << 40, 1 << 40)]);
    let (perf, net) = linux(root);
    perf.tick(100);
    dev(root, &[("eth0", 5, 5)]);
    perf.tick(101);
    assert_eq!(
        rates(&net, Moment::At(101)),
        vec![("eth0".to_owned(), Reading::Unsupported, Reading::Unsupported)]
    );
}

#[test]
fn an_interface_that_goes_away_keeps_its_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    dev(root, &[("eth0", 0, 0), ("tun0", 0, 0)]);
    let (perf, net) = linux(root);
    perf.tick(100);
    dev(root, &[("eth0", 100, 10), ("tun0", 7000, 3000)]);
    perf.tick(101);
    dev(root, &[("eth0", 200, 20)]);
    perf.tick(102);
    // A new interface has no rate until its second reading.
    dev(root, &[("eth0", 300, 30), ("usb0", 50, 50)]);
    perf.tick(103);

    let names = |t| rates(&net, Moment::At(t)).into_iter().map(|r| r.0).collect::<Vec<_>>();
    assert_eq!(names(101), vec!["eth0", "tun0"], "the past second still has it");
    assert_eq!(names(102), vec!["eth0"]);
    assert_eq!(
        rates(&net, Moment::At(103))[1],
        ("usb0".to_owned(), Reading::Unsupported, Reading::Unsupported)
    );

    let hour = net.summary(0, 103);
    assert_eq!(hour.seconds, 3);
    let tun = hour.interfaces.iter().find(|i| &*i.name == "tun0").expect("tun0 kept");
    assert_eq!((tun.received, tun.sent, tun.last_seen), (7000, 3000, 101));
    let names: Vec<&str> = hour.interfaces.iter().map(|i| &*i.name).collect();
    assert_eq!(names, vec!["eth0", "tun0", "usb0"]);
    assert_eq!(hour.busiest().map(|i| &*i.name), Some("tun0"));
}

#[test]
fn no_net_dev_is_unsupported_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Something else is readable, so seconds are recorded.
    let stat = root.join("proc/stat");
    fs::create_dir_all(stat.parent().unwrap()).unwrap();
    fs::write(stat, "cpu  1 0 1 1 0 0 0 0 0 0\n").unwrap();
    let (perf, net) = linux(root);
    perf.tick(100);
    perf.tick(101);
    assert_eq!(net.at(Moment::At(101)).unwrap().interfaces, Reading::Unsupported);
    assert!(!net.has_samples(), "the rail shows Network only with interfaces");
    assert_eq!(net.summary(0, 101).seconds, 0);
}

#[test]
fn a_moment_with_no_sample_is_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    dev(root, &[("eth0", 0, 0)]);
    let (perf, net) = linux(root);
    assert!(net.live().is_none());
    assert!(!net.has_samples());
    perf.tick(100);
    perf.tick(101);
    assert!(net.at(Moment::At(100)).is_none(), "the first reading only primes the next");
    assert!(net.at(Moment::At(5000)).is_none());
    assert_eq!(net.count(0, 200), 1);
    let peaks = net.peaks(100, 101, 2);
    assert_eq!(peaks, vec![None, Some(0)]);
}
