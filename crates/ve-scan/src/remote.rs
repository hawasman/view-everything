//! SSH Remotes (ADR-0005): run the system `ssh` binary with `find <root> -xdev -printf ...` on
//! the server, stream its output and apply it to the Index as it arrives. Nothing is installed
//! on the server, or, where that `find` is not GNU's, walk the tree over SFTP through the same
//! `ssh` ([`crate::sftp`]). `BatchMode` means keys and the agent only: a password-only host reports an error
//! instead of prompting.

use crate::{Progress, SharedIndex, sftp};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use ve_core::{Capacity, EntryId, EntryMeta, Remote, SyncState, VolumeId};

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
    /// Bytes allocated (find: 512-byte blocks; SFTP: the Logical Size, as it reports no allocation).
    pub on_disk: u64,
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
        Some(Record { inode, links, size, on_disk: blocks * 512, modified, is_dir: kind == b"d", path })
    }

    fn meta(&self) -> EntryMeta {
        // Folders carry no size of their own: Folder Size is the sum of their contents.
        let file = !self.is_dir;
        EntryMeta {
            is_dir: self.is_dir,
            logical: if file { self.size } else { 0 },
            on_disk: if file { self.on_disk } else { 0 },
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
    let known: Vec<_> = index
        .volumes()
        .filter_map(|(id, v)| match &v.sync {
            SyncState::Ssh(r) => Some((id, r.clone())),
            _ => None,
        })
        .collect();
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
    sync_with("ssh", index, volume, progress)
}

/// Remotes known to lack GNU `find -printf`, so the next sync goes straight to SFTP.
///
/// ponytail: remembered for the session only; persisting it needs a field on `Remote` in ve-core.
static SFTP_ONLY: Mutex<Vec<Remote>> = Mutex::new(Vec::new());

/// [`sync`] through the `ssh` program `program` (a test double, or the system one): over remote
/// `find`, or over SFTP when the server has no GNU `find -printf` (remembered per Remote).
pub fn sync_with(program: &str, index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    let (root, remote) = {
        let index = index.read();
        let v = index.volume(volume).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such Remote"))?;
        match &v.sync {
            SyncState::Ssh(remote) => (v.root, remote.clone()),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a Remote")),
        }
    };
    list(program, index, root, &remote, progress)?;
    // Capacity is a bonus: a host without a working `df` just has none, and the sync stands.
    let capacity = df_with(program, &remote).ok();
    index.write().set_capacity(volume, capacity);
    Ok(())
}

/// Whether the Remote's server is known to lack GNU `find -printf` (it is indexed over SFTP).
pub fn is_sftp_only(remote: &Remote) -> bool {
    SFTP_ONLY.lock().contains(remote)
}

fn list(program: &str, index: &SharedIndex, root: EntryId, remote: &Remote, progress: &Progress) -> io::Result<()> {
    if !is_sftp_only(remote) {
        match sync_find(program, index, root, remote, progress) {
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {}
            done => return done,
        }
    }
    sync_sftp(program, index, root, remote, progress)?;
    let mut only = SFTP_ONLY.lock();
    if !only.contains(remote) {
        only.push(remote.clone());
    }
    Ok(())
}

/// The capacity of the filesystem holding the Remote's root, from `df -kP` run over `program`
/// (ssh) with the options the listing uses; so it works on SFTP-only hosts that can run `df`.
pub fn df_with(program: &str, remote: &Remote) -> io::Result<Capacity> {
    let mut child = spawn(program, remote, false, &format!("df -kP {}", shell_quote(&remote.path)), Stdio::null())?;
    let stderr = drain_stderr(&mut child);
    let mut out = String::new();
    // df prints a few lines; a server printing more than 64 KiB is not answering df.
    child.stdout.take().expect("piped").take(64 * 1024).read_to_string(&mut out)?;
    let status = child.wait()?;
    let stderr = stderr.join().unwrap_or_default();
    if !status.success() {
        return Err(io::Error::other(format!("df on {} failed: {}", remote.host, stderr.trim().lines().last().unwrap_or(""))));
    }
    parse_df(&out).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "df printed no capacity"))
}

