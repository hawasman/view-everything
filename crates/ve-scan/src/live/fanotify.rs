//! The app's end of the opt-in `fanotify` helper (ADR-0009).
//!
//! The helper (`ve-fanotify`, a separate program) runs as root, started with pkexec only while
//! the app runs. It reads fanotify events for the Volumes it is told about and writes the changed
//! paths to its stdout, each followed by a NUL byte; `!lost` stands for an overflowed queue and `!ready` says that every Volume it was
//! started with is being watched. It
//! reads `mark <mount>` lines from stdin to watch Volumes mounted later, and exits at the end of
//! stdin, which is how it stops when the app does. It writes no files.

use super::Event;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

/// The helper's file name, next to the app or in the packaged libexec folder.
pub const HELPER: &str = "ve-fanotify";
const LIBEXEC: &str = "/usr/libexec/view-everything";

/// The helper pkexec will run: it must have an absolute path.
pub fn find_helper() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok()?.with_file_name(HELPER);
    [beside, Path::new(LIBEXEC).join(HELPER)].into_iter().find(|p| p.is_file())
}

/// What a helper that ended without being asked to means for the user.
pub fn describe_exit(code: Option<i32>) -> String {
    match code {
        Some(126) => "Authentication was dismissed".to_owned(),
        Some(127) => "Authentication failed or the helper is not allowed to run".to_owned(),
        Some(2) => "This kernel does not allow the helper to watch filesystems".to_owned(),
        Some(c) => format!("The helper stopped (exit code {c})"),
        None => "The helper was stopped by a signal".to_owned(),
    }
}

/// A running helper. Dropping it ends the helper.
pub struct Helper {
    stdin: Mutex<Option<ChildStdin>>,
    stopped: Arc<AtomicBool>,
}

impl Helper {
    /// Asks for authentication with pkexec and starts `program` on `mounts`. Changes go to `events`.
    /// `ready` is called once the helper is watching (after authentication). `ended` is called, with what to tell the user, if the helper ends on its own.
    pub fn start(
        program: &Path,
        mounts: &[PathBuf],
        events: Sender<Event>,
        ready: impl FnOnce() + Send + 'static,
        ended: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<Helper> {
        let mut command = Command::new("pkexec");
        command.arg(program).args(mounts);
        Helper::spawn(command, events, ready, ended)
    }

    fn spawn(
        mut command: Command,
        events: Sender<Event>,
        ready: impl FnOnce() + Send + 'static,
        ended: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<Helper> {
        let mut child: Child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout was piped");
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        thread::Builder::new().name("ve-fanotify".into()).spawn(move || {
            read_events(
                BufReader::new(stdout),
                |event| {
                    let _ = events.send(event);
                },
                ready,
            );
            let code = child.wait().ok().and_then(|s| s.code());
            if !flag.load(Ordering::Relaxed) {
                ended(describe_exit(code));
            }
        })?;
        Ok(Helper {
            stdin: Mutex::new(stdin),
            stopped,
        })
    }

    /// Tells the helper to watch Volumes mounted since it started.
    pub fn watch(&self, mounts: &[PathBuf]) {
        if let Some(stdin) = self.stdin.lock().unwrap().as_mut() {
            for mount in mounts {
                // A mount path with a line break cannot be said in this protocol; skip it.
                if let Some(path) = mount.to_str().filter(|p| !p.contains('\n')) {
                    let _ = writeln!(stdin, "mark {path}");
                }
            }
            let _ = stdin.flush();
        }
    }

    /// Ends the helper by closing its stdin. It is root's process, so it cannot be killed from here.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.stdin.lock().unwrap().take();
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Reads the helper's output until its end, turning each record into an [`Event`].
pub fn read_events(mut from: impl BufRead, mut send: impl FnMut(Event), ready: impl FnOnce()) {
    let mut ready = Some(ready);
    let mut record = Vec::new();
    loop {
        record.clear();
        match from.read_until(0, &mut record) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if record.last() == Some(&0) {
            record.pop();
        }
        if record == b"!ready" {
            if let Some(ready) = ready.take() {
                ready();
            }
        } else if record == b"!lost" {
            send(Event::Lost);
        } else if record.first() == Some(&b'/') {
            use std::os::unix::ffi::OsStringExt;
            send(Event::Changed(PathBuf::from(std::ffi::OsString::from_vec(record.clone()))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn records_are_nul_separated_paths_and_a_lost_marker() {
        let mut got = Vec::new();
        let mut was_ready = false;
        read_events(
            &b"/a/b c\0/d\n e\0!ready\0!lost\0junk\0/last"[..],
            |e| got.push(e),
            || was_ready = true,
        );
        assert!(was_ready);
        assert_eq!(
            got,
            [
                Event::Changed("/a/b c".into()),
                Event::Changed("/d\n e".into()),
                Event::Lost,
                Event::Changed("/last".into()),
            ]
        );
    }

    #[test]
    fn a_helper_that_ends_on_its_own_is_reported_with_its_exit_code() {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let mut sh = Command::new("sh");
        sh.args(["-c", "printf '/x/y\\0'; exit 126"]);
        let _helper = Helper::spawn(sh, tx, || (), move |why| done_tx.send(why).unwrap()).unwrap();
        assert_eq!(done_rx.recv().unwrap(), "Authentication was dismissed");
        assert_eq!(rx.recv().unwrap(), Event::Changed("/x/y".into()));
    }

    #[test]
    fn stopping_closes_stdin_and_is_not_reported_as_a_failure() {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel::<String>();
        let mut sh = Command::new("sh");
        // Like the helper: runs until the end of stdin, and passes on what it was told to watch.
        sh.args(["-c", "while read verb path; do printf '%s\\0' \"$path\"; done"]);
        let helper = Helper::spawn(sh, tx, || (), move |why| done_tx.send(why).unwrap()).unwrap();
        helper.watch(&["/mnt/usb".into()]);
        assert_eq!(rx.recv().unwrap(), Event::Changed("/mnt/usb".into()));
        helper.stop();
        // The reader ends when the shell does; nothing is reported.
        assert!(rx.recv().is_err());
        assert!(done_rx.try_recv().is_err());
    }
}
