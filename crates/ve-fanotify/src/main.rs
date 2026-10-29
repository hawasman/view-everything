//! Reports file changes on the filesystems it is given, and nothing else (ADR-0009).
//!
//! Started by view-everything through pkexec, only when the user turned live updates on, and
//! only while the app runs. Usage: `ve-fanotify <mount>...`.
//!
//! Privileges: fanotify on a whole filesystem and `open_by_handle_at` (to turn the kernel's file
//! handles into paths) need `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH`, so the helper runs as
//! root. It opens files with `O_PATH` only to read their path, never reads file contents, and
//! writes nothing except the changed paths on stdout (each followed by a NUL byte; `!lost`
//! after a queue overflow, `!ready` once watching). stdin takes `mark <mount>` lines for filesystems mounted later; at
//! the end of stdin, or when the parent dies, it exits.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(linux::run(std::env::args_os().skip(1).collect()));
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ve-fanotify works on Linux only");
    std::process::exit(2);
}
