//! The Engine's local socket (ADR-0016): newline-delimited JSON over a Unix socket the user alone
//! can open. It is a thin adapter over the in-process [`Engine`] handle.
//!
//! A connection starts with a handshake: the client sends `{"version": N}` and the engine answers
//! `{"version": N, "engine": "..."}`, or refuses a different version with
//! `{"error": "...", "expected": VERSION}` and closes. Then each request is one line,
//! `{"id": ..., "method": "...", "params": ...}`, and its replies carry the same id:
//!
//! - a single reply: `{"id", "result"}`;
//! - a stream: repeated `{"id", "item"}` lines ending with `{"id", "end"}`;
//! - a failure: `{"id", "error"}`.
//!
//! Methods: `echo` (result: the params), `show` (raises the app's window), `launcher` (opens the
//! Launcher, or closes it when open), `search` (streams [`search::Row`]s for a
//! [`search::Search`]; end: `{"count", "last_synced"}`), `launch.match` (streams the Launcher's
//! [`crate::launch::Match`]es for `{"text"}`; end: `{"count"}`), `tasks` (streams running Tasks'
//! progress; with `{"follow": true}` until none runs; end: `{}`), and the agent methods
//! `agents.pending`, `agents.approve` and one per Module agents read (`files`, `performance`,
//! `network`, `apps`; see [`agents`]).

mod agents;
pub mod search;
#[cfg(windows)]
mod windows;

use crate::Engine;
use crate::tasks::{Outcome, TaskInfo};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The protocol version a client must open with.
pub const VERSION: u64 = 1;

/// Overrides where the socket is (a sandboxed copy of the app, tests). Its folder must be the
/// user's alone (0700) or not exist yet; the lock file `engine.lock` goes beside it.
pub const PATH_ENV: &str = "VE_SOCKET";

/// How often a followed `tasks` stream looks at the running Tasks.
const TASKS_POLL: Duration = Duration::from_millis(250);

/// What a client asks the app to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// The app's window (`show`).
    Window,
    /// The Launcher (`launcher`).
    Launcher,
}

/// Called when a client asks the app to show its window or its Launcher.
pub type Show = Arc<dyn Fn(Surface) + Send + Sync>;

/// Where the socket is: `$VE_SOCKET`, else `$XDG_RUNTIME_DIR/view-everything/engine.sock`, else a
/// per-user folder in the temporary folder. On Windows a named pipe, `\\.\pipe\view-everything-<SID>-engine`
/// (`$VE_SOCKET` names it, or is turned into a pipe name). `None` when the user cannot be identified.
pub fn path() -> Option<PathBuf> {
    let custom = std::env::var_os(PATH_ENV).filter(|p| !p.is_empty());
    #[cfg(windows)]
    return windows::path(custom.as_deref());
    #[cfg(unix)]
    {
        if let Some(path) = custom {
            return Some(path.into());
        }
        let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|p| !p.is_empty()) {
            Some(runtime) => PathBuf::from(runtime).join(ve_core::paths::APP_DIR),
            // SAFETY: getuid has no preconditions and cannot fail.
            None => std::env::temp_dir().join(format!("{}-{}", ve_core::paths::APP_DIR, unsafe { libc::getuid() })),
        };
        Some(dir.join("engine.sock"))
    }
}

/// Why a client could not talk to the engine.
#[derive(Debug)]
pub enum Error {
    /// The socket could not be reached or broke off.
    Io(io::Error),
    /// The engine refused the request (or the handshake), with its reason.
    Refused(String),
}

