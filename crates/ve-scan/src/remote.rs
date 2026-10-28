//! SSH Remotes (ADR-0005): run the system `ssh` binary with `find <root> -xdev -printf ...` on
//! the server, stream its output and apply it to the Index as it arrives. Nothing is installed
//! on the server. `BatchMode` means keys and the agent only: a password-only host reports an error
//! instead of prompting.

use crate::{Progress, SharedIndex};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use ve_core::{EntryId, EntryMeta, Remote, SshRemote, SyncState, VolumeId};

/// One record per Entry, NUL-terminated so a name may hold anything but NUL; the name is last
/// so that a tab inside it stays inside it. Fields: inode, link count, size in bytes, 512-byte
/// blocks, modification time in seconds since the epoch (with fraction), file type, path relative
/// to the root (empty for the root itself).
const FORMAT: &str = r"%i\t%n\t%s\t%b\t%T@\t%y\t%P\0";

/// Entries applied to the Index per write lock.
const BATCH: usize = 4096;

/// A record parsed from `find -printf` output.
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    pub inode: u64,
    pub links: u64,
    pub size: u64,
    pub blocks: u64,
    /// Nanoseconds since the Unix epoch.
    pub modified: i64,
    pub is_dir: bool,
    pub path: String,
}

impl Record {
    /// Parses one record without its terminating NUL; `None` if it is malformed.
    pub fn parse(record: &[u8]) -> Option<Record> {
        let mut fields = record.splitn(7, |&b| b == b'\t');
        let number = |f: &[u8]| std::str::from_utf8(f).ok()?.parse::<u64>().ok();
        let inode = number(fields.next()?)?;
        let links = number(fields.next()?)?;
        let size = number(fields.next()?)?;
        let blocks = number(fields.next()?)?;
        let modified = parse_time(std::str::from_utf8(fields.next()?).ok()?)?;
        let kind = fields.next()?;
        let path = String::from_utf8_lossy(fields.next()?).into_owned();
        Some(Record { inode, links, size, blocks, modified, is_dir: kind == b"d", path })
    }

    fn meta(&self) -> EntryMeta {
        // Folders carry no size of their own: Folder Size is the sum of their contents.
        let file = !self.is_dir;
        EntryMeta {
            is_dir: self.is_dir,
            logical: if file { self.size } else { 0 },
            on_disk: if file { self.blocks * 512 } else { 0 },
            modified: self.modified,
            key: self.inode,
            // Names sharing an inode (find lists each) are one file, counted once by the Index.
            hardlinked: file && self.links > 1,
        }
    }
}

/// `1700000000.1234567890` (GNU find prints ten fraction digits) as nanoseconds.
fn parse_time(text: &str) -> Option<i64> {
    let (secs, frac) = text.split_once('.').unwrap_or((text, ""));
    let negative = secs.starts_with('-');
    let secs: i64 = secs.parse().ok()?;
    let digits: String = frac.chars().take(9).collect();
    let nanos: i64 = if digits.is_empty() { 0 } else { digits.parse::<i64>().ok()? * 10i64.pow(9 - digits.len() as u32) };
    secs.checked_mul(1_000_000_000)?.checked_add(if negative { -nanos } else { nanos })
}

/// `'...'` for a POSIX shell, which is what the server runs the command line through.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn command_line(root: &str) -> String {
    format!("find {} -xdev -printf {}", shell_quote(root), shell_quote(FORMAT))
}

/// Adds Remotes in `wanted` that the Index lacks and removes Remotes it holds that are no longer
/// wanted. A Remote is always online. Returns the Remotes newly added, to sync now.
pub fn reconcile_remotes(index: &SharedIndex, wanted: &[Remote]) -> Vec<VolumeId> {
    let mut index = index.write();
    let known: Vec<_> = index.volumes().filter_map(|(id, v)| Some((id, v.remote()?))).collect();
    for (id, remote) in &known {
        if wanted.contains(remote) {
            let mount = index.volume(*id).map(|v| v.mount.clone());
            index.set_mount(*id, mount.as_deref());
        } else {
            index.remove_volume(*id);
        }
    }
    wanted
        .iter()
        .filter(|r| !known.iter().any(|(_, k)| k == *r))
        .map(|r| index.add_remote(r))
        .collect()
}