/// Parses `df -kP` output (GNU, BusyBox and BSD agree on the POSIX format): a header, then
/// `Filesystem 1024-blocks Used Available Capacity Mounted-on`. The filesystem and mount names
/// may hold spaces, so the figures are found around the `NN%` column. A server that prints
/// `-` for sizes (some FUSE mounts) has no capacity.
pub fn parse_df(text: &str) -> Option<Capacity> {
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let percent = fields.iter().skip(3).position(|f| f.ends_with('%') || *f == "-")? + 3;
    let kib = |f: &str| f.parse::<u64>().ok()?.checked_mul(1024);
    let (total, free) = (kib(fields[percent - 3])?, kib(fields[percent - 1])?);
    (total > 0).then_some(Capacity { total, free: free.min(total) })
}

/// Starts `program` (ssh) with the options every run shares; `command` runs on the server, or
/// with `subsystem` names the subsystem to request.
fn spawn(program: &str, remote: &Remote, subsystem: bool, command: &str, stdin: Stdio) -> io::Result<Child> {
    let mut ssh = Command::new(program);
    ssh.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=15"]);
    if subsystem {
        ssh.arg("-s");
    }
    ssh.args(["--", &remote.host, command])
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => io::Error::new(io::ErrorKind::NotFound, "the ssh program was not found"),
            _ => e,
        })
}

/// Collects ssh's stderr on a thread: a full pipe would block ssh, and the sync with it.
fn drain_stderr(child: &mut Child) -> std::thread::JoinHandle<String> {
    let mut stderr = child.stderr.take().expect("piped");
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_string(&mut text);
        let _ = io::copy(&mut stderr, &mut io::sink());
        text
    })
}

fn sync_find(program: &str, index: &SharedIndex, root: EntryId, remote: &Remote, progress: &Progress) -> io::Result<()> {
    let mut child = spawn(program, remote, false, &command_line(&remote.path), Stdio::null())?;
    let stderr = drain_stderr(&mut child);
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
                    Some(record) => batch.push(record),
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
    if let Some(e) = failure(remote, status.code(), applier.records, &stderr) {
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

/// Indexes a Remote over `ssh -s host sftp`. As with find, Entries are pruned only after a
/// listing that reached every folder.
fn sync_sftp(program: &str, index: &SharedIndex, root: EntryId, remote: &Remote, progress: &Progress) -> io::Result<()> {
    let mut child = spawn(program, remote, true, "sftp", Stdio::piped())?;
    let stderr = drain_stderr(&mut child);
    let (stdin, stdout) = (child.stdin.take().expect("piped"), child.stdout.take().expect("piped"));
    let mut applier = Applier::new(index, root, progress);
    let mut batch = Vec::new();
    let walked = sftp::walk(BufReader::new(stdout), stdin, &remote.path, |record| {
        batch.push(record);
        if batch.len() >= BATCH {
            applier.apply(&mut batch);
        }
    });
    applier.apply(&mut batch);
    // The walk dropped ssh's stdin and stdout, so ssh ends on its own; its exit code says why a
    // failed walk failed (killing it here would race with that).
    let status = child.wait()?;
    let stderr = stderr.join().unwrap_or_default();
    let walked = walked.map_err(|e| match status.code() {
        Some(255) => failure(remote, Some(255), 0, &stderr).unwrap_or(e),
        _ => io::Error::new(e.kind(), format!("SFTP on {} failed: {e} {}", remote.host, stderr.trim().lines().last().unwrap_or(""))),
    })?;
    progress.unreadable.fetch_add(walked.unreadable, Ordering::Relaxed);
    if walked.unreadable == 0 {
        applier.prune();
    }
    Ok(())
}

/// Why the run failed, in words for the user, or `None` if it produced a listing.
fn failure(remote: &Remote, code: Option<i32>, records: u64, stderr: &str) -> Option<io::Error> {
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
        // No GNU find -printf (BusyBox, BSD) or no find at all: the caller falls back to SFTP.
        _ if code == Some(127) || stderr.contains("-printf") => {
            err(io::ErrorKind::Unsupported, format!("{host} has no GNU find -printf"))
        }
        _ => err(io::ErrorKind::Other, format!("find on {host} failed: {message}")),
    }
}

/// Applies records to the Index, creating, updating and (at the end) pruning Entries.
///
/// ponytail: a refresh maps every existing Entry by (parent, name) in memory; a stream-diff
/// would need less but the Remotes this is for are one machine's folders.
struct Applier<'a> {
    index: &'a SharedIndex,
    progress: &'a Progress,
    root: EntryId,
    /// Folder path relative to the root -> its Entry. The root is the empty path.
    dirs: HashMap<String, EntryId>,
    /// Entries already in the Index that the listing has not mentioned yet.
    unseen: HashMap<(EntryId, String), EntryId>,
    records: u64,
}

