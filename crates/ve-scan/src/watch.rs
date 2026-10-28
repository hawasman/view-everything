//! Noticing Volumes mounted or unmounted while the app runs.
//!
//! Linux waits for the kernel to flag a change of `/proc/self/mountinfo` (`POLLPRI`), so a
//! newly mounted Volume is seen at once and an idle app uses no CPU. Windows and macOS compare the
//! list of mounted Volumes every few seconds; their native events (`WM_DEVICECHANGE`,
//! DiskArbitration) need a message loop or run loop that this thread does not own.

use crate::{VolumeInfo, discover_volumes};
use std::thread;
use std::time::Duration;

/// Changes arrive in bursts (a disk with several partitions); wait for the burst to end.
#[cfg(target_os = "linux")]
const SETTLE: Duration = Duration::from_millis(300);
/// How often platforms without a change notification look again.
#[cfg(not(target_os = "linux"))]
const POLL: Duration = Duration::from_secs(3);

/// Starts a thread that calls `on_change` with the Volumes mounted now whenever they differ
/// from the last look, the first being `initial`.
pub fn watch(initial: Vec<VolumeInfo>, on_change: impl FnMut(Vec<VolumeInfo>) + Send + 'static) {
    let spawned = thread::Builder::new()
        .name("ve-watch".into())
        .spawn(move || run(initial, mount_waiter(), discover_volumes, on_change));
    if let Err(e) = spawned {
        eprintln!("Cannot watch for new Volumes: {e}");
    }
}

/// Reports each change to what `look` finds, once `wait` says something may have changed;
/// ends when `wait` returns false.
fn run(
    mut last: Vec<VolumeInfo>,
    mut wait: impl FnMut() -> bool,
    mut look: impl FnMut() -> Vec<VolumeInfo>,
    mut on_change: impl FnMut(Vec<VolumeInfo>),
) {
    while wait() {
        let now = look();
        if now != last {
            on_change(now.clone());
            last = now;
        }
    }
}

/// A function that blocks until the operating system may have mounted or unmounted a
/// filesystem, and returns true (it never gives up).
#[cfg(target_os = "linux")]
pub fn mount_waiter() -> impl FnMut() -> bool {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open("/proc/self/mountinfo").ok();
    move || {
        let Some(file) = &file else {
            thread::sleep(Duration::from_secs(3));
            return true;
        };
        let mut fd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLPRI | libc::POLLERR,
            revents: 0,
        };
        // SAFETY: `fd` is one valid pollfd and the count says so; the file outlives the call.
        // Waking re-arms the next poll, as the kernel compares against the last event seen.
        let woke = unsafe { libc::poll(&mut fd, 1, -1) };
        if woke > 0 {
            thread::sleep(SETTLE);
        }
        true
    }
}

#[cfg(not(target_os = "linux"))]
pub fn mount_waiter() -> impl FnMut() -> bool {
    || {
        thread::sleep(POLL);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(mount: &str) -> VolumeInfo {
        VolumeInfo {
            mount: mount.into(),
            identity: format!("uuid:{mount}"),
            fs_type: "ext4".into(),
            label: String::new(),
            removable: true,
            network: false,
            read_only: false,
        }
    }

    #[test]
    fn only_a_changed_list_of_volumes_is_reported() {
        let (a, b) = (volume("/a"), volume("/b"));
        let first = vec![a.clone()];
        let mut looks = vec![
            vec![a.clone()],            // woken, nothing new
            vec![a.clone(), b.clone()], // plugged in
            vec![a.clone()],            // unplugged
        ]
        .into_iter();
        let mut wakes = 3;
        let mut seen = Vec::new();
        run(
            first,
            || {
                wakes -= 1;
                wakes >= 0
            },
            || looks.next().unwrap(),
            |now| seen.push(now.len()),
        );
        assert_eq!(seen, vec![2, 1]);
    }

    #[test]
    fn a_returning_offline_volume_is_queued_for_catch_up_by_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let usb = VolumeInfo { mount: dir.path().into(), ..volume("/x") };
        let index = crate::SharedIndex::new(ve_core::Index::new());
        crate::reconcile(&index, std::slice::from_ref(&usb), std::slice::from_ref(&usb));
        let first = vec![usb.clone()];
        let mut looks = vec![vec![], vec![usb.clone()]].into_iter();
        let (mut wakes, mut caught_up) = (2, Vec::new());
        run(
            first,
            || {
                wakes -= 1;
                wakes >= 0
            },
            || looks.next().unwrap(),
            |now| {
                // What the window does: the Volume is wanted whenever it is mounted.
                let (added, _) = crate::reconcile(&index, &now, &now);
                caught_up.push((now.len(), added.len(), index.read().volumes().all(|(_, v)| v.online)));
            },
        );
        // Unplugged: offline, nothing to sync. Back: online again and queued to catch up.
        assert_eq!(caught_up, vec![(0, 0, false), (1, 1, true)]);
    }

    /// The Linux wait really is woken by the kernel: a mount in a private mount namespace
    /// (made with `unshare`, not touching the user's disks) wakes it.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wait_returns_when_the_mount_table_changes() {
        use std::process::Command;
        let probe = Command::new("unshare").args(["-Urm", "true"]).status();
        if !probe.is_ok_and(|s| s.success()) {
            eprintln!("skipped: cannot create a mount namespace here");
            return;
        }
        // The namespace is private, so the test re-runs itself inside one: the wait is armed in
        // the child, which then mounts a tmpfs and must be woken.
        let exe = std::env::current_exe().unwrap();
        let out = Command::new("unshare")
            .args(["-Urm"])
            .arg(exe)
            .args(["--exact", "watch::tests::namespace_child", "--ignored", "--nocapture"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "run inside a mount namespace by linux_wait_returns_when_the_mount_table_changes"]
    fn namespace_child() {
        let mut wait = mount_waiter();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let mounter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            let ok = std::process::Command::new("mount")
                .args(["-t", "tmpfs", "tmpfs"])
                .arg(&path)
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "mount failed");
        });
        let started = std::time::Instant::now();
        wait();
        assert!(started.elapsed() >= Duration::from_millis(250), "returned before the mount");
        mounter.join().unwrap();
    }
}
