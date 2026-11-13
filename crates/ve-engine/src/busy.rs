//! Pauses the Tasks the app started on its own while the machine is busy, reading the load from
//! the Performance samples: about 30 s of heavy CPU or memory load makes the machine busy, about
//! 2 quiet minutes make it quiet again. GPU load is not considered.

use crate::perf::{Sample, Store};
use crate::tasks::Tasks;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

/// Heavy for this long, in seconds of samples, before the machine counts as busy.
pub const BUSY_AFTER: i64 = 30;
/// Quiet for this long before it no longer does.
pub const QUIET_AFTER: i64 = 120;
/// The CPU share the rest of the machine (this app left out) uses that counts as heavy.
const HEAVY_CPU: f32 = 0.85;
/// The share of memory in use (not available to programs) that counts as heavy.
const HEAVY_MEMORY: f32 = 0.90;
/// A longer run of seconds without a sample breaks a streak: nothing is known of them.
const MAX_GAP: i64 = 5;

/// Whether a second's sample shows heavy load. The app's own CPU share is left out, so its own
/// Tasks cannot make the machine look busy; an unreadable share counts as none.
fn heavy(sample: &Sample, own_pid: u32) -> bool {
    let own = sample
        .processes
        .value()
        .and_then(|p| p.by_pid(own_pid))
        .and_then(|p| p.cpu.value().copied())
        .unwrap_or(0.);
    let cpu = sample
        .cpu
        .value()
        .and_then(|c| c.busy.value().copied())
        .is_some_and(|busy| busy - own >= HEAVY_CPU);
    let memory = sample.memory.value().is_some_and(|m| {
        let taken = match m.available.value() {
            Some(&available) => m.total.saturating_sub(available),
            None => m.used,
        };
        m.total > 0 && taken as f32 / m.total as f32 >= HEAVY_MEMORY
    });
    cpu || memory
}

/// Turns a run of seconds into busy or quiet.
#[derive(Default)]
struct Watch {
    busy: bool,
    last: Option<i64>,
    /// Since when the samples have said the opposite of `busy`, without a gap.
    since: Option<i64>,
}

impl Watch {
    fn observe(&mut self, time: i64, heavy: bool) -> bool {
        if self.last.is_some_and(|last| time - last > MAX_GAP) {
            self.since = None;
        }
        self.last = Some(time);
        if heavy == self.busy {
            self.since = None;
            return self.busy;
        }
        let since = *self.since.get_or_insert(time);
        if time - since >= if self.busy { QUIET_AFTER } else { BUSY_AFTER } {
            self.busy = heavy;
            self.since = None;
        }
        self.busy
    }
}

/// Reads each new sample of `store` once a second and tells `tasks` whether the machine is
/// busy. Stops once the Performance recorder is gone.
pub(crate) fn start(store: &Arc<Store>, tasks: Tasks) {
    let store: Weak<Store> = Arc::downgrade(store);
    let own_pid = std::process::id();
    let _ = thread::Builder::new().name("ve-busy".into()).spawn(move || {
        let mut watch = Watch::default();
        let mut seen = i64::MIN;
        loop {
            thread::sleep(Duration::from_secs(1));
            let Some(store) = store.upgrade() else { return };
            let new: Vec<(i64, bool)> = store.timeline(|t| {
                t.range(seen.saturating_add(1), i64::MAX)
                    .map(|(time, s)| (time, heavy(s, own_pid)))
                    .collect()
            });
            drop(store);
            for (time, heavy) in new {
                seen = time;
                tasks.set_machine_busy(watch.observe(time, heavy));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(watch: &mut Watch, from: i64, to: i64, heavy: bool) -> bool {
        (from..=to).map(|t| watch.observe(t, heavy)).last().unwrap()
    }

    #[test]
    fn busy_takes_30_heavy_seconds_and_quiet_two_minutes() {
        let mut w = Watch::default();
        assert!(!run(&mut w, 0, 29, true));
        assert!(w.observe(30, true));
        // A heavy second inside the quiet run starts it again.
        assert!(run(&mut w, 31, 100, false));
        assert!(w.observe(101, true));
        assert!(run(&mut w, 102, 221, false));
        assert!(!w.observe(222, false));
    }

    #[test]
    fn a_gap_breaks_the_streak() {
        let mut w = Watch::default();
        assert!(!run(&mut w, 0, 20, true));
        // Nothing recorded for a minute: the heavy run starts over.
        assert!(!run(&mut w, 80, 105, true));
        assert!(w.observe(110, true));
    }
}
