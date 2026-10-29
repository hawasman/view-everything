//! FTP Remotes (ADR-0006 addendum): walk an FTP server's folders in passive mode with MLSD where
//! `FEAT` offers it, else by parsing `LIST` (unix and DOS formats), and apply what is listed to
//! the Index. Read-only: only `AUTH`/`PBSZ`/`PROT`, login, `FEAT`, `TYPE`, `CWD`, `PASV`, `MLSD`/`LIST` and `QUIT` are ever sent.
//! Connections are explicit FTPS (`AUTH TLS`) unless the Remote allows plain FTP. The password is
//! in the OS keyring, through the same helper as S3's secrets.

use crate::remote::Applier;
use crate::s3::{forget_secret, read_secret};
use crate::{Progress, SharedIndex};
use chrono::{Datelike, NaiveDate, NaiveDateTime, Utc};
use rustls::{ClientConfig, RootCertStore};
use std::io;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use suppaftp::{FtpError, RustlsConnector, RustlsFtpStream};
use ve_core::{EntryId, EntryMeta, FtpRemote, FtpTls, SyncState, VolumeId};

/// Entries applied to the Index per write lock.
const BATCH: usize = 4096;

/// A server that is silent this long is gone.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Forgets the password kept for a removed Remote.
pub fn forget_password(remote: &FtpRemote) {
    forget_secret(&remote.identity());
}

// ---- listing lines ---------------------------------------------------------------------------

/// What one line of a listing says.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// A file or a folder.
    Item {
        name: String,
        is_dir: bool,
        size: u64,
        modified: i64,
        /// The MLSD `create` fact; 0 when the server does not say (LIST never does).
        created: i64,
    },
    /// Not an Entry: `.`/`..`, a symlink or special file, a `total` line, a blank.
    Skip,
    /// Not understood: the listing cannot be trusted to be complete.
    Bad,
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn nanos(t: NaiveDateTime) -> i64 {
    t.and_utc().timestamp_nanos_opt().unwrap_or(0)
}

/// Whitespace-separated fields of `line` with the byte offset each starts at.
fn fields(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                out.push((s, &line[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &line[s..]));
    }
    out
}

/// The name an Entry may have: not `.`, `..`, empty or holding a `/` we would split on.
fn usable(name: &str) -> bool {
    !(name.is_empty() || name == "." || name == ".." || name.contains('/'))
}

/// A name that cannot be an Entry: `.` and `..` are just skipped, but a name that is empty or holds
/// a `/` is an Entry we cannot record, so the listing is not complete.
fn unusable(name: &str) -> Line {
    if name == "." || name == ".." { Line::Skip } else { Line::Bad }
}

/// `Mon D HH:MM` or `Mon D YYYY` from `ls -l`, as nanoseconds. A time of day carries no year:
/// it is this year's, unless that is more than a day ahead of `now`, then last year's (`ls`
/// shows times only for the last six months). The server's time zone is not known, so times are
/// read as UTC: right to within a day, which is as honest as a LIST gets.
fn unix_time(month: &str, day: &str, year_or_time: &str, now: NaiveDateTime) -> Option<i64> {
    let month = MONTHS.iter().position(|m| month.eq_ignore_ascii_case(m))? as u32 + 1;
    let day: u32 = day.parse().ok()?;
    if let Some((h, m)) = year_or_time.split_once(':') {
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        let at = |year| NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(h, m, 0);
        let this = at(now.year()).filter(|t| *t <= now + chrono::Duration::days(1));
        // Feb 29 has no last year in most years: such a date is not honest either way.
        return Some(nanos(this.or_else(|| at(now.year() - 1))?));
    }
    let year: i32 = year_or_time.parse().ok()?;
    Some(nanos(NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(0, 0, 0)?))
}

