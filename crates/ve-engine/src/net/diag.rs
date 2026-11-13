//! Each TCP socket's byte counts on Linux, from the kernel's socket diagnostics (`sock_diag`
//! over netlink): the `bytes_received` and `bytes_acked` of its `tcp_info`, which `ss -ti`
//! shows to any user, no privileges needed. Keyed by socket inode, the number
//! `/proc/net/tcp` and the `socket:[inode]` links of `/proc/<pid>/fd` use.

use super::TcpBytes;
use std::collections::HashMap;

/// `nlmsghdr`: length, type, flags, sequence, port.
const HEADER: usize = 16;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
/// Set on a dump's messages when the table changed while it was read: it may miss sockets.
const NLM_F_DUMP_INTR: u16 = 0x10;
/// `inet_diag_msg`: family, state, timer, retransmits, the socket id (48 bytes), expires,
/// receive and send queues, user, inode; its attributes follow.
const DIAG_MSG: usize = 72;
const INODE_AT: usize = 68;
/// The attribute holding `tcp_info`, and the bit asking for it.
const INET_DIAG_INFO: u16 = 2;
/// Where `tcp_info` holds `bytes_acked` and `bytes_received` (Linux 4.1 on).
const BYTES_ACKED_AT: usize = 120;
const BYTES_RECEIVED_AT: usize = 128;
/// TCP states not asked for: they have no `tcp_info` (`TIME_WAIT`, `SYN_RECV`, `NEW_SYN_RECV`).
const NO_INFO_STATES: u32 = (1 << 6) | (1 << 3) | (1 << 12);