/// Brings a Remote's Entries up to date with what its server lists now.
pub fn sync(index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    let s3 = matches!(index.read().volume(volume).map(|v| &v.sync), Some(SyncState::S3(_)));
    if s3 { crate::s3::sync(index, volume, progress) } else { sync_with("ssh", index, volume, progress) }
}

/// [`sync`] through the `ssh` program `program` (a test double, or the system one).
pub fn sync_with(program: &str, index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    let (root, remote) = {
        let index = index.read();
        let v = index.volume(volume).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such Remote"))?;
        match &v.sync {
            SyncState::Ssh(remote) => (v.root, remote.clone()),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a Remote")),
        }
    };
    let mut child = Command::new(program)
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=15", "--"])
        .arg(&remote.host)
        .arg(command_line(&remote.path))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => io::Error::new(io::ErrorKind::NotFound, "the ssh program was not found"),
            _ => e,
        })?;
    let mut stderr = child.stderr.take().expect("piped");
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_string(&mut text);
        // Keep draining: a full pipe would block find and ssh, and the sync with them.
        let _ = io::copy(&mut stderr, &mut io::sink());
        text
    });
    let mut applier = Applier::new(index, root, progress);
    let mut input = BufReader::new(child.stdout.take().expect("piped"));
    let (mut buf, mut batch) = (Vec::new(), Vec::new());
    let (mut read_error, mut malformed) = (None, false);
    loop {
        buf.clear();
        match input.read_until(0, &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                if buf.last() == Some(&0) {
                    buf.pop();
                }
                match Record::parse(&buf) {
                    Some(record) => batch.push((record.path.clone(), record.meta())),
                    None => malformed = true,
                }
                if batch.len() >= BATCH {
                    applier.apply(&mut batch);
                }
            }
            Err(e) => {
                read_error = Some(e);
                break;
            }
        }
    }
    applier.apply(&mut batch);
    drop(input);
    let status = child.wait()?;
    let stderr = stderr.join().unwrap_or_default();
    if let Some(e) = read_error {
        return Err(e);
    }
    if let Some(e) = failure(&remote, status.code(), applier.records, &stderr) {
        return Err(e);
    }
    // Exit 1 from find with output means some folders were unreadable.
    let unreadable = stderr.lines().filter(|l| l.contains("Permission denied")).count();
    progress.unreadable.fetch_add(unreadable as u64, Ordering::Relaxed);
    // Pruning needs a complete listing: after find reported unreadable folders (exit 1) or a
    // record was unreadable to us, what is missing may only be unseen, so stale Entries stay.
    if status.code() == Some(0) && !malformed {
        applier.prune();
    }
    Ok(())
}

/// Why the run failed, in words for the user, or `None` if it produced a listing.
fn failure(remote: &SshRemote, code: Option<i32>, records: u64, stderr: &str) -> Option<io::Error> {
    let message = stderr.trim().lines().last().unwrap_or("").trim();
    let host = &remote.host;
    let err = |kind, text: String| Some(io::Error::new(kind, text));
    match code {
        Some(0) => None,
        // find reports unreadable folders with 1 and still lists the rest.
        Some(1) if records > 0 => None,
        Some(255) if stderr.contains("Permission denied") => err(
            io::ErrorKind::PermissionDenied,
            format!("{host} needs a key or ssh-agent login; password login is not used ({message})"),
        ),
        Some(255) => err(io::ErrorKind::ConnectionRefused, format!("ssh to {host} failed: {message}")),
        _ if stderr.contains("-printf") => err(
            io::ErrorKind::Unsupported,
            format!("{host} has no GNU find -printf: needs SFTP fallback (issue 16)"),
        ),
        _ => err(io::ErrorKind::Other, format!("find on {host} failed: {message}")),
    }
}

/// Applies records to the Index, creating, updating and (at the end) pruning Entries.
///
/// ponytail: a refresh maps every existing Entry by (parent, name) in memory; a stream-diff
/// would need less but the Remotes this is for are one machine's folders.
pub(crate) struct Applier<'a> {
    index: &'a SharedIndex,
    progress: &'a Progress,
    root: EntryId,
    /// Folder path relative to the root -> its Entry. The root is the empty path.
    dirs: HashMap<String, EntryId>,
    /// Entries already in the Index that the listing has not mentioned yet.
    unseen: HashMap<(EntryId, String), EntryId>,
    pub(crate) records: u64,
}