/// One line of `ls -l`-style `LIST` output:
/// `-rw-r--r-- 1 owner group 1234 Nov  5 13:46 a name.txt`. The size is found as the number before
/// the month, so servers that omit the group still parse.
fn parse_unix(line: &str, now: NaiveDateTime) -> Line {
    let f = fields(line);
    let kind = f.first().and_then(|(_, perms)| perms.chars().next());
    if !f
        .first()
        .is_some_and(|(_, perms)| perms.len() >= 10 && perms.chars().skip(1).all(|c| "rwxsStT-+@.".contains(c)))
    {
        return Line::Bad;
    }
    // Month at fields i..i+3, size just before it, the name after. The first place that reads as
    // both wins, so an owner called Mar does not pass for a month.
    let found = (1..f.len().saturating_sub(3)).find_map(|i| {
        let size = f[i - 1].1.parse::<u64>().ok()?;
        Some((i, size, unix_time(f[i].1, f[i + 1].1, f[i + 2].1, now)?))
    });
    let Some((i, size, modified)) = found else { return Line::Bad };
    let name = line[f[i + 3].0..].trim_end_matches(['\r', '\n']);
    match kind {
        Some('d') if usable(name) => Line::Item {
            name: name.into(),
            is_dir: true,
            size: 0,
            modified,
            created: 0,
        },
        Some('-') if usable(name) => Line::Item {
            name: name.into(),
            is_dir: false,
            size,
            modified,
            created: 0,
        },
        // Symlinks are skipped; so are devices, pipes and sockets.
        Some('l' | 'c' | 'b' | 'p' | 's') => Line::Skip,
        Some('d' | '-') => unusable(name),
        _ => Line::Bad,
    }
}

/// One line of Windows (IIS) `LIST` output: `10-19-20  03:19PM       <DIR>          pub` or
/// `04-08-14  03:09PM                  403 readme.txt`.
fn parse_dos(line: &str) -> Line {
    let f = fields(line);
    if f.len() < 4 {
        return Line::Bad;
    }
    let date: Vec<u32> = f[0].1.split('-').filter_map(|p| p.parse().ok()).collect();
    let [month, day, year] = date[..] else { return Line::Bad };
    // IIS prints two digits: 70..99 are 19xx, 00..69 are 20xx.
    let year = if year < 70 {
        2000 + year
    } else if year < 100 {
        1900 + year
    } else {
        year
    };
    let time = f[1].1.to_ascii_uppercase();
    let (clock, pm) = match time.strip_suffix("PM") {
        Some(c) => (c, Some(true)),
        None => (time.strip_suffix("AM").unwrap_or(&time), time.ends_with("AM").then_some(false)),
    };
    let Some((h, m)) = clock
        .split_once(':')
        .and_then(|(h, m)| Some((h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)))
    else {
        return Line::Bad;
    };
    let h = match pm {
        Some(pm) => (h % 12) + if pm { 12 } else { 0 },
        None => h,
    };
    let Some(modified) = NaiveDate::from_ymd_opt(year as i32, month, day)
        .and_then(|d| d.and_hms_opt(h, m, 0))
        .map(nanos)
    else {
        return Line::Bad;
    };
    let name = line[f[3].0..].trim_end_matches(['\r', '\n']);
    let (is_dir, size) = if f[2].1.eq_ignore_ascii_case("<DIR>") {
        (true, 0)
    } else {
        match f[2].1.replace(',', "").parse() {
            Ok(size) => (false, size),
            Err(_) => return Line::Bad,
        }
    };
    if usable(name) {
        Line::Item {
            name: name.into(),
            is_dir,
            size,
            modified,
            created: 0,
        }
    } else {
        unusable(name)
    }
}

/// One line of `LIST` output in either format. `now` anchors the year of times without one.
pub fn parse_list_line(line: &str, now: NaiveDateTime) -> Line {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() || line.trim_start().to_ascii_lowercase().starts_with("total") {
        return Line::Skip;
    }
    if line.starts_with(|c: char| c.is_ascii_digit()) {
        parse_dos(line)
    } else {
        parse_unix(line, now)
    }
}

/// `YYYYMMDDHHMMSS[.sss]` (UTC), the `modify` and `create` facts of MLSD, as nanoseconds.
fn mlsd_time(text: &str) -> Option<i64> {
    let digits = text.split_once('.').map_or(text, |(d, _)| d);
    Some(nanos(NaiveDateTime::parse_from_str(digits, "%Y%m%d%H%M%S").ok()?))
}

