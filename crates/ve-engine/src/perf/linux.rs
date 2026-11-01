//! The Linux backend: `/proc/stat`, `/proc/meminfo`, `/proc/diskstats` and `/sys`, all read
//! under a root path so tests can point it at a fixture tree.

use super::{Backend, CoreKind, Counters, CpuCounters, CpuTimes, DiskCounters, Memory};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct LinuxBackend {
    root: PathBuf,
    /// Core kinds are read once: they do not change while the machine runs.
    core_kinds: Arc<[(u32, CoreKind)]>,
    /// Reused for every file read, so a sample allocates no text.
    buf: String,
    /// Every disk name seen, shared by all samples.
    names: Vec<Arc<str>>,
}

impl LinuxBackend {
    /// A backend reading `root/proc` and `root/sys` (`/` on a live system).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let core_kinds = core_kinds(&root).into();
        Self {
            root,
            core_kinds,
            buf: String::new(),
            names: Vec::new(),
        }
    }

    /// Reads `root/relative` into the shared buffer; false if it cannot be read.
    fn load(&mut self, relative: &str) -> bool {
        self.buf.clear();
        fs::File::open(self.root.join(relative))
            .and_then(|mut f| f.read_to_string(&mut self.buf))
            .is_ok()
    }
}

impl Backend for LinuxBackend {
    fn read(&mut self) -> Counters {
        let cpu = if self.load("proc/stat") { cpu_counters(&self.buf) } else { None };
        let memory = if self.load("proc/meminfo") { memory(&self.buf) } else { None };
        let disks = if self.load("proc/diskstats") {
            let whole = whole_disks(&self.root);
            Some(disk_counters(&self.buf, whole.as_deref(), &mut self.names))
        } else {
            None
        };
        Counters {
            cpu: cpu.into(),
            core_kinds: self.core_kinds.clone(),
            memory: memory.into(),
            disks: disks.into(),
        }
    }
}

/// The `cpu` and `cpuN` lines of `/proc/stat`. Busy time is everything but idle and iowait;
/// guest time is already inside user time.
fn cpu_counters(text: &str) -> Option<CpuCounters> {
    let mut total = None;
    let mut cores = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(label) = fields.next().and_then(|l| l.strip_prefix("cpu")) else {
            continue;
        };
        // user nice system idle iowait irq softirq steal; guest is counted in user already.
        let mut values = [0u64; 8];
        let mut read = 0;
        for (slot, field) in values.iter_mut().zip(fields) {
            let Ok(v) = field.parse() else { break };
            *slot = v;
            read += 1;
        }
        if read < 4 {
            continue;
        }
        let all: u64 = values.iter().sum();
        let times = CpuTimes {
            busy: all - values[3] - values[4],
            total: all,
        };
        if label.is_empty() {
            total = Some(times);
        } else if let Ok(id) = label.parse() {
            cores.push((id, times));
        }
    }
    Some(CpuCounters { total: total?, cores })
}

/// `/proc/meminfo`, in bytes. Cache is what the kernel can give back (buffers, page cache,
/// reclaimable slab) less shared memory, which it cannot; used is what is left.
fn memory(text: &str) -> Option<Memory> {
    let field = |name: &str| -> Option<u64> {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let kb: u64 = rest.split_ascii_whitespace().next()?.parse().ok()?;
            Some(kb * 1024)
        })
    };
    let total = field("MemTotal")?;
    let free = field("MemFree")?;
    let cache = (field("Buffers").unwrap_or(0) + field("Cached").unwrap_or(0) + field("SReclaimable").unwrap_or(0))
        .saturating_sub(field("Shmem").unwrap_or(0));
    let swap_total = field("SwapTotal").unwrap_or(0);
    Some(Memory {
        total,
        used: total.saturating_sub(free).saturating_sub(cache),
        cache,
        free,
        available: field("MemAvailable").into(),
        swap_total,
        swap_used: swap_total.saturating_sub(field("SwapFree").unwrap_or(0)),
    })
}

