//! The Windows transport: a named pipe only the current user can open, remote clients refused.
//!
//! The pipe is `\\.\pipe\view-everything-<user SID>-engine`. Creating its first instance with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` is the single-instance lock: a second host's create fails,
//! so it becomes a client instead. The host keeps one instance listening at all times.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW};
use windows_sys::Win32::Security::{GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The error code of a create that finds the pipe already hosted.
pub const ACCESS_DENIED: i32 = ERROR_ACCESS_DENIED as i32;

/// The prefix every pipe path has.
const PIPE_PREFIX: &str = r"\\.\pipe\";

/// How long a client waits for a busy pipe to have a free instance, per attempt.
const BUSY_WAIT_MS: u32 = 2000;

/// How many busy waits a client tries before giving up.
const BUSY_TRIES: u32 = 3;

/// The transport: one end of a named pipe.
pub type Stream = File;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// The current user's SID as text (`S-1-5-21-...`).
fn current_user_sid() -> io::Result<String> {
    // SAFETY: plain Win32 calls on handles and buffers owned here; each out-pointer is valid for
    // the call, the token is closed, and the string LocalAlloc'd by Windows is freed after copying.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut needed = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            // u64 elements keep the buffer aligned for TOKEN_USER.
            let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
            if GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed) == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            let mut text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Ok(sid)
        })();
        CloseHandle(token);
        result
    }
}

/// The pipe path: `$VE_SOCKET` when it already is a pipe path, a name derived from it otherwise,
/// else one per user. `None` when the user cannot be identified.
pub fn path(custom: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(custom) = custom {
        let text = custom.to_string_lossy();
        if text.starts_with(PIPE_PREFIX) {
            return Some(PathBuf::from(custom));
        }
        let name: String = text
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        return Some(PathBuf::from(format!("{PIPE_PREFIX}{}-{name}-engine", ve_core::paths::APP_DIR)));
    }
    let sid = current_user_sid().ok()?;
    Some(PathBuf::from(format!("{PIPE_PREFIX}{}-{sid}-engine", ve_core::paths::APP_DIR)))
}

/// Opens the pipe at `path`, waiting a little when every instance is busy.
pub fn connect(path: &Path) -> io::Result<Stream> {
    let name = wide(path.as_os_str());
    let mut tries = 0;
    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && tries < BUSY_TRIES => {
                tries += 1;
                // SAFETY: `name` is a NUL-terminated wide string living across the call.
                unsafe { WaitNamedPipeW(name.as_ptr(), BUSY_WAIT_MS) };
            }
            other => return other,
        }
    }
}

/// Runs a blocking read/write sequence on `stream` that is abandoned after `limit`: the pending
/// I/O is cancelled and fails with `TimedOut`. A pipe has no read timeout of its own.
pub fn with_deadline<T, E: From<io::Error>>(stream: &File, limit: Duration, run: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let handle = stream.as_raw_handle() as usize;
    let (done, wait) = mpsc::channel::<()>();
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = timed_out.clone();
    let watchdog = std::thread::spawn(move || {
        if wait.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
            flag.store(true, Ordering::SeqCst);
            // SAFETY: the handle outlives this thread: the caller joins it before returning.
            unsafe { CancelIoEx(handle as HANDLE, std::ptr::null()) };
        }
    });
    let result = run();
    let _ = done.send(());
    let _ = watchdog.join();
    match result {
        Err(_) if timed_out.load(Ordering::SeqCst) => {
            Err(io::Error::new(io::ErrorKind::TimedOut, "the engine did not answer in time").into())
        }
        other => other,
    }
}

/// Whether a pipe of this name has an instance, free or busy (checked without connecting). The
/// first-instance flag is the real lock; this also keeps a host from replacing a live one on
/// systems that do not enforce the flag.
fn exists(path: &Path) -> bool {
    let name = wide(path.as_os_str());
    // SAFETY: `name` is a NUL-terminated wide string living across the call.
    unsafe { WaitNamedPipeW(name.as_ptr(), 1) != 0 || GetLastError() != ERROR_FILE_NOT_FOUND }
}

/// The host side: the pipe's instances.
pub struct Pipe {
    name: Vec<u16>,
    sddl: Vec<u16>,
    first: Option<File>,
}

impl Pipe {
    /// Creates the first instance, or fails (access denied) when a host already has the pipe.
    pub fn claim(path: &Path) -> io::Result<Pipe> {
        if exists(path) {
            return Err(io::Error::from_raw_os_error(ACCESS_DENIED));
        }
        let sddl = format!("D:P(A;;GA;;;{})", current_user_sid()?);
        let mut pipe = Pipe {
            name: wide(path.as_os_str()),
            sddl: wide(OsStr::new(&sddl)),
            first: None,
        };
        pipe.first = Some(pipe.instance(true)?);
        Ok(pipe)
    }

    fn instance(&self, first: bool) -> io::Result<File> {
        // SAFETY: the descriptor is converted from a NUL-terminated string, used only during the
        // create call, then freed; the returned handle is owned by the File that wraps it.
        unsafe {
            let mut descriptor = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(self.sddl.as_ptr(), 1, &mut descriptor, std::ptr::null_mut()) == 0 {
                return Err(io::Error::last_os_error());
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let open_mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
            let handle = CreateNamedPipeW(
                self.name.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &attributes,
            );
            let created = if handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(handle)
            };
            LocalFree(descriptor);
            Ok(File::from_raw_handle(created? as RawHandle))
        }
    }

    /// Waits for the next client and returns its end of the pipe. A fresh instance is listening
    /// before this returns, so the pipe name never vanishes.
    pub fn accept(&mut self) -> io::Result<File> {
        let listening = match self.first.take() {
            Some(first) => first,
            None => self.instance(false)?,
        };
        // SAFETY: the handle is a valid pipe instance owned by `listening`; no OVERLAPPED is used.
        let connected = unsafe { ConnectNamedPipe(listening.as_raw_handle() as HANDLE, std::ptr::null_mut()) };
        if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            let e = io::Error::last_os_error();
            self.first = Some(listening);
            return Err(e);
        }
        self.first = Some(self.instance(false)?);
        Ok(listening)
    }
}