/// One line of MLSD output (RFC 3659): `type=file;size=12;modify=20240102030405; name`. Only
/// `file` and `dir` are Entries; `cdir`, `pdir`, symlinks and anything else are skipped.
pub fn parse_mlsd_line(line: &str) -> Line {
    let line = line.trim_end_matches(['\r', '\n']);
    // The facts end at the first "; ": after that comes the name, which may hold anything.
    let Some((facts, name)) = line.split_once("; ") else {
        return if line.trim().is_empty() { Line::Skip } else { Line::Bad };
    };
    let (mut kind, mut size, mut modified, mut created) = (None, 0, 0, 0);
    for fact in facts.split(';') {
        let Some((key, value)) = fact.split_once('=') else { continue };
        match key.to_ascii_lowercase().as_str() {
            "type" => kind = Some(value.to_ascii_lowercase()),
            "size" => match value.parse() {
                Ok(s) => size = s,
                Err(_) => return Line::Bad,
            },
            // A server's missing or odd time is not a failed listing: the Entry has none.
            "modify" => modified = mlsd_time(value).unwrap_or(0),
            "create" => created = mlsd_time(value).unwrap_or(0),
            _ => {}
        }
    }
    match kind.as_deref() {
        Some("file") if usable(name) => Line::Item {
            name: name.into(),
            is_dir: false,
            size,
            modified,
            created,
        },
        Some("dir") if usable(name) => Line::Item {
            name: name.into(),
            is_dir: true,
            size: 0,
            modified,
            created,
        },
        Some("file" | "dir") => unusable(name),
        Some("cdir" | "pdir") => Line::Skip,
        // Symlinks (OS.unix=slink, OS.unix=symlink) and anything unknown are not Entries.
        Some(_) => Line::Skip,
        None => Line::Bad,
    }
}

// ---- the session -----------------------------------------------------------------------------

fn handshake_failure(host: &str, why: &str) -> io::Error {
    io::Error::other(format!(
        "FTPS handshake with {host} failed: {why} (servers with a private CA or a self-signed certificate are not supported)"
    ))
}

fn failure(remote: &FtpRemote, e: FtpError) -> io::Error {
    let host = format!("{}:{}", remote.host, remote.port);
    match e {
        FtpError::SecureError(e) => handshake_failure(&host, &e),
        // The handshake runs inside a socket read, so rustls's complaint arrives as an I/O error.
        FtpError::ConnectionError(e) if e.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()) => {
            handshake_failure(&host, &e.to_string())
        }
        FtpError::ConnectionError(e) => io::Error::new(e.kind(), format!("FTP {host}: {e}")),
        FtpError::UnexpectedResponse(r) if r.status.code() == 530 => io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{host} rejected the login: check the user and password"),
        ),
        e => io::Error::other(format!("FTP {host}: {e}")),
    }
}

pub(crate) fn web_roots_config() -> Arc<ClientConfig> {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    client_config(roots)
}

/// A TLS client trusting exactly `roots`.
pub fn client_config(roots: RootCertStore) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Connects, secures (unless plain FTP was chosen on purpose) and logs in.
fn connect(remote: &FtpRemote, password: &str, tls: Arc<ClientConfig>) -> io::Result<RustlsFtpStream> {
    let host = format!("{}:{}", remote.host, remote.port);
    let fail = |e| failure(remote, e);
    let addrs = (remote.host.as_str(), remote.port)
        .to_socket_addrs()
        .map_err(|e| io::Error::new(e.kind(), format!("{host}: {e}")))?;
    // Every address the name has, in order: `localhost` may be ::1 first on a server bound to IPv4.
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
    let mut ftp = None;
    for addr in addrs {
        match RustlsFtpStream::connect_timeout(addr, Duration::from_secs(15)) {
            Ok(stream) => {
                ftp = Some(stream);
                break;
            }
            Err(e) => last = failure(remote, e),
        }
    }
    let mut ftp = ftp.ok_or(last)?;
    ftp.get_ref().set_read_timeout(Some(TIMEOUT))?;
    ftp.get_ref().set_write_timeout(Some(TIMEOUT))?;
    // Passive mode (the default), reaching the data port on the address we dialled: servers behind
    // NAT announce an address of their own that we could not reach, or that is somebody else's.
    ftp.set_passive_nat_workaround(true);
    if remote.tls == FtpTls::Explicit {
        ftp = ftp.into_secure(RustlsConnector::from(tls), &remote.host).map_err(|e| match e {
            // A refusal of AUTH itself (suppaftp reads 534 as no known status): a busy (421) or login (530) reply is not "no FTPS".
            FtpError::UnexpectedResponse(r) if !matches!(r.status.code(), 421 | 530) => io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{host} does not offer FTPS (AUTH TLS): turn on Allow plain FTP to connect without encryption"),
            ),
            e => failure(remote, e),
        })?;
    }
    ftp.login(remote.user.as_str(), password).map_err(fail)?;
    Ok(ftp)
}

