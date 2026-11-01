//! `perf.summary(range)` (#59): what the Performance recorder saw over a stretch of seconds,
//! boiled down for the Overview's "what is slowing the machine" answer (and agents). What was
//! not read stays unknown, never zero.

use super::{Perf, ProcessId, Reading};
use std::collections::HashMap;
use std::sync::Arc;

/// What was recorded from one second to another.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// Seconds in the stretch with a sample.
    pub seconds: usize,
    /// Mean busy share of the whole machine's CPU, 0 to 1, over the seconds that read it.
    pub cpu: Option<f32>,
    /// Mean share of memory taken by programs (neither free nor cache), 0 to 1, over the
    /// seconds that read it.
    pub memory: Option<f32>,
    /// The process that took the most CPU over the stretch; `None` when no process table had a
    /// process using any.
    pub top: Option<TopProcess>,
}

/// The busiest process of a stretch.
#[derive(Clone, Debug, PartialEq)]
pub struct TopProcess {
    pub id: ProcessId,
    pub name: Arc<str>,
    /// Its mean share of the machine's CPU over the stretch's process tables, 0 to 1. A table
    /// it was not in (not running yet, or no longer) counts as none, so a short burst ranks
    /// below a steady load.
    pub cpu: f32,
}

impl Perf {
    /// What was recorded from `from` to `to` (inclusive).
    pub fn summary(&self, from: i64, to: i64) -> Summary {
        self.inner.timeline(|timeline| {
            let mut seconds = 0;
            let (mut cpu, mut cpu_seconds) = (0f64, 0usize);
            let (mut memory, mut memory_seconds) = (0f64, 0usize);
            // A process table stands for two seconds: each is counted once, by when it was read.
            let mut last_table = None;
            let mut tables = 0usize;
            let mut shares: HashMap<ProcessId, (f64, Arc<str>)> = HashMap::new();
            for (_, sample) in timeline.range(from, to) {
                seconds += 1;
                if let Some(busy) = sample.cpu.value().and_then(|c| c.busy.value()) {
                    cpu += f64::from(*busy);
                    cpu_seconds += 1;
                }
                if let Some(m) = sample.memory.value().filter(|m| m.total > 0) {
                    memory += m.used as f64 / m.total as f64;
                    memory_seconds += 1;
                }
                if let Reading::Value(table) = &sample.processes
                    && last_table != Some(table.time)
                {
                    last_table = Some(table.time);
                    tables += 1;
                    for p in &table.list {
                        if let Some(&share) = p.cpu.value() {
                            shares.entry(p.id).or_insert_with(|| (0., p.name.clone())).0 += f64::from(share);
                        }
                    }
                }
            }
            let mean = |sum: f64, n: usize| (n > 0).then(|| (sum / n as f64) as f32);
            let top = shares
                .into_iter()
                .filter(|(_, (sum, _))| *sum > 0.)
                // Ties by id, so the pick is stable.
                .max_by(|(a_id, (a, _)), (b_id, (b, _))| a.total_cmp(b).then(b_id.cmp(a_id)))
                .map(|(id, (sum, name))| TopProcess {
                    id,
                    name,
                    cpu: (sum / tables as f64) as f32,
                });
            Summary {
                seconds,
                cpu: mean(cpu, cpu_seconds),
                memory: mean(memory, memory_seconds),
                top,
            }
        })
    }
}
