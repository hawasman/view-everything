//! FSEvents on macOS (ADR-0009): live changes while the app runs, and the changes since a
//! saved event id replayed at launch.
//!
//! Direct CoreServices bindings: the few functions needed are declared here. The stream is
//! delivered on a dispatch queue, so no run loop is needed.

use super::Event;
use std::ffi::{CStr, OsStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;

type Id = u64;
type Flags = u32;
type CfIndex = isize;
type Callback = extern "C" fn(*const c_void, *mut c_void, usize, *mut c_void, *const Flags, *const Id);

const UTF8: u32 = 0x0800_0100;
const NO_DEFER: Flags = 0x02;
const WATCH_ROOT: Flags = 0x04;
const FILE_EVENTS: Flags = 0x10;

const MUST_SCAN_SUBDIRS: Flags = 0x01;
const USER_DROPPED: Flags = 0x02;
const KERNEL_DROPPED: Flags = 0x04;
const IDS_WRAPPED: Flags = 0x08;
const HISTORY_DONE: Flags = 0x10;
const ROOT_CHANGED: Flags = 0x20;

/// Seconds FSEvents may gather changes before delivering them.
const LATENCY: f64 = 1.0;

#[repr(C)]
struct StreamContext {
    version: CfIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: Callback,
        context: *const StreamContext,
        paths: *const c_void,
        since: Id,
        latency: f64,
        flags: Flags,
    ) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(stream: *mut c_void, queue: *mut c_void);
    fn FSEventStreamStart(stream: *mut c_void) -> u8;
    fn FSEventStreamStop(stream: *mut c_void);
    fn FSEventStreamInvalidate(stream: *mut c_void);
    fn FSEventStreamRelease(stream: *mut c_void);
    fn FSEventsGetCurrentEventId() -> Id;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(allocator: *const c_void, bytes: *const u8, len: CfIndex, encoding: u32, external: u8) -> *const c_void;
    fn CFArrayCreate(allocator: *const c_void, values: *const *const c_void, count: CfIndex, callbacks: *const c_void) -> *const c_void;
    fn CFRelease(object: *const c_void);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

/// The current event id: what to replay from at the next launch if nothing was applied yet.
pub fn current_id() -> u64 {
    // SAFETY: takes no arguments and returns a number.
    unsafe { FSEventsGetCurrentEventId() }
}

/// A running stream. Dropping it stops the stream.
pub struct Stream {
    stream: *mut c_void,
    queue: *mut c_void,
    /// The callback's sender, freed after the stream is stopped.
    sender: *mut SyncSender<Event>,
}

// SAFETY: the pointers are only used to stop and release the stream when it is dropped.
unsafe impl Send for Stream {}

impl Stream {
    /// Follows changes under `mounts`, first replaying those after event id `since`.
    pub fn start(mounts: &[PathBuf], since: Id, events: SyncSender<Event>) -> Result<Stream, String> {
        let strings: Vec<*const c_void> = mounts
            .iter()
            .map(|m| {
                let bytes = m.as_os_str().as_bytes();
                // SAFETY: `bytes` is valid for its length; the string is released below.
                unsafe { CFStringCreateWithBytes(std::ptr::null(), bytes.as_ptr(), bytes.len() as CfIndex, UTF8, 0) }
            })
            .collect();
        // No callbacks: the array neither retains nor releases; the stream copies it.
        // SAFETY: `strings` holds `len` valid CFStrings.
        let paths = unsafe { CFArrayCreate(std::ptr::null(), strings.as_ptr(), strings.len() as CfIndex, std::ptr::null()) };
        let sender = Box::into_raw(Box::new(events));
        let context = StreamContext {
            version: 0,
            info: sender.cast(),
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        // SAFETY: `paths` is a valid array and the context is copied by the call; the callback
        // matches `Callback`.
        let stream = unsafe {
            FSEventStreamCreate(
                std::ptr::null(),
                deliver,
                &context,
                paths,
                since,
                LATENCY,
                FILE_EVENTS | NO_DEFER | WATCH_ROOT,
            )
        };
        // SAFETY: each object was created above and is no longer needed by us.
        unsafe {
            CFRelease(paths);
            strings.iter().for_each(|&s| CFRelease(s));
        }
        if stream.is_null() {
            // SAFETY: the stream was not created, so nothing else holds the sender.
            drop(unsafe { Box::from_raw(sender) });
            return Err("FSEvents refused the stream".to_owned());
        }
        // SAFETY: a valid stream; the queue is serial (null attributes) and kept until drop.
        let queue = unsafe { dispatch_queue_create(c"ve-fsevents".as_ptr(), std::ptr::null()) };
        // SAFETY: the stream and the queue are valid (the queue is checked just here).
        let running = !queue.is_null()
            && unsafe {
                FSEventStreamSetDispatchQueue(stream, queue);
                FSEventStreamStart(stream) != 0
            };
        if queue.is_null() {
            // SAFETY: never started, so no callback can run.
            unsafe {
                FSEventStreamRelease(stream);
                drop(Box::from_raw(sender));
            }
            return Err("FSEvents could not make its queue".to_owned());
        }
        let this = Stream { stream, queue, sender };
        if running {
            Ok(this)
        } else {
            Err("FSEvents could not start the stream".to_owned())
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: stopping, invalidating and releasing in that order is how a stream ends; once
        // it is, no callback runs, so the sender can be freed.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            // A callback already running on the queue finishes before the sender is freed.
            dispatch_sync_f(self.queue, std::ptr::null_mut(), barrier);
            dispatch_release(self.queue);
            drop(Box::from_raw(self.sender));
        }
    }
}

extern "C" fn barrier(_: *mut c_void) {}

extern "C" fn deliver(_: *const c_void, info: *mut c_void, count: usize, paths: *mut c_void, flags: *const Flags, ids: *const Id) {
    // SAFETY: `info` is the sender given at creation; FSEvents passes `count` C-string paths,
    // flags and ids (we did not ask for CFTypes).
    let (sender, paths, flags, ids) = unsafe {
        (
            &*info.cast::<SyncSender<Event>>(),
            std::slice::from_raw_parts(paths.cast::<*const c_char>(), count),
            std::slice::from_raw_parts(flags, count),
            std::slice::from_raw_parts(ids, count),
        )
    };
    for ((&path, &flag), &id) in paths.iter().zip(flags).zip(ids) {
        // SAFETY: each path is a valid C string for the duration of the callback.
        let path = PathBuf::from(OsStr::from_bytes(unsafe { CStr::from_ptr(path) }.to_bytes()));
        // The end of the replay marks no change.
        if flag & HISTORY_DONE == 0 {
            let _ = sender.send(classify(path, flag));
        }
        let _ = sender.send(Event::Position(id));
    }
}

/// What a reported path and its flags mean. Missed or merged changes (including a refused
/// replay) call for a re-walk.
fn classify(path: PathBuf, flags: Flags) -> Event {
    if flags & (IDS_WRAPPED | ROOT_CHANGED) != 0 {
        Event::Lost
    } else if flags & (MUST_SCAN_SUBDIRS | USER_DROPPED | KERNEL_DROPPED) != 0 {
        Event::Subtree(path)
    } else {
        Event::Changed(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_and_wrapped_events_call_for_a_re_walk() {
        let p = || PathBuf::from("/a/b");
        assert_eq!(classify(p(), 0x1000), Event::Changed(p()));
        assert_eq!(classify(p(), MUST_SCAN_SUBDIRS | KERNEL_DROPPED), Event::Subtree(p()));
        assert_eq!(classify(p(), IDS_WRAPPED), Event::Lost);
        assert_eq!(classify(p(), ROOT_CHANGED), Event::Lost);
    }
}
