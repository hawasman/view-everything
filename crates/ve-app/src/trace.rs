//! Frame tracing: with `VE_FRAME_TRACE=1` in the environment, every `render` that takes longer
//! than the threshold (ms, default 8) prints its name and duration to stderr, so a slow frame
//! can be pinned to the view that built it without a profiler.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

static THRESHOLD: LazyLock<Option<Duration>> = LazyLock::new(|| {
    let value = std::env::var("VE_FRAME_TRACE").ok()?;
    Some(Duration::from_millis(value.parse().unwrap_or(8)))
});

/// Logs how long the enclosing scope took, if tracing is on and it was slow.
pub struct Span {
    name: &'static str,
    started: Option<Instant>,
}

pub fn span(name: &'static str) -> Span {
    Span {
        name,
        started: THRESHOLD.map(|_| Instant::now()),
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let (Some(started), Some(limit)) = (self.started, *THRESHOLD) {
            let took = started.elapsed();
            if took >= limit {
                eprintln!("frame-trace: {} took {:.1} ms", self.name, took.as_secs_f64() * 1e3);
            }
        }
    }
}