/// The byte counts of every TCP socket (v4 and v6) of this network namespace, by inode; `None`
/// when the kernel cannot be asked (not Linux, no `sock_diag`). A family the kernel refuses
/// (IPv6 turned off) leaves the other's counts.
pub(crate) fn tcp_bytes() -> Option<HashMap<u64, TcpBytes>> {
    #[cfg(target_os = "linux")]
    {
        let mut counts = HashMap::new();
        let mut read = false;
        // One receive buffer and one scratch map for both families.
        let mut buf = vec![0u8; 64 * 1024];
        let mut of_family = HashMap::new();
        for family in [libc::AF_INET, libc::AF_INET6] {
            // A socket per family, so what is left of a failed dump never reads as the next;
            // a failed dump adds nothing, since what it did read may miss sockets.
            of_family.clear();
            if netlink::Socket::open()
                .and_then(|s| s.dump(family as u8, &mut buf, &mut of_family))
                .is_some()
            {
                counts.extend(of_family.drain());
                read = true;
            }
        }
        read.then_some(counts)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The request for every TCP socket of `family` with its `tcp_info`.
fn request(family: u8) -> [u8; HEADER + 56] {
    let mut req = [0u8; HEADER + 56];
    req[0..4].copy_from_slice(&((HEADER + 56) as u32).to_ne_bytes());
    req[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    req[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    req[8..12].copy_from_slice(&1u32.to_ne_bytes());
    // inet_diag_req_v2: family, protocol, extensions, padding, states, socket id (any).
    req[HEADER] = family;
    req[HEADER + 1] = 6; // IPPROTO_TCP
    req[HEADER + 2] = 1 << (INET_DIAG_INFO - 1);
    req[HEADER + 4..HEADER + 8].copy_from_slice(&(!NO_INFO_STATES).to_ne_bytes());
    req
}

/// What one datagram of a dump reply says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Part {
    /// More messages follow in the next datagram.
    More,
    /// The dump is complete.
    Done,
    /// The kernel refused the request, or the reply is not one this reader understands.
    Failed,
}

/// Adds the byte counts of each socket in `datagram`, one datagram of a dump reply, to
/// `counts`. A socket without `tcp_info`, or one too short to hold the byte counts (a kernel
/// before 4.1), is left out.
pub(crate) fn parse(datagram: &[u8], counts: &mut HashMap<u64, TcpBytes>) -> Part {
    let u16_at = |b: &[u8], at: usize| b.get(at..at + 2).map(|s| u16::from_ne_bytes([s[0], s[1]]));
    let u32_at = |b: &[u8], at: usize| b.get(at..at + 4).map(|s| u32::from_ne_bytes(s.try_into().unwrap()));
    let u64_at = |b: &[u8], at: usize| b.get(at..at + 8).map(|s| u64::from_ne_bytes(s.try_into().unwrap()));
    let mut rest = datagram;
    while !rest.is_empty() {
        let (Some(len), Some(kind), Some(flags)) = (u32_at(rest, 0), u16_at(rest, 4), u16_at(rest, 6)) else {
            return Part::Failed;
        };
        let len = len as usize;
        if len < HEADER || len > rest.len() || flags & NLM_F_DUMP_INTR != 0 {
            return Part::Failed;
        }
        match kind {
            NLMSG_DONE => return Part::Done,
            NLMSG_ERROR => return Part::Failed,
            SOCK_DIAG_BY_FAMILY => {
                let message = &rest[HEADER..len];
                let Some(inode) = u32_at(message, INODE_AT) else {
                    return Part::Failed;
                };
                let mut attributes = message.get(DIAG_MSG..).unwrap_or_default();
                while let (Some(attr_len), Some(attr_kind)) = (u16_at(attributes, 0), u16_at(attributes, 2)) {
                    let attr_len = usize::from(attr_len);
                    if attr_len < 4 || attr_len > attributes.len() {
                        break;
                    }
                    // The top two bits are flags (nested, byte order).
                    if attr_kind & 0x3fff == INET_DIAG_INFO && inode != 0 {
                        let info = &attributes[4..attr_len];
                        if let (Some(sent), Some(received)) = (u64_at(info, BYTES_ACKED_AT), u64_at(info, BYTES_RECEIVED_AT)) {
                            counts.insert(u64::from(inode), TcpBytes { received, sent });
                        }
                    }
                    attributes = attributes.get(align(attr_len)..).unwrap_or_default();
                }
            }
            _ => {}
        }
        rest = rest.get(align(len)..).unwrap_or_default();
    }
    Part::More
}

/// Netlink lengths are padded to four bytes.
fn align(len: usize) -> usize {
    (len + 3) & !3
}

#[cfg(target_os = "linux")]
mod netlink {
    use super::{Part, TcpBytes, parse, request};
    use std::collections::HashMap;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    /// The protocol of `sock_diag` sockets.
    const NETLINK_SOCK_DIAG: i32 = 4;

    pub(super) struct Socket(OwnedFd);

    impl Socket {
        pub(super) fn open() -> Option<Socket> {
            // SAFETY: socket takes no pointers; a negative result is checked before use.
            let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, NETLINK_SOCK_DIAG) };
            if fd < 0 {
                return None;
            }
            // SAFETY: fd is a fresh descriptor this function owns.
            let socket = Socket(unsafe { OwnedFd::from_raw_fd(fd) });
            // A kernel that never answers must not stall the recorder.
            let timeout = libc::timeval { tv_sec: 1, tv_usec: 0 };
            // SAFETY: the pointer and length describe `timeout`, which outlives the call.
            let set = unsafe {
                libc::setsockopt(
                    socket.0.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&raw const timeout).cast(),
                    size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            (set == 0).then_some(socket)
        }

        /// Asks for every TCP socket of `family` and adds their byte counts to `counts`,
        /// receiving into `buf`.
        pub(super) fn dump(&self, family: u8, buf: &mut [u8], counts: &mut HashMap<u64, TcpBytes>) -> Option<()> {
            let req = request(family);
            // SAFETY: zeroed is a valid sockaddr_nl: port 0 and no groups address the kernel.
            let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
            // SAFETY: the pointers and lengths describe `req` and `kernel`, alive for the call.
            let sent = unsafe {
                libc::sendto(
                    self.0.as_raw_fd(),
                    req.as_ptr().cast(),
                    req.len(),
                    0,
                    (&raw const kernel).cast(),
                    size_of::<libc::sockaddr_nl>() as libc::socklen_t,
                )
            };
            if sent < 0 {
                return None;
            }
            loop {
                // SAFETY: the pointer and length describe `buf`, which outlives the call.
                let got = unsafe { libc::recv(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
                let got = usize::try_from(got).ok().filter(|&n| n > 0)?;
                match parse(&buf[..got], counts) {
                    Part::More => {}
                    Part::Done => return Some(()),
                    Part::Failed => return None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `SOCK_DIAG_BY_FAMILY` message for socket `inode` with a `tcp_info` of `info_len`
    /// bytes holding `acked` and `received` where the kernel puts them, after an unrelated
    /// attribute.
    fn message(inode: u32, info_len: usize, acked: u64, received: u64) -> Vec<u8> {
        let mut diag = vec![0u8; DIAG_MSG];
        diag[0] = 2; // AF_INET
        diag[INODE_AT..INODE_AT + 4].copy_from_slice(&inode.to_ne_bytes());
        // An attribute of another kind first (INET_DIAG_CONG, "cubic\0"), padded.
        attribute(&mut diag, 4, b"cubic\0");
        let mut info = vec![0u8; info_len];
        if info_len >= BYTES_RECEIVED_AT + 8 {
            info[BYTES_ACKED_AT..BYTES_ACKED_AT + 8].copy_from_slice(&acked.to_ne_bytes());
            info[BYTES_RECEIVED_AT..BYTES_RECEIVED_AT + 8].copy_from_slice(&received.to_ne_bytes());
        }
        attribute(&mut diag, INET_DIAG_INFO, &info);
        framed(SOCK_DIAG_BY_FAMILY, &diag)
    }

    fn attribute(out: &mut Vec<u8>, kind: u16, data: &[u8]) {
        out.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(data);
        out.resize(align(out.len()), 0);
    }

    fn framed(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut m = vec![0u8; HEADER];
        m[0..4].copy_from_slice(&((HEADER + payload.len()) as u32).to_ne_bytes());
        m[4..6].copy_from_slice(&kind.to_ne_bytes());
        m.extend_from_slice(payload);
        m.resize(align(m.len()), 0);
        m
    }

    #[test]
    fn a_dump_reads_each_sockets_received_and_acked_bytes_by_inode() {
        let mut datagram = message(4242, 232, 1_500, 90_000);
        datagram.extend(message(4243, 232, 0, 7));
        let mut counts = HashMap::new();
        assert_eq!(parse(&datagram, &mut counts), Part::More);
        assert_eq!(counts.len(), 2);
        assert_eq!(
            counts[&4242],
            TcpBytes {
                received: 90_000,
                sent: 1_500
            }
        );
        assert_eq!(counts[&4243], TcpBytes { received: 7, sent: 0 });

        assert_eq!(parse(&framed(NLMSG_DONE, &[0; 4]), &mut counts), Part::Done);
    }

    #[test]
    fn a_tcp_info_without_byte_counts_or_a_socket_without_inode_is_left_out() {
        // A kernel before 4.1 has a shorter tcp_info.
        let mut datagram = message(10, 104, 1, 1);
        datagram.extend(message(0, 232, 5, 5));
        let mut counts = HashMap::new();
        assert_eq!(parse(&datagram, &mut counts), Part::More);
        assert!(counts.is_empty());
    }

    #[test]
    fn an_error_a_cut_message_or_an_interrupted_dump_fails_the_dump() {
        let mut counts = HashMap::new();
        assert_eq!(parse(&framed(NLMSG_ERROR, &[0xff; 20]), &mut counts), Part::Failed);
        let whole = message(1, 232, 1, 1);
        assert_eq!(parse(&whole[..whole.len() - 10], &mut counts), Part::Failed);
        // The socket table changed while it was read: the dump may miss sockets.
        let mut interrupted = message(1, 232, 1, 1);
        interrupted[6..8].copy_from_slice(&(NLM_F_DUMP_INTR | 0x2).to_ne_bytes());
        assert_eq!(parse(&interrupted, &mut counts), Part::Failed);
    }
}
