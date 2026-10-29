//! A small SFTP v3 client (ADR-0005's fallback for servers without GNU `find -printf`): it speaks
//! the protocol over any reader/writer pair, in practice the stdin/stdout of `ssh -s host sftp`,
//! and walks a folder tree breadth-first. Folders are opened and read through a window of
//! outstanding requests, so a slow link costs one round trip per window rather than per folder.
//!
//! SFTP v3 has no allocation size, so On-disk Size equals Logical Size, and no device number, so
//! there is no `-xdev`: the walk stays inside the root's subtree and skips symlinks instead.

use crate::remote::Record;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};

const INIT: u8 = 1;
const VERSION: u8 = 2;
const CLOSE: u8 = 4;
const LSTAT: u8 = 7;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const NAME: u8 = 104;
const ATTRS: u8 = 105;

const FX_EOF: u32 = 1;
const ATTR_SIZE: u32 = 0x1;
const ATTR_UIDGID: u32 = 0x2;
const ATTR_PERMISSIONS: u32 = 0x4;
const ATTR_ACMODTIME: u32 = 0x8;
const ATTR_EXTENDED: u32 = 0x8000_0000;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

/// Requests in flight at once. OpenSSH's server allows 256 open handles; this stays far below.
const WINDOW: usize = 16;
/// Larger than any packet a sane server sends (OpenSSH caps at 256 KiB).
const MAX_PACKET: usize = 1 << 20;

/// What a server reports about a file; fields it left out are `None`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Attrs {
    size: Option<u64>,
    permissions: Option<u32>,
    /// Seconds since the epoch.
    mtime: Option<u32>,
}

impl Attrs {
    fn is_dir(&self) -> bool {
        self.permissions.is_some_and(|p| p & S_IFMT == S_IFDIR)
    }

    fn is_symlink(&self) -> bool {
        self.permissions.is_some_and(|p| p & S_IFMT == S_IFLNK)
    }

    fn record(&self, path: String) -> Record {
        let is_dir = self.is_dir();
        let size = self.size.unwrap_or(0);
        Record {
            inode: 0,
            links: 1,
            size,
            on_disk: size,
            modified: i64::from(self.mtime.unwrap_or(0)) * 1_000_000_000,
            created: 0,
            is_dir,
            path,
        }
    }
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("malformed SFTP reply: {what}"))
}

/// A cursor over one packet's body.
struct Body<'a>(&'a [u8]);

impl<'a> Body<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(bad("truncated"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn string(&mut self) -> io::Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    fn attrs(&mut self) -> io::Result<Attrs> {
        let flags = self.u32()?;
        let mut attrs = Attrs::default();
        if flags & ATTR_SIZE != 0 {
            attrs.size = Some(self.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            self.take(8)?;
        }
        if flags & ATTR_PERMISSIONS != 0 {
            attrs.permissions = Some(self.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            self.u32()?;
            attrs.mtime = Some(self.u32()?);
        }
        if flags & ATTR_EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.string()?;
                self.string()?;
            }
        }
        Ok(attrs)
    }
}