impl Error {
    /// Nothing is listening: no app runs (a stale socket file counts as none).
    pub fn is_not_running(&self) -> bool {
        matches!(self, Error::Io(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            Error::Refused(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

fn send(out: &mut impl Write, line: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, line)?;
    out.write_all(b"\n")
}

/// A connection that has passed the handshake.
pub struct Client<S: io::Read + Write = Stream> {
    reader: BufReader<S>,
    writer: BufWriter<S>,
    next_id: u64,
}

/// How long a client waits for the engine's handshake answer before giving up on it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The transport: a Unix socket.
#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;
/// The transport: a named pipe.
#[cfg(windows)]
pub type Stream = windows::Stream;

impl Client {
    /// Connects to the engine at `path` and shakes hands, giving up on a host that does not
    /// answer within a few seconds.
    pub fn connect(path: &Path) -> Result<Client, Error> {
        #[cfg(unix)]
        {
            let stream = Stream::connect(path)?;
            stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
            let client = Client::handshake(stream.try_clone()?, stream).map_err(timed_out)?;
            client.reader.get_ref().set_read_timeout(None)?;
            Ok(client)
        }
        #[cfg(windows)]
        {
            let stream = windows::connect(path)?;
            let read = stream.try_clone()?;
            windows::with_deadline(&stream, HANDSHAKE_TIMEOUT, || Client::handshake(read, stream.try_clone()?))
        }
    }
}

/// A read that timed out is not "nothing running" and not a refusal: it says so.
#[cfg(unix)]
fn timed_out(e: Error) -> Error {
    match e {
        Error::Io(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
            Error::Io(io::Error::new(io::ErrorKind::TimedOut, "the engine did not answer in time"))
        }
        other => other,
    }
}

impl<S: io::Read + Write> Client<S> {
    fn handshake(read: S, write: S) -> Result<Client<S>, Error> {
        let mut client = Client {
            reader: BufReader::new(read),
            writer: BufWriter::new(write),
            next_id: 0,
        };
        send(&mut client.writer, &json!({ "version": VERSION }))?;
        client.writer.flush()?;
        let reply = client.read()?;
        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Err(Error::Refused(error.to_owned()));
        }
        Ok(client)
    }

    fn read(&mut self) -> Result<Value, Error> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(Error::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        serde_json::from_str(&line).map_err(|e| Error::Io(e.into()))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<u64, Error> {
        self.next_id += 1;
        let id = self.next_id;
        send(&mut self.writer, &json!({ "id": id, "method": method, "params": params }))?;
        self.writer.flush()?;
        Ok(id)
    }

    /// The next reply to request `id`, as (key, value): `result`, `item`, `end` or an error. An
    /// error without an id (a line the engine could not read) is this request's too.
    fn reply(&mut self, id: u64) -> Result<(&'static str, Value), Error> {
        loop {
            let mut reply = self.read()?;
            let ours = match reply.get("id") {
                None | Some(Value::Null) => reply.get("error").is_some(),
                Some(theirs) => theirs.as_u64() == Some(id),
            };
            if !ours {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(Error::Refused(error.as_str().unwrap_or("unknown error").to_owned()));
            }
            for key in ["result", "item", "end"] {
                if let Some(value) = reply.get_mut(key) {
                    return Ok((key, value.take()));
                }
            }
        }
    }

    /// Sends one request and returns its single result.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.request(method, params)?;
        match self.reply(id)? {
            ("result", value) => Ok(value),
            (key, _) => Err(Error::Refused(format!("`{method}` replied with a stream ({key})"))),
        }
    }

    /// Sends one request, hands each streamed item to `item`, and returns the end line's value.
    pub fn stream(&mut self, method: &str, params: Value, mut item: impl FnMut(Value) -> Result<(), Error>) -> Result<Value, Error> {
        let id = self.request(method, params)?;
        loop {
            match self.reply(id)? {
                ("item", value) => item(value)?,
                (_, value) => return Ok(value),
            }
        }
    }
}

/// What a process found when it tried to host the engine socket.
pub enum Claim {
    /// The socket is ours: serve it with [`serve`].
    Host(Listener),
    /// The app already runs here and answered.
    Running(Client),
    /// Something answers that refused us (another version of the app), with its reason.
    Incompatible(String),
}

/// A bound socket, private to this user, not yet served. It holds the host lock for as long as
/// it lives.
pub struct Listener {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixListener,
    #[cfg(unix)]
    _lock: std::fs::File,
    #[cfg(windows)]
    inner: windows::Pipe,
}

/// How long a launch that lost the host lock waits for the winner's socket to answer.
const CLAIM_WAIT: Duration = Duration::from_secs(5);

/// Makes this process the engine's host, unless an app already runs here. Whoever holds an
/// exclusive lock on `engine.lock` beside the socket hosts, so two launches at once never both
/// do; the winner replaces a socket file left by a crash, binds, and makes it 0600 in a folder
/// private to the user (0700). The loser connects to the winner, waiting for it to bind.
#[cfg(unix)]
pub fn claim(path: &Path) -> io::Result<Claim> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path.with_extension("lock"))?;
    // SAFETY: flock on a descriptor this function owns; the kernel drops the lock with it.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let inner = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        return Ok(Claim::Host(Listener { inner, _lock: lock }));
    }
    let locked = io::Error::last_os_error();
    if locked.kind() != io::ErrorKind::WouldBlock {
        return Err(locked);
    }
    await_host(path)
}