/// The listing of the working folder: MLSD if `mlsd` and the server answers it, else LIST.
/// A server that turns out not to know MLSD despite `FEAT` clears `mlsd` for the rest of the walk.
fn list_folder(ftp: &mut RustlsFtpStream, mlsd: &mut bool, now: NaiveDateTime) -> Result<Vec<Line>, FtpError> {
    if *mlsd {
        match ftp.mlsd(None) {
            Ok(lines) => return Ok(lines.iter().map(|l| parse_mlsd_line(l)).collect()),
            Err(FtpError::UnexpectedResponse(r)) if matches!(r.status.code(), 500 | 501 | 502 | 504) => *mlsd = false,
            Err(e) => return Err(e),
        }
    }
    Ok(ftp.list(None)?.iter().map(|l| parse_list_line(l, now)).collect())
}

/// Walks every folder under `root`, giving each Entry's path relative to it (parents first) and
/// metadata to `emit`. Returns how many folders or lines could not be read: such a listing is
/// not complete, and nothing may be pruned after it.
fn walk(
    ftp: &mut RustlsFtpStream,
    root: &str,
    cancel: &std::sync::atomic::AtomicBool,
    mut emit: impl FnMut(String, EntryMeta),
) -> io::Result<u64> {
    let mut mlsd = ftp.feat().is_ok_and(|f| f.keys().any(|k| k.eq_ignore_ascii_case("MLST")));
    let now = Utc::now().naive_utc();
    let (mut unreadable, mut stack) = (0, vec![(String::new(), root.to_owned())]);
    while let Some((rel, abs)) = stack.pop() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let lines = match ftp.cwd(abs.as_str()).and_then(|_| list_folder(ftp, &mut mlsd, now)) {
            Ok(lines) => lines,
            // The root must be there; a folder that is refused (550) is just unreadable.
            Err(FtpError::UnexpectedResponse(r)) if !rel.is_empty() && r.status.code() / 100 == 5 => {
                unreadable += 1;
                continue;
            }
            Err(FtpError::UnexpectedResponse(r)) if r.status.code() == 550 => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{root} is not a folder on the server"),
                ));
            }
            Err(e) => return Err(io::Error::other(e.to_string())),
        };
        for line in lines {
            match line {
                Line::Item {
                    name,
                    is_dir,
                    size,
                    modified,
                    created,
                } => {
                    let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                    // Folders carry no size of their own: Folder Size is the sum of their contents.
                    let (logical, on_disk) = if is_dir { (0, 0) } else { (size, size) };
                    emit(
                        path.clone(),
                        EntryMeta {
                            is_dir,
                            logical,
                            on_disk,
                            modified,
                            created,
                            ..EntryMeta::default()
                        },
                    );
                    if is_dir {
                        stack.push((path, format!("{}/{name}", abs.trim_end_matches('/'))));
                    }
                }
                Line::Skip => {}
                Line::Bad => unreadable += 1,
            }
        }
    }
    Ok(unreadable)
}

/// Brings an FTP Remote's Entries up to date with what its server lists now.
pub fn sync(index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    sync_with(index, volume, progress, &|remote| password(remote), web_roots_config())
}

/// The password kept in the keyring. A public server has none to keep.
pub(crate) fn password(remote: &FtpRemote) -> Result<String, String> {
    match read_secret(&remote.identity()) {
        Err(_) if remote.is_anonymous() => Ok("anonymous@".into()),
        Err(e) => Err(e.replace("the secret key is", "the password is")),
        Ok(p) => Ok(p),
    }
}

/// The Test of the Remote dialog: `check` with the typed password, else the one in the keyring.
pub fn check_remote(r: &FtpRemote, secret: Option<&str>) -> io::Result<()> {
    let password = match secret {
        Some(p) => p.to_owned(),
        None => password(r).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?,
    };
    check(r, &password, web_roots_config())
}