fn string(bytes: &[u8]) -> Vec<u8> {
    let mut v = (bytes.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(bytes);
    v
}

struct Wire<R, W> {
    input: R,
    output: W,
    next_id: u32,
}

impl<R: Read, W: Write> Wire<R, W> {
    fn send(&mut self, kind: u8, body: &[u8]) -> io::Result<()> {
        let mut packet = Vec::with_capacity(5 + body.len());
        packet.extend_from_slice(&(body.len() as u32 + 1).to_be_bytes());
        packet.push(kind);
        packet.extend_from_slice(body);
        self.output.write_all(&packet)?;
        self.output.flush()
    }

    /// Sends a request and returns its id.
    fn request(&mut self, kind: u8, payload: &[u8]) -> io::Result<u32> {
        self.next_id += 1;
        let mut body = self.next_id.to_be_bytes().to_vec();
        body.extend_from_slice(payload);
        self.send(kind, &body)?;
        Ok(self.next_id)
    }

    /// Reads one packet: its type and body.
    fn receive(&mut self) -> io::Result<(u8, Vec<u8>)> {
        let mut len = [0; 4];
        self.input.read_exact(&mut len)?;
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > MAX_PACKET {
            return Err(bad("packet length"));
        }
        let mut packet = vec![0; len];
        self.input.read_exact(&mut packet)?;
        let body = packet.split_off(1);
        Ok((packet[0], body))
    }

    fn handshake(&mut self) -> io::Result<()> {
        self.send(INIT, &3u32.to_be_bytes())?;
        match self.receive()? {
            (VERSION, _) => Ok(()),
            _ => Err(bad("expected a version reply")),
        }
    }
}

/// `STATUS` as an error: the server's code and message.
fn status_error(mut body: Body) -> io::Error {
    let code = body.u32().unwrap_or(0);
    let message = body.string().map(|m| String::from_utf8_lossy(m).into_owned()).unwrap_or_default();
    let kind = match code {
        2 => io::ErrorKind::NotFound,
        3 => io::ErrorKind::PermissionDenied,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{message} (SFTP status {code})"))
}

/// A folder being read.
struct Dir {
    path: Vec<u8>,
    /// Relative to the root; empty for the root itself.
    rel: String,
    handle: Vec<u8>,
}

enum Pending {
    Open { path: Vec<u8>, rel: String },
    Read(Dir),
    Close,
}

/// What a walk could not read.
#[derive(Debug, Default)]
pub struct Walk {
    /// Folders that could not be opened or read to the end.
    pub unreadable: u64,
}

/// Walks the tree under `root` (an absolute path on the server), calling `emit` with a record for
/// the root (empty path) and then for every Entry beneath it, each folder before its contents.
pub fn walk(input: impl Read, output: impl Write, root: &str, mut emit: impl FnMut(Record)) -> io::Result<Walk> {
    let mut wire = Wire { input, output, next_id: 0 };
    wire.handshake()?;
    let id = wire.request(LSTAT, &string(root.as_bytes()))?;
    let attrs = match wire.receive()? {
        (ATTRS, body) => {
            let mut body = Body(&body);
            (body.u32()? == id).then_some(()).ok_or_else(|| bad("reply id"))?;
            body.attrs()?
        }
        (STATUS, body) => return Err(status_error(Body(body.get(4..).unwrap_or_default()))),
        _ => return Err(bad("reply to LSTAT")),
    };
    if !attrs.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{root} is not a folder on the server"),
        ));
    }
    emit(attrs.record(String::new()));

    let mut walk = Walk::default();
    let mut queue = VecDeque::from([(root.as_bytes().to_vec(), String::new())]);
    let mut pending: HashMap<u32, Pending> = HashMap::new();
    while !queue.is_empty() || !pending.is_empty() {
        while pending.len() < WINDOW {
            let Some((path, rel)) = queue.pop_front() else { break };
            let id = wire.request(OPENDIR, &string(&path))?;
            pending.insert(id, Pending::Open { path, rel });
        }
        let (kind, body) = wire.receive()?;
        let mut body = Body(&body);
        let state = pending.remove(&body.u32()?).ok_or_else(|| bad("unknown request id"))?;
        match (state, kind) {
            (Pending::Open { path, rel }, HANDLE) => {
                let handle = body.string()?.to_vec();
                let id = wire.request(READDIR, &string(&handle))?;
                pending.insert(id, Pending::Read(Dir { path, rel, handle }));
            }
            (Pending::Open { .. }, STATUS) => walk.unreadable += 1,
            (Pending::Read(dir), NAME) => {
                for _ in 0..body.u32()? {
                    let name = body.string()?;
                    body.string()?; // long name, for humans
                    let attrs = body.attrs()?;
                    if name == b"." || name == b".." || name.contains(&b'/') || attrs.is_symlink() {
                        continue;
                    }
                    let text = String::from_utf8_lossy(name);
                    let rel = if dir.rel.is_empty() {
                        text.into_owned()
                    } else {
                        format!("{}/{text}", dir.rel)
                    };
                    if attrs.is_dir() {
                        let mut path = dir.path.clone();
                        if path.last() != Some(&b'/') {
                            path.push(b'/');
                        }
                        path.extend_from_slice(name);
                        queue.push_back((path, rel.clone()));
                    }
                    emit(attrs.record(rel));
                }
                let id = wire.request(READDIR, &string(&dir.handle))?;
                pending.insert(id, Pending::Read(dir));
            }
            (Pending::Read(dir), STATUS) => {
                if body.u32()? != FX_EOF {
                    walk.unreadable += 1;
                }
                let id = wire.request(CLOSE, &string(&dir.handle))?;
                pending.insert(id, Pending::Close);
            }
            (Pending::Close, STATUS) => {}
            _ => return Err(bad("unexpected reply")),
        }
    }
    Ok(walk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_parse_with_every_optional_part() {
        let mut b = Vec::new();
        b.extend((ATTR_SIZE | ATTR_UIDGID | ATTR_PERMISSIONS | ATTR_ACMODTIME | ATTR_EXTENDED).to_be_bytes());
        b.extend(7u64.to_be_bytes());
        b.extend([0; 8]);
        b.extend((S_IFDIR | 0o755).to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(1_700_000_000u32.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(string(b"k"));
        b.extend(string(b"v"));
        let attrs = Body(&b).attrs().unwrap();
        assert_eq!(
            attrs,
            Attrs {
                size: Some(7),
                permissions: Some(S_IFDIR | 0o755),
                mtime: Some(1_700_000_000)
            }
        );
        assert!(attrs.is_dir() && !attrs.is_symlink());
        assert!(Body(&b[..10]).attrs().is_err());
    }

    #[test]
    fn a_truncated_or_oversized_reply_is_an_error_not_a_panic() {
        let walk_of = |bytes: &[u8]| walk(bytes, io::sink(), "/x", |_| {}).unwrap_err().kind();
        assert_eq!(walk_of(&[]), io::ErrorKind::UnexpectedEof);
        // VERSION, then a packet claiming more bytes than the server sent.
        assert_eq!(
            walk_of(&[0, 0, 0, 5, 2, 0, 0, 0, 3, 0, 0, 0, 50, 105]),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(walk_of(&[0xff, 0xff, 0xff, 0xff]), io::ErrorKind::InvalidData);
    }
}