/// Makes this process the engine's host, unless an app already runs here: whoever creates the
/// pipe's first instance hosts (the pipe is the user's alone, remote clients refused); a launch
/// that cannot create it connects to the host instead, waiting for it to answer.
#[cfg(windows)]
pub fn claim(path: &Path) -> io::Result<Claim> {
    match windows::Pipe::claim(path) {
        Ok(inner) => Ok(Claim::Host(Listener { inner })),
        // ERROR_ACCESS_DENIED: the name is taken by a host's first instance.
        Err(e) if e.raw_os_error() == Some(windows::ACCESS_DENIED) => await_host(path),
        Err(e) => Err(e),
    }
}

/// Connects to the host that holds the engine socket, waiting for it to start answering.
fn await_host(path: &Path) -> io::Result<Claim> {
    let deadline = std::time::Instant::now() + CLAIM_WAIT;
    loop {
        match Client::connect(path) {
            Ok(client) => return Ok(Claim::Running(client)),
            Err(Error::Refused(reason)) => return Ok(Claim::Incompatible(reason)),
            Err(e) if e.is_not_running() && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(Error::Io(e)) => return Err(e),
        }
    }
}

/// Creates `dir` for the user alone (0700), or checks an existing one is the user's and closed
/// to everyone else. Anything else is refused rather than changed: others could reach or replace
/// the socket, and some systems ignore a socket file's own permissions.
#[cfg(unix)]
fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        other => return other,
    }
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: getuid has no preconditions and cannot fail.
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a folder this user alone can open", dir.display()),
        ));
    }
    Ok(())
}

/// Answers connections on `listener` from a background thread for as long as the process
/// runs, each connection on its own thread.
pub fn serve(listener: Listener, engine: Arc<Engine>, show: Show) {
    #[cfg(unix)]
    std::thread::Builder::new()
        .name("engine socket".into())
        .spawn(move || {
            // The whole Listener, lock included, lives as long as this thread (a closure would
            // capture only the field it uses).
            let listener = listener;
            for stream in listener.inner.incoming() {
                let Ok(stream) = stream else { continue };
                let (engine, show) = (engine.clone(), show.clone());
                let _ = std::thread::Builder::new().name("engine client".into()).spawn(move || {
                    let Ok(read) = stream.try_clone() else { return };
                    // A client that hangs up mid-reply is its own business.
                    let _ = connection(read, stream, &engine, &show);
                });
            }
        })
        .expect("spawning the socket thread");
    #[cfg(windows)]
    std::thread::Builder::new()
        .name("engine socket".into())
        .spawn(move || {
            let mut listener = listener;
            loop {
                let stream = match listener.inner.accept() {
                    Ok(stream) => stream,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                };
                let (engine, show) = (engine.clone(), show.clone());
                let _ = std::thread::Builder::new().name("engine client".into()).spawn(move || {
                    let Ok(read) = stream.try_clone() else { return };
                    let _ = connection(read, stream, &engine, &show);
                });
            }
        })
        .expect("spawning the socket thread");
}

/// The longest request line the engine reads; a client sending more is refused and dropped.
const MAX_LINE: u64 = 1024 * 1024;

/// Reads one line of at most [`MAX_LINE`] bytes into `line` (0 at end of input).
fn read_capped(reader: &mut impl BufRead, line: &mut String) -> io::Result<usize> {
    let n = io::Read::take(&mut *reader, MAX_LINE + 1).read_line(line)?;
    if n as u64 > MAX_LINE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request line too long"));
    }
    Ok(n)
}

/// Serves one connection: the handshake, then requests until the client hangs up.
fn connection(read: impl io::Read, write: impl Write, engine: &Engine, show: &Show) -> io::Result<()> {
    let mut reader = BufReader::new(read);
    let mut out = BufWriter::new(write);
    match serve_lines(&mut reader, &mut out, engine, show) {
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            send(&mut out, &json!({ "id": null, "error": format!("unreadable request: {e}") }))?;
            out.flush()
        }
        other => other,
    }
}

