use std::ffi::{CString, OsString};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Created, deleted, moved, closed after writing, or attributes changed; folders included.
const MASK: u64 = libc::FAN_CREATE
    | libc::FAN_DELETE
    | libc::FAN_MOVED_FROM
    | libc::FAN_MOVED_TO
    | libc::FAN_CLOSE_WRITE
    | libc::FAN_ATTRIB
    | libc::FAN_ONDIR;
const EVENT_INFO_DFID_NAME: u8 = 2;
const EVENT_INFO_DFID: u8 = 3;
const EVENT_HEADER: usize = std::mem::size_of::<libc::fanotify_event_metadata>();

/// A change as the kernel reports it: the folder as a file handle, and the name in it.
#[derive(Debug, PartialEq, Eq)]
struct Raw {
    fsid: [i32; 2],
    /// The `struct file_handle` as the kernel wrote it (size, type, bytes).
    handle: Vec<u8>,
    name: Option<Vec<u8>>,
}

enum Parsed {
    Change(Raw),
    Overflow,
}

/// Splits a read of the fanotify descriptor into events. Stops at anything malformed.
fn parse(buf: &[u8]) -> Vec<Parsed> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + EVENT_HEADER <= buf.len() {
        let len = u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
        let meta_len = u16::from_ne_bytes(buf[at + 6..at + 8].try_into().unwrap()) as usize;
        let mask = u64::from_ne_bytes(buf[at + 8..at + 16].try_into().unwrap());
        if len < EVENT_HEADER || at + len > buf.len() || meta_len > len {
            break;
        }
        if mask & libc::FAN_Q_OVERFLOW != 0 {
            out.push(Parsed::Overflow);
        } else if let Some(raw) = parse_info(&buf[at + meta_len..at + len]) {
            out.push(Parsed::Change(raw));
        }
        at += len;
    }
    out
}

/// The folder-and-name record of an event (the folder alone for a record without a name).
fn parse_info(mut info: &[u8]) -> Option<Raw> {
    while info.len() >= 4 {
        let kind = info[0];
        let len = u16::from_ne_bytes([info[2], info[3]]) as usize;
        if len < 4 || len > info.len() {
            return None;
        }
        let record = &info[4..len];
        if matches!(kind, EVENT_INFO_DFID_NAME | EVENT_INFO_DFID) && record.len() >= 16 {
            let fsid = [
                i32::from_ne_bytes(record[0..4].try_into().unwrap()),
                i32::from_ne_bytes(record[4..8].try_into().unwrap()),
            ];
            let handle_len = 8 + u32::from_ne_bytes(record[8..12].try_into().unwrap()) as usize;
            if handle_len > record.len() - 8 {
                return None;
            }
            let handle = record[8..8 + handle_len].to_vec();
            let name = (kind == EVENT_INFO_DFID_NAME).then(|| {
                let rest = &record[8 + handle_len..];
                rest[..rest.iter().position(|&b| b == 0).unwrap_or(rest.len())].to_vec()
            });
            return Some(Raw { fsid, handle, name });
        }
        // The object's own handle (type 1) comes along too; only the folder's record is wanted.
        info = &info[len..];
    }
    None
}

struct Mark {
    fsid: [i32; 2],
    mount: OwnedFd,
}

fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: `c` is a valid NUL-terminated path.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn init() -> io::Result<OwnedFd> {
    let flags = libc::FAN_CLASS_NOTIF | libc::FAN_CLOEXEC | libc::FAN_NONBLOCK | libc::FAN_REPORT_FID | libc::FAN_REPORT_DFID_NAME;
    // SAFETY: plain syscall; the descriptor it returns is owned below.
    let fd = unsafe { libc::fanotify_init(flags, (libc::O_RDONLY | libc::O_LARGEFILE) as u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fanotify_init` returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn mark(fan: &OwnedFd, mount: &Path) -> io::Result<Mark> {
    let dir = open_dir(mount)?;
    // SAFETY: zeroed statfs is a valid output buffer; `dir` is an open descriptor.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(dir.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let c = CString::new(mount.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: valid descriptor and NUL-terminated path; no event descriptor is requested.
    let marked = unsafe {
        libc::fanotify_mark(
            fan.as_raw_fd(),
            libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
            MASK,
            libc::AT_FDCWD,
            c.as_ptr(),
        )
    };
    if marked != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fsid_t is two ints.
    let fsid: [i32; 2] = unsafe { std::mem::transmute(st.f_fsid) };
    Ok(Mark { fsid, mount: dir })
}

/// The path of the folder a handle names, or None if it is gone.
fn folder_path(marks: &[Mark], raw: &Raw) -> Option<PathBuf> {
    let mount = marks.iter().find(|m| m.fsid == raw.fsid)?;
    // The kernel reads a `struct file_handle`: 4-byte alignment is what it asks for.
    let mut words = vec![0u32; raw.handle.len().div_ceil(4)];
    // SAFETY: `words` holds at least `handle.len()` bytes.
    unsafe { std::ptr::copy_nonoverlapping(raw.handle.as_ptr(), words.as_mut_ptr().cast::<u8>(), raw.handle.len()) };
    // SAFETY: the buffer is a file handle as received from the kernel; `mount` is open.
    let fd = unsafe { libc::open_by_handle_at(mount.mount.as_raw_fd(), words.as_mut_ptr().cast(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: new descriptor, owned here and closed on drop.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()
}

/// What goes to stdout for `raw`: the changed path, or `None` if its folder is gone.
fn record(marks: &[Mark], raw: &Raw) -> Option<Vec<u8>> {
    let mut path = folder_path(marks, raw)?.into_os_string().into_vec();
    if let Some(name) = &raw.name {
        if path != b"/" {
            path.push(b'/');
        }
        path.extend_from_slice(name);
    }
    Some(path)
}

fn drain(fan: &OwnedFd, marks: &[Mark], out: &mut impl Write) -> io::Result<()> {
    let mut buf = vec![0u8; 1 << 16];
    loop {
        // SAFETY: reading into our own buffer, with its length.
        let n = unsafe { libc::read(fan.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::WouldBlock { Ok(()) } else { Err(e) };
        }
        if n == 0 {
            return Ok(());
        }
        for event in parse(&buf[..n as usize]) {
            match event {
                Parsed::Overflow => out.write_all(b"!lost\0")?,
                Parsed::Change(raw) => {
                    if let Some(mut path) = record(marks, &raw) {
                        path.push(0);
                        out.write_all(&path)?;
                    }
                }
            }
        }
        out.flush()?;
    }
}

pub fn run(mounts: Vec<OsString>) -> i32 {
    // SAFETY: plain syscall; dies with the parent (pkexec), whatever happens to the app.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
    let fan = match init() {
        Ok(fan) => fan,
        Err(e) => {
            eprintln!("ve-fanotify: fanotify_init: {e}");
            return 2;
        }
    };
    let mut marks = Vec::new();
    for mount in mounts {
        match mark(&fan, Path::new(&mount)) {
            Ok(m) => marks.push(m),
            Err(e) => eprintln!("ve-fanotify: {}: {e}", mount.to_string_lossy()),
        }
    }
    let mut out = io::stdout().lock();
    if out.write_all(b"!ready\0").and_then(|()| out.flush()).is_err() {
        return 0;
    }
    let mut told = Vec::new();
    loop {
        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: fan.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfds, and the count says so.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return 1;
        }
        if fds[1].revents & libc::POLLIN != 0 && drain(&fan, &marks, &mut out).is_err() {
            // stdout is closed: the app is gone.
            return 0;
        }
        if fds[0].revents != 0 {
            let mut chunk = [0u8; 4096];
            // SAFETY: reading into our own buffer, with its length.
            let n = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n <= 0 {
                return 0;
            }
            told.extend_from_slice(&chunk[..n as usize]);
            while let Some(end) = told.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = told.drain(..=end).collect();
                if let Some(path) = line[..end].strip_prefix(b"mark ") {
                    let path = PathBuf::from(OsString::from_vec(path.to_vec()));
                    // Only an absolute folder: a malformed line must not widen what is watched.
                    if path.is_absolute() {
                        match mark(&fan, &path) {
                            Ok(m) => marks.push(m),
                            Err(e) => eprintln!("ve-fanotify: {}: {e}", path.display()),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One event: metadata, then a DFID_NAME record for `dir_handle` and `name`.
    fn event(mask: u64, fsid: [i32; 2], handle_bytes: &[u8], name: &str) -> Vec<u8> {
        let mut handle = (handle_bytes.len() as u32).to_ne_bytes().to_vec();
        handle.extend(1i32.to_ne_bytes());
        handle.extend(handle_bytes);
        let mut record = vec![EVENT_INFO_DFID_NAME, 0];
        let mut body = [fsid[0].to_ne_bytes(), fsid[1].to_ne_bytes()].concat();
        body.extend(&handle);
        body.extend(name.as_bytes());
        body.push(0);
        record.extend(((body.len() + 4) as u16).to_ne_bytes());
        record.extend(body);
        let mut out = ((EVENT_HEADER + record.len()) as u32).to_ne_bytes().to_vec();
        out.extend([3, 0]);
        out.extend((EVENT_HEADER as u16).to_ne_bytes());
        out.extend(mask.to_ne_bytes());
        out.extend((-1i32).to_ne_bytes());
        out.extend(0i32.to_ne_bytes());
        out.extend(record);
        out
    }

    #[test]
    fn events_split_into_folder_handle_and_name() {
        let mut buf = event(libc::FAN_CREATE, [7, 9], &[1, 2, 3, 4, 5, 6, 7, 8], "new file");
        buf.extend(event(libc::FAN_DELETE, [7, 9], &[9, 9], "b"));
        let got = parse(&buf);
        assert_eq!(got.len(), 2);
        let Parsed::Change(first) = &got[0] else { panic!() };
        assert_eq!(first.fsid, [7, 9]);
        assert_eq!(first.name.as_deref(), Some(&b"new file"[..]));
        assert_eq!(&first.handle[8..], &[1, 2, 3, 4, 5, 6, 7, 8]);
        let Parsed::Change(second) = &got[1] else { panic!() };
        assert_eq!(second.name.as_deref(), Some(&b"b"[..]));
    }

    #[test]
    fn an_overflow_and_a_truncated_event_are_handled() {
        let mut buf = event(libc::FAN_Q_OVERFLOW, [0, 0], &[], "");
        assert!(matches!(parse(&buf)[..], [Parsed::Overflow]));
        buf = event(libc::FAN_CREATE, [1, 1], &[1, 2, 3, 4], "x");
        buf.truncate(buf.len() - 3);
        assert!(parse(&buf).is_empty());
    }

    /// Real fanotify needs CAP_SYS_ADMIN: skipped (with this message) when run unprivileged.
    #[test]
    fn real_fanotify_reports_a_created_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let marks = match init().and_then(|fan| mark(&fan, &root).map(|m| (fan, m))) {
            Ok(ok) => ok,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipped: watching a filesystem is not permitted here ({e}); run the tests as root to check it");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        let (fan, marks) = (marks.0, vec![marks.1]);
        std::fs::write(root.join("made"), b"x").unwrap();
        let mut out = Vec::new();
        drain(&fan, &marks, &mut out).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains(root.join("made").to_str().unwrap()), "{text:?}");
    }
}