/// Logs in to `remote` with `password` and enters its folder, trusting `tls` for FTPS.
pub fn check(remote: &FtpRemote, password: &str, tls: Arc<ClientConfig>) -> io::Result<()> {
    let mut ftp = connect(remote, password, tls)?;
    let entered = ftp.cwd(remote.path.as_str()).map_err(|e| match e {
        FtpError::UnexpectedResponse(r) if r.status.code() == 550 => {
            io::Error::new(io::ErrorKind::NotFound, format!("{} is not a folder on the server", remote.path))
        }
        e => failure(remote, e),
    });
    let _ = ftp.quit();
    entered
}

/// [`sync`] with `auth` giving the password and `tls` the trust for FTPS (test doubles, or the
/// keyring and the bundled web roots).
pub fn sync_with(
    index: &SharedIndex,
    volume: VolumeId,
    progress: &Progress,
    auth: &dyn Fn(&FtpRemote) -> Result<String, String>,
    tls: Arc<ClientConfig>,
) -> io::Result<()> {
    let (root, remote): (EntryId, FtpRemote) = {
        let index = index.read();
        let v = index
            .volume(volume)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such Remote"))?;
        match &v.sync {
            SyncState::Ftp(remote) => (v.root, remote.clone()),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "not an FTP Remote")),
        }
    };
    let password = auth(&remote).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
    let mut ftp = connect(&remote, &password, tls)?;
    let mut applier = Applier::new(index, root, progress);
    let mut batch = Vec::new();
    let walked = walk(&mut ftp, &remote.path, &progress.cancel, |path, meta| {
        batch.push((path, meta));
        if batch.len() >= BATCH {
            applier.apply(&mut batch);
        }
    });
    applier.apply(&mut batch);
    let _ = ftp.quit();
    let unreadable = walked?;
    progress.unreadable.fetch_add(unreadable, Ordering::Relaxed);
    // Only a listing that reached every folder says what is gone.
    if unreadable == 0 {
        applier.prune();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        nanos(NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_opt(h, mi, 0).unwrap())
    }

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 6, 15).unwrap().and_hms_opt(12, 0, 0).unwrap()
    }

    fn item(name: &str, is_dir: bool, size: u64, modified: i64) -> Line {
        Line::Item {
            name: name.into(),
            is_dir,
            size,
            modified,
            created: 0,
        }
    }

    #[test]
    fn unix_lines_parse_with_year_or_time_and_odd_names() {
        let p = |l: &str| parse_list_line(l, now());
        assert_eq!(
            p("-rw-r--r--   1 user group     1234 Nov  5  2018 example.txt"),
            item("example.txt", false, 1234, at(2018, 11, 5, 0, 0))
        );
        assert_eq!(
            p("drwxr-xr-x   2 0  0  4096 Jan 10 09:30 my folder"),
            item("my folder", true, 0, at(2026, 1, 10, 9, 30))
        );
        // A time of day in a month still to come is last year's (ls shows times for six months).
        assert_eq!(
            p("-rw-r--r-- 1 u g 7 Dec 31 23:59 old.log"),
            item("old.log", false, 7, at(2025, 12, 31, 23, 59))
        );
        // Within a day of now, a time stays this year (the server's clock may run ahead).
        assert_eq!(
            p("-rw-r--r-- 1 u g 7 Jun 16 01:00 fresh"),
            item("fresh", false, 7, at(2026, 6, 16, 1, 0))
        );
        // No group column, a name with leading text that looks like a month, and a double space.
        assert_eq!(
            p("-rw-r--r-- 1 owner 99 Mar  3  2020 Mar  4  2021.txt"),
            item("Mar  4  2021.txt", false, 99, at(2020, 3, 3, 0, 0))
        );
        // Symlinks, devices, `total`, blanks, `.` and `..` are not Entries.
        assert_eq!(p("lrwxrwxrwx 1 u g 11 Nov  5  2018 link -> target"), Line::Skip);
        assert_eq!(p("crw-rw-rw- 1 u g 1, 3 Nov  5  2018 null"), Line::Skip);
        assert_eq!(p("total 24"), Line::Skip);
        assert_eq!(p(""), Line::Skip);
        assert_eq!(p("drwxr-xr-x 2 u g 4096 Nov  5  2018 .."), Line::Skip);
        // A name we cannot record makes the listing incomplete, so it is not pruned after.
        assert_eq!(p("-rw-r--r-- 1 u g 5 Nov  5  2018 a/b"), Line::Bad);
        assert_eq!(parse_mlsd_line("type=file;size=1; x/y"), Line::Bad);
        // Anything else is not understood.
        assert_eq!(p("-rw-r--r-- 1 u g notasize Nov  5  2018 x"), Line::Bad);
        assert_eq!(p("what is this"), Line::Bad);
        assert_eq!(p("-rw-r--r-- 1 u g 5 Foo  5  2018 x"), Line::Bad);
    }

    #[test]
    fn dos_lines_parse_with_two_digit_years_and_12_hour_clock() {
        let p = |l: &str| parse_list_line(l, now());
        assert_eq!(
            p("10-19-20  03:19PM       <DIR>          pub"),
            item("pub", true, 0, at(2020, 10, 19, 15, 19))
        );
        assert_eq!(
            p("04-08-14  12:09AM                  403 read me.txt"),
            item("read me.txt", false, 403, at(2014, 4, 8, 0, 9))
        );
        assert_eq!(
            p("01-02-1999  12:00PM 1,234,567 big.bin"),
            item("big.bin", false, 1_234_567, at(1999, 1, 2, 12, 0))
        );
        assert_eq!(p("12-31-98  11:59PM 5 old"), item("old", false, 5, at(1998, 12, 31, 23, 59)));
        assert_eq!(p("13-45-20  03:19PM <DIR> bad"), Line::Bad);
    }

    #[test]
    fn mlsd_lines_parse_files_and_folders_and_skip_the_rest() {
        assert_eq!(
            parse_mlsd_line("type=file;size=1234;modify=20240102030405.123; a; b.txt"),
            item("a; b.txt", false, 1234, at(2024, 1, 2, 3, 4) + 5_000_000_000)
        );
        assert_eq!(
            parse_mlsd_line("Type=DIR;Modify=20240102030405;UNIX.mode=0755; sub"),
            item("sub", true, 0, at(2024, 1, 2, 3, 4) + 5_000_000_000)
        );
        assert_eq!(
            parse_mlsd_line("type=file;size=1;modify=20240102030405;create=20230101000000; made"),
            Line::Item {
                name: "made".into(),
                is_dir: false,
                size: 1,
                modified: at(2024, 1, 2, 3, 4) + 5_000_000_000,
                created: at(2023, 1, 1, 0, 0),
            }
        );
        assert_eq!(parse_mlsd_line("type=cdir;modify=20240102030405; ."), Line::Skip);
        assert_eq!(parse_mlsd_line("type=pdir; .."), Line::Skip);
        assert_eq!(parse_mlsd_line("type=OS.unix=slink:/x;size=1; link"), Line::Skip);
        // No usable time is just no time; a broken size or no type is a listing we cannot trust.
        assert_eq!(parse_mlsd_line("type=file;size=3; nodate"), item("nodate", false, 3, 0));
        assert_eq!(parse_mlsd_line("type=file;size=x; f"), Line::Bad);
        assert_eq!(parse_mlsd_line("size=3; f"), Line::Bad);
        assert_eq!(parse_mlsd_line("garbage"), Line::Bad);
    }

    #[test]
    fn a_remote_is_an_identity_and_is_checked() {
        let r = FtpRemote::new(" ftp.example.com ", 2121, "me", "/pub/", FtpTls::Explicit).unwrap();
        assert_eq!(r.identity(), "ftp://me@ftp.example.com:2121/pub");
        assert_eq!(
            FtpRemote::new("h", 21, "me", "", FtpTls::Plain).unwrap().identity(),
            "ftp://me@h:21/"
        );
        assert!(FtpRemote::new("a@b", 21, "me", "/", FtpTls::Explicit).is_err());
        assert!(FtpRemote::new("h", 0, "me", "/", FtpTls::Explicit).is_err());
        assert!(FtpRemote::new("h", 21, "", "/", FtpTls::Explicit).is_err());
        assert!(FtpRemote::new("h", 21, "me", "pub", FtpTls::Explicit).is_err());
    }
}