fn serve_lines(reader: &mut impl BufRead, out: &mut BufWriter<impl Write>, engine: &Engine, show: &Show) -> io::Result<()> {
    let mut line = String::new();
    if read_capped(reader, &mut line)? == 0 {
        return Ok(());
    }
    let version = serde_json::from_str::<Value>(&line)
        .ok()
        .and_then(|hello| hello.get("version").and_then(Value::as_u64));
    if version != Some(VERSION) {
        let theirs = version.map_or("no version".to_owned(), |v| format!("version {v}"));
        send(
            out,
            &json!({
                "error": format!("this engine speaks protocol version {VERSION}; the client sent {theirs}"),
                "expected": VERSION,
            }),
        )?;
        return out.flush();
    }
    send(
        out,
        &json!({ "version": VERSION, "engine": concat!("view-everything ", env!("CARGO_PKG_VERSION")) }),
    )?;
    out.flush()?;
    loop {
        line.clear();
        if read_capped(reader, &mut line)? == 0 {
            return Ok(());
        }
        let request = match serde_json::from_str::<Value>(&line) {
            Ok(request) => request,
            Err(e) => {
                send(out, &json!({ "id": null, "error": format!("not a JSON request: {e}") }))?;
                out.flush()?;
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        match request.get("method").and_then(Value::as_str).unwrap_or("") {
            "echo" => send(out, &json!({ "id": id, "result": params }))?,
            "show" => {
                show(Surface::Window);
                send(out, &json!({ "id": id, "result": null }))?;
            }
            "launcher" => {
                show(Surface::Launcher);
                send(out, &json!({ "id": id, "result": null }))?;
            }
            "launch.match" => answer_launch_match(out, &id, &params, engine)?,
            "search" => answer_search(out, &id, params, engine)?,
            "tasks" => answer_tasks(out, &id, &params, engine)?,
            method if agents::answers(method) => agents::answer(out, &id, method, params, engine)?,
            other => send(out, &json!({ "id": id, "error": format!("unknown method `{other}`") }))?,
        }
        out.flush()?;
    }
}

fn answer_search(out: &mut impl Write, id: &Value, params: Value, engine: &Engine) -> io::Result<()> {
    let parsed = match params {
        Value::Null => Ok(search::Search::default()),
        params => serde_json::from_value::<search::Search>(params).map_err(|e| format!("bad search: {e}")),
    };
    let found = parsed.and_then(|search| engine.find_rows(&search));
    match found {
        Ok(found) => {
            for row in &found.rows {
                send(out, &json!({ "id": id, "item": row }))?;
            }
            send(
                out,
                &json!({ "id": id, "end": { "count": found.rows.len(), "last_synced": found.last_synced } }),
            )
        }
        Err(e) => send(out, &json!({ "id": id, "error": e })),
    }
}

fn answer_launch_match(out: &mut impl Write, id: &Value, params: &Value, engine: &Engine) -> io::Result<()> {
    let text = params.get("text").and_then(Value::as_str).unwrap_or_default();
    match engine.launch_match(text) {
        Ok(found) => {
            for row in &found {
                send(out, &json!({ "id": id, "item": row }))?;
            }
            send(out, &json!({ "id": id, "end": { "count": found.len() } }))
        }
        Err(e) => send(out, &json!({ "id": id, "error": e })),
    }
}

fn task_item(task: &TaskInfo) -> Value {
    json!({
        "task": task.id,
        "kind": task.kind.name(),
        "label": task.label,
        "done": task.done,
        "total": task.total,
        "unit": task.kind.unit(),
    })
}

/// Streams each running Task's progress. Following, it then streams each change, and a last
/// item carrying `outcome` for each Task that finishes (short ones included), until none runs.
fn answer_tasks(out: &mut impl Write, id: &Value, params: &Value, engine: &Engine) -> io::Result<()> {
    let follow = params.get("follow").and_then(Value::as_bool).unwrap_or(false);
    let tasks = &engine.shared.tasks;
    // Tasks that finished before the stream began are not news.
    let mut finished: HashSet<u64> = tasks.finished().iter().map(|t| t.id).collect();
    let mut sent: HashMap<u64, (u64, Option<u64>, String)> = HashMap::new();
    loop {
        let running = tasks.running();
        for task in &running {
            let now = (task.done, task.total, task.label.clone());
            if sent.get(&task.id) != Some(&now) {
                send(out, &json!({ "id": id, "item": task_item(task) }))?;
                sent.insert(task.id, now);
            }
        }
        if !follow {
            return send(out, &json!({ "id": id, "end": {} }));
        }
        for info in tasks.finished() {
            if finished.insert(info.id) {
                let mut item = task_item(&info);
                item["outcome"] = match info.outcome {
                    Some(Outcome::Done(summary)) => json!({ "done": summary }),
                    Some(Outcome::Failed(reason)) => json!({ "failed": reason }),
                    Some(Outcome::Cancelled) | None => json!("cancelled"),
                };
                send(out, &json!({ "id": id, "item": item }))?;
            }
        }
        sent.retain(|task, _| running.iter().any(|t| t.id == *task));
        if running.is_empty() {
            return send(out, &json!({ "id": id, "end": {} }));
        }
        out.flush()?;
        std::thread::sleep(TASKS_POLL);
    }
}
