//! Background priority for the threads of a Task the app started on its own: the operating
//! system's lowest CPU and disk priority, so the machine's own work goes first.
//!
//! Only a thread spawned for that Task may be lowered: on Linux an unprivileged thread cannot
//! raise its priority again, so a lowered thread stays low until it ends.

use std::cell::Cell;
use std::io;

thread_local! {
    static LOWERED: Cell<bool> = const { Cell::new(false) };
}

/// Lowers the calling thread to background priority for the rest of its life: on Linux the idle
/// scheduling class (`SCHED_IDLE`, nice 19) and the idle I/O class; on Windows background
/// processing mode; on macOS the Darwin background band (`PRIO_DARWIN_BG`), which throttles
/// both CPU and disk.
pub fn lower_current_thread() -> io::Result<()> {
    lower()?;
    LOWERED.with(|l| l.set(true));
    Ok(())
}

/// Whether [`lower_current_thread`] lowered the calling thread; the threads a walk spawns
/// from it are lowered too.
pub fn current_thread_lowered() -> bool {
    LOWERED.with(Cell::get)
}

/// Lowers the calling thread when `background`: for the threads a walk or a search spawns,
/// called with what [`current_thread_lowered`] said on the thread that spawned them.
pub(crate) fn inherit(background: bool) {
    if background {
        let _ = lower_current_thread();
    }
}

#[cfg(target_os = "linux")]
fn lower() -> io::Result<()> {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    const IOPRIO_CLASS_IDLE: libc::c_long = 3;
    const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
    let check = |r: libc::c_long| if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) };
    // SAFETY: plain syscalls on the calling thread, with arguments built here.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        let param = libc::sched_param { sched_priority: 0 };
        // On Linux both act on the one thread `tid` (0: the caller), not the whole process.
        check(libc::sched_setscheduler(0, libc::SCHED_IDLE, &param).into())?;
        check(libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 19).into())?;
        check(libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            tid,
            IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT,
        ))
    }
}

#[cfg(target_os = "macos")]
fn lower() -> io::Result<()> {
    // SAFETY: sets the calling thread's own Darwin priority band.
    let r = unsafe { libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(windows)]
fn lower() -> io::Result<()> {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
    // SAFETY: the pseudo handle of the calling thread, put in background processing mode.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn lower() -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "no background priority here"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_lowered_thread_runs_at_idle_cpu_and_disk_priority() {
        std::thread::spawn(|| {
            assert!(!current_thread_lowered());
            lower_current_thread().unwrap();
            assert!(current_thread_lowered());
            // SAFETY: reads of the calling thread's own scheduling state.
            let (policy, nice, ioprio) = unsafe {
                let tid = libc::syscall(libc::SYS_gettid);
                (
                    libc::sched_getscheduler(0),
                    libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t),
                    libc::syscall(libc::SYS_ioprio_get, 1, tid),
                )
            };
            assert_eq!((policy, nice, ioprio >> 13), (libc::SCHED_IDLE, 19, 3));
        })
        .join()
        .unwrap();
        // The test's own thread is untouched.
        assert!(!current_thread_lowered());
    }
}
