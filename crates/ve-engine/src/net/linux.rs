//! `/proc/net` on Linux: each interface's cumulative bytes received and sent (`dev`), and
//! every TCP and UDP socket (`tcp`, `tcp6`, `udp`, `udp6`) with the process holding it, found
//! through `/proc/<pid>/fd`.

use super::{InterfaceCounters, Protocol, Socket, SocketHolder, State};
use crate::perf::Reading;
use std::collections::HashMap;
use std::fs;
use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

/// The interfaces of `/proc/net/dev`, less loopback, which is no link. Names come from
/// `names`, so every sample shares one copy of each.
pub(crate) fn interface_counters(text: &str, names: &mut Vec<Arc<str>>) -> Vec<InterfaceCounters> {
    text.lines()
        .filter_map(|line| {
            // "  eth0: rx_bytes rx_packets ... (8 receive fields) tx_bytes ..."; the two header
            // lines hold no colon.
            let (name, fields) = line.split_once(':')?;
            let name = name.trim();
            if name.is_empty() || name == "lo" {
                return None;
            }
            let mut fields = fields.split_ascii_whitespace();
            let received = fields.next()?.parse().ok()?;
            let sent = fields.nth(7)?.parse().ok()?;
            let name = match names.iter().find(|n| &***n == name) {
                Some(n) => n.clone(),
                None => {
                    let n: Arc<str> = name.into();
                    names.push(n.clone());
                    n
                }
            };
            Some(InterfaceCounters { name, received, sent })
        })
        .collect()
}

/// The socket tables of `/proc/net`, with their protocol and address family.
const SOCKET_TABLES: [(&str, Protocol, bool); 4] = [
    ("proc/net/tcp", Protocol::Tcp, false),
    ("proc/net/tcp6", Protocol::Tcp, true),
    ("proc/net/udp", Protocol::Udp, false),
    ("proc/net/udp6", Protocol::Udp, true),
];

/// Every TCP and UDP socket (v4 and v6) of `root/proc/net`, each with the process holding it
/// where the user may read it; unsupported when none of the tables can be read. Only the
/// user's own processes are searched (`/proc/<pid>/fd` of others' cannot be read), and only
/// for the user's own sockets: another user's socket is held by a process of theirs.
pub(crate) fn sockets(root: &Path) -> Reading<Vec<Socket>> {
    let user = current_user();
    let mut sockets = Vec::new();
    // The inode of each socket whose holder is still to be found, by its index in `sockets`.
    let mut wanted: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut read_any = false;
    let mut text = String::new();
    for (file, protocol, v6) in SOCKET_TABLES {
        text.clear();
        if fs::File::open(root.join(file))
            .and_then(|mut f| f.read_to_string(&mut text))
            .is_err()
        {
            continue;
        }
        read_any = true;
        // The first line holds the column names.
        for line in text.lines().skip(1) {
            let Some(row) = parse_socket(line, v6) else { continue };
            let holder = if row.inode == 0 {
                SocketHolder::Nobody
            } else if user.is_some_and(|u| u == row.uid) {
                wanted.entry(row.inode).or_default().push(sockets.len());
                SocketHolder::Unknown
            } else {
                SocketHolder::Unknown
            };
            sockets.push(Socket {
                protocol,
                local: row.local,
                remote: row.remote,
                state: row.state,
                holder,
            });
        }
    }
    if !read_any {
        return Reading::Unsupported;
    }
    if let Some(user) = user
        && !wanted.is_empty()
    {
        find_holders(root, user, &mut wanted, &mut sockets);
    }
    Reading::Value(sockets)
}

/// One row of a socket table: the fields a connection needs.
struct SocketRow {
    local: SocketAddr,
    remote: SocketAddr,
    state: State,
    uid: u32,
    inode: u64,
}

/// `sl local_address rem_address st tx_queue:rx_queue tr:tm->when retrnsmt uid timeout inode ...`
fn parse_socket(line: &str, v6: bool) -> Option<SocketRow> {
    let mut fields = line.split_ascii_whitespace();
    let _slot = fields.next()?;
    let local = parse_address(fields.next()?, v6)?;
    let remote = parse_address(fields.next()?, v6)?;
    let state = State::from_code(u8::from_str_radix(fields.next()?, 16).ok()?);
    // Queues, timer, retransmits.
    let uid = fields.nth(3)?.parse().ok()?;
    let inode = fields.nth(1)?.parse().ok()?;
    Some(SocketRow {
        local,
        remote,
        state,
        uid,
        inode,
    })
}

/// `0100007F:0277` (v4) or 32 hex digits and a port (v6). The kernel prints each 32-bit word
/// of the address as a number in the machine's byte order, the port as a plain number.
fn parse_address(text: &str, v6: bool) -> Option<SocketAddr> {
    let (ip, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| -> Option<[u8; 4]> { Some(u32::from_str_radix(ip.get(i * 8..i * 8 + 8)?, 16).ok()?.to_ne_bytes()) };
    let ip = if v6 {
        if ip.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for i in 0..4 {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
        }
        IpAddr::V6(Ipv6Addr::from(bytes))
    } else {
        if ip.len() != 8 {
            return None;
        }
        IpAddr::V4(Ipv4Addr::from(word(0)?))
    };
    Some(SocketAddr::new(ip, port))
}

/// Marks each socket in `wanted` with the process of `user` holding it, found through the
/// `socket:[inode]` links of `/proc/<pid>/fd`. A process whose descriptors cannot be read
/// leaves its sockets unknown. A socket several processes hold (inherited across a fork) is
/// marked with the first found. Stops once every wanted socket is found.
fn find_holders(root: &Path, user: u32, wanted: &mut HashMap<u64, Vec<usize>>, sockets: &mut [Socket]) {
    let Ok(entries) = fs::read_dir(root.join("proc")) else { return };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if owner(&entry.path()) != Some(user) {
            continue;
        }
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else { continue };
            let Some(inode) = target
                .to_str()
                .and_then(|t| t.strip_prefix("socket:[")?.strip_suffix(']')?.parse::<u64>().ok())
            else {
                continue;
            };
            if let Some(indices) = wanted.remove(&inode) {
                for i in indices {
                    sockets[i].holder = SocketHolder::Pid(pid);
                }
                if wanted.is_empty() {
                    return;
                }
            }
        }
    }
}

/// The user this process runs as.
fn current_user() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        Some(unsafe { libc::getuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The user owning `dir` (`/proc/<pid>`), which is the user the process runs as.
fn owner(dir: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        fs::metadata(dir).ok().map(|m| m.uid())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}