impl<'a> Applier<'a> {
    pub(crate) fn new(index: &'a SharedIndex, root: EntryId, progress: &'a Progress) -> Self {
        let mut dirs = HashMap::from([(String::new(), root)]);
        let mut unseen = HashMap::new();
        {
            let index = index.read();
            let mut stack = vec![(root, String::new())];
            while let Some((dir, path)) = stack.pop() {
                for &child in index.children(dir) {
                    unseen.insert((dir, index.name(child).to_owned()), child);
                    if index.is_dir(child) {
                        let p = if path.is_empty() { index.name(child).to_owned() } else { format!("{path}/{}", index.name(child)) };
                        dirs.insert(p.clone(), child);
                        stack.push((child, p));
                    }
                }
            }
        }
        Applier { index, progress, root, dirs, unseen, records: 0 }
    }

    pub(crate) fn apply(&mut self, batch: &mut Vec<(String, EntryMeta)>) {
        if batch.is_empty() {
            return;
        }
        let mut index = self.index.write();
        for (path, meta) in batch.drain(..) {
            self.records += 1;
            self.progress.entries.fetch_add(1, Ordering::Relaxed);
            if path.is_empty() {
                index.update(self.root, meta);
                continue;
            }
            let (parent_path, name) = path.rsplit_once('/').unwrap_or(("", &path));
            let Some(&parent) = self.dirs.get(parent_path) else { continue };
            let id = match self.unseen.get(&(parent, name.to_owned())) {
                Some(&id) if index.is_dir(id) == meta.is_dir => {
                    self.unseen.remove(&(parent, name.to_owned()));
                    index.update(id, meta);
                    id
                }
                // A file that became a folder or back: the old Entry is pruned at the end.
                _ => index.insert(parent, name, meta),
            };
            if meta.is_dir {
                self.dirs.insert(path, id);
            }
        }
    }

    /// Removes Entries the listing no longer has.
    pub(crate) fn prune(self) {
        let gone: std::collections::HashSet<EntryId> = self.unseen.values().copied().collect();
        let mut by_parent: HashMap<EntryId, Vec<EntryId>> = HashMap::new();
        let mut index = self.index.write();
        for &id in &gone {
            // An Entry under a removed folder goes with it.
            if let Some(parent) = index.parent(id).filter(|p| !gone.contains(p)) {
                by_parent.entry(parent).or_default().push(id);
            }
        }
        for (parent, ids) in by_parent {
            index.remove_children(parent, &ids);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_parses_with_tabs_in_the_name_and_fractional_time() {
        let r = Record::parse(b"1234\t2\t5120\t16\t1700000000.5000000000\tf\tdir/odd\tname").unwrap();
        assert_eq!(
            r,
            Record {
                inode: 1234,
                links: 2,
                size: 5120,
                blocks: 16,
                modified: 1_700_000_000_500_000_000,
                is_dir: false,
                path: "dir/odd\tname".into(),
            }
        );
        assert!(Record::parse(b"1\t1\tx\t0\t0\tf\ta").is_none());
        assert_eq!(parse_time("-1.5"), Some(-1_500_000_000));
        assert_eq!(parse_time("-0.5"), Some(-500_000_000));
    }

    #[test]
    fn the_command_line_quotes_the_root() {
        assert_eq!(
            command_line("/srv/it's"),
            r"find '/srv/it'\''s' -xdev -printf '%i\t%n\t%s\t%b\t%T@\t%y\t%P\0'"
        );
    }

    #[test]
    fn failures_read_as_what_to_do() {
        let r = SshRemote { host: "me@box".into(), path: "/srv".into() };
        let why = |code, records, stderr: &str| failure(&r, code, records, stderr).map(|e| e.to_string());
        assert_eq!(why(Some(0), 3, ""), None);
        assert_eq!(why(Some(1), 3, "find: './x': Permission denied"), None);
        assert!(why(Some(255), 0, "me@box: Permission denied (publickey,password).").unwrap().contains("key or ssh-agent"));
        assert!(why(Some(255), 0, "ssh: Could not resolve hostname box").unwrap().contains("ssh to me@box failed"));
        assert!(why(Some(1), 0, "find: unknown predicate `-printf'").unwrap().contains("SFTP fallback (issue 16)"));
        assert!(why(Some(1), 0, "find: '/nope': No such file or directory").unwrap().contains("find on me@box failed"));
    }
}