impl<'a> Applier<'a> {
    fn new(index: &'a SharedIndex, root: EntryId, progress: &'a Progress) -> Self {
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

    fn apply(&mut self, batch: &mut Vec<Record>) {
        if batch.is_empty() {
            return;
        }
        let mut index = self.index.write();
        for record in batch.drain(..) {
            self.records += 1;
            self.progress.entries.fetch_add(1, Ordering::Relaxed);
            let meta = record.meta();
            if record.path.is_empty() {
                index.update(self.root, meta);
                continue;
            }
            let (parent_path, name) = record.path.rsplit_once('/').unwrap_or(("", &record.path));
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
                self.dirs.insert(record.path, id);
            }
        }
    }

    /// Removes Entries the listing no longer has.
    fn prune(self) {
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
                on_disk: 16 * 512,
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
    fn df_output_parses_from_gnu_busybox_and_bsd() {
        let gnu = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n/dev/nvme0n1p2   495911220 382096892 104909188      79% /\n";
        let want = Some(Capacity { total: 495_911_220 * 1024, free: 104_909_188 * 1024 });
        assert_eq!(parse_df(gnu), want);
        // BusyBox: the same columns, the filesystem name may be long or contain spaces.
        let busybox = "Filesystem           1024-blocks    Used Available Capacity Mounted on\n/dev/root               495911220 382096892 104909188  79% /\n";
        assert_eq!(parse_df(busybox), want);
        let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on\nmy disk 10 4 6 40% /mnt/with 5% sign\n";
        assert_eq!(parse_df(spaced), Some(Capacity { total: 10 * 1024, free: 6 * 1024 }));
        // BSD/macOS with -kP: POSIX columns, 1K blocks.
        let bsd = "Filesystem  1024-blocks     Used Available Capacity  Mounted on\n/dev/disk3s1s1   494384795 11491048 307610816     4%    /\n";
        assert_eq!(parse_df(bsd), Some(Capacity { total: 494_384_795 * 1024, free: 307_610_816 * 1024 }));
        // Long names wrapped by a non-POSIX df put the figures on the next line: still the last line.
        assert_eq!(parse_df("Filesystem 1024-blocks Used Available Capacity Mounted on\nserver:/very/long\n   100 40 60 40% /n\n").map(|c| c.total), Some(100 * 1024));
        // No figures, or nothing: no capacity.
        assert_eq!(parse_df("Filesystem 1024-blocks Used Available Capacity Mounted on\nfuse - - - - /mnt\n"), None);
        assert_eq!(parse_df("df: /nope: No such file or directory\n"), None);
        assert_eq!(parse_df(""), None);
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
        let r = Remote::new("me@box", "/srv").unwrap();
        let why = |code, records, stderr: &str| failure(&r, code, records, stderr).map(|e| e.to_string());
        assert_eq!(why(Some(0), 3, ""), None);
        assert_eq!(why(Some(1), 3, "find: './x': Permission denied"), None);
        assert!(why(Some(255), 0, "me@box: Permission denied (publickey,password).").unwrap().contains("key or ssh-agent"));
        assert!(why(Some(255), 0, "ssh: Could not resolve hostname box").unwrap().contains("ssh to me@box failed"));
        assert_eq!(failure(&r, Some(1), 0, "find: unknown predicate `-printf'").unwrap().kind(), io::ErrorKind::Unsupported);
        assert_eq!(failure(&r, Some(127), 0, "sh: find: not found").unwrap().kind(), io::ErrorKind::Unsupported);
        assert!(why(Some(1), 0, "find: '/nope': No such file or directory").unwrap().contains("find on me@box failed"));
    }
}