/// The block devices `/sys/block` lists (whole disks, not partitions), or `None` if it
/// cannot be read.
fn whole_disks(root: &Path) -> Option<Vec<String>> {
    let entries = fs::read_dir(root.join("sys/block")).ok()?;
    Some(entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
}

/// The sectors read and written per disk in `/proc/diskstats`: the whole disks `whole` lists
/// (every device when it is unknown), less loop and RAM devices, which are not disks. Names
/// come from `names`, so every sample shares one copy of each.
fn disk_counters(text: &str, whole: Option<&[String]>, names: &mut Vec<Arc<str>>) -> Vec<DiskCounters> {
    text.lines()
        .filter_map(|line| {
            // major minor name reads merged sectors_read ms writes merged sectors_written ...
            let mut fields = line.split_ascii_whitespace();
            let name = fields.nth(2)?;
            if name.starts_with("loop") || name.starts_with("ram") {
                return None;
            }
            if whole.is_some_and(|w| !w.iter().any(|d| d == name)) {
                return None;
            }
            let sectors_read = fields.nth(2)?.parse().ok()?;
            let sectors_written = fields.nth(3)?.parse().ok()?;
            let name = match names.iter().find(|n| &***n == name) {
                Some(n) => n.clone(),
                None => {
                    let n: Arc<str> = name.into();
                    names.push(n.clone());
                    n
                }
            };
            Some(DiskCounters {
                name,
                sectors_read,
                sectors_written,
            })
        })
        .collect()
}

/// The kind of each logical CPU. Hybrid Intel parts list their performance and efficiency
/// CPUs under `/sys/devices/cpu_core` and `cpu_atom` (a part with no efficiency cores may
/// list none under `cpu_atom`: then nothing is told apart); other hybrid parts (Arm
/// big.LITTLE) give the big cores a higher `cpu_capacity`. Anything else is told apart by
/// nothing.
fn core_kinds(root: &Path) -> Vec<(u32, CoreKind)> {
    let list = |path: &str| fs::read_to_string(root.join(path)).ok().map(|t| cpu_list(&t));
    if let (Some(core), Some(atom)) = (list("sys/devices/cpu_core/cpus"), list("sys/devices/cpu_atom/cpus"))
        && !core.is_empty()
        && !atom.is_empty()
    {
        let mut kinds: Vec<_> = core.into_iter().map(|id| (id, CoreKind::Performance)).collect();
        kinds.extend(atom.into_iter().map(|id| (id, CoreKind::Efficiency)));
        kinds.sort_by_key(|&(id, _)| id);
        return kinds;
    }
    let Ok(entries) = fs::read_dir(root.join("sys/devices/system/cpu")) else {
        return Vec::new();
    };
    let mut capacities: Vec<(u32, u64)> = entries
        .flatten()
        .filter_map(|e| {
            let id = e.file_name().to_str()?.strip_prefix("cpu")?.parse().ok()?;
            let capacity = fs::read_to_string(e.path().join("cpu_capacity")).ok()?.trim().parse().ok()?;
            Some((id, capacity))
        })
        .collect();
    capacities.sort_unstable();
    let max = capacities.iter().map(|&(_, c)| c).max();
    let min = capacities.iter().map(|&(_, c)| c).min();
    if max == min {
        return Vec::new();
    }
    capacities
        .into_iter()
        .map(|(id, c)| {
            let kind = if Some(c) == max {
                CoreKind::Performance
            } else {
                CoreKind::Efficiency
            };
            (id, kind)
        })
        .collect()
}

/// A kernel CPU list such as `0-7,16-19`.
fn cpu_list(text: &str) -> Vec<u32> {
    text.trim()
        .split(',')
        .filter_map(|part| match part.split_once('-') {
            Some((a, b)) => Some(a.parse().ok()?..=b.parse().ok()?),
            None => part.parse().ok().map(|id| id..=id),
        })
        .flatten()
        .collect()
}
