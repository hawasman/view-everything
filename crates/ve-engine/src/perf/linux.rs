//! The Linux backend: `/proc/stat`, `/proc/meminfo`, `/proc/diskstats`, `/proc/net/dev`,
//! `/proc/<pid>` and `/sys`, all read under a root path so tests can point it at a fixture tree.

use super::{
    Backend, CoreKind, Counters, CpuCounters, CpuTimes, Details, DiskCounters, Io, Memory, ProcessCounters, ProcessCountersTable,
    ProcessId, Reading, Swap,
};
use std::collections::HashSet;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Process names kept for sharing before the set starts over (names in kept samples stay).
const MAX_NAMES: usize = 4096;

pub struct LinuxBackend {
    root: PathBuf,
    /// Core kinds are read once: they do not change while the machine runs.
    core_kinds: Arc<[(u32, CoreKind)]>,
    /// Reused for every file read, so a sample allocates no text.
    buf: String,
    /// Reused for each process's file path.
    relative: String,
    path: PathBuf,
    /// Every disk and network interface name seen, shared by all samples.
    names: Vec<Arc<str>>,
    /// Process names, shared by every table that has them.
    process_names: HashSet<Arc<str>>,
    /// `btime` of `/proc/stat`, read once: `None` until then, `Some(None)` if it is missing.
    boot_time: Option<Option<i64>>,
    ticks_per_second: u64,
    page_size: u64,
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
            relative: String::new(),
            path: PathBuf::new(),
            names: Vec::new(),
            process_names: HashSet::new(),
            boot_time: None,
            ticks_per_second: sysconf_or(SysConf::TicksPerSecond, 100),
            page_size: sysconf_or(SysConf::PageSize, 4096),
        }
    }

    /// Reads `root/relative` into the shared buffer; false if it cannot be read.
    fn load(&mut self, relative: &str) -> bool {
        self.buf.clear();
        fs::File::open(self.root.join(relative))
            .and_then(|mut f| f.read_to_string(&mut self.buf))
            .is_ok()
    }

    /// Reads `root/proc/<pid>/<file>` into the shared buffer, building the path in reused
    /// buffers: hundreds of processes are read every two seconds. False if it cannot be read.
    fn load_process(&mut self, pid: u32, file: &str) -> bool {
        use std::fmt::Write as _;
        self.relative.clear();
        let _ = write!(self.relative, "proc/{pid}/{file}");
        self.path.as_mut_os_string().clear();
        self.path.push(&self.root);
        self.path.push(&self.relative);
        self.buf.clear();
        fs::File::open(&self.path).and_then(|mut f| f.read_to_string(&mut self.buf)).is_ok()
    }

    /// One process from `/proc/<pid>`, or `None` if it exited while being read.
    fn process(&mut self, pid: u32, io: bool) -> Option<ProcessCounters> {
        if !self.load_process(pid, "stat") {
            return None;
        }
        let stat = parse_stat(&self.buf)?;
        let name = intern(&mut self.process_names, stat.name);
        let (ppid, cpu_ticks, start) = (stat.ppid, stat.cpu_ticks, stat.start);
        let rss = stat.rss_pages.map(|pages| pages * self.page_size);
        let io = if io && self.load_process(pid, "io") {
            parse_io(&self.buf)
        } else {
            None
        };
        Some(ProcessCounters {
            id: ProcessId { pid, start },
            ppid,
            name,
            cpu_ticks,
            rss,
            io,
        })
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
        let network = if self.load("proc/net/dev") {
            Some(crate::net::interface_counters(&self.buf, &mut self.names))
        } else {
            None
        };
        Counters {
            cpu: cpu.into(),
            core_kinds: self.core_kinds.clone(),
            memory: memory.into(),
            disks: disks.into(),
            network: network.into(),
        }
    }

    fn processes(&mut self, io: bool) -> Reading<ProcessCountersTable> {
        let Ok(entries) = fs::read_dir(self.root.join("proc")) else {
            return Reading::Unsupported;
        };
        let pids: Vec<u32> = entries.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect();
        if self.boot_time.is_none() {
            let btime = if self.load("proc/stat") { boot_time(&self.buf) } else { None };
            self.boot_time = Some(btime);
        }
        let processes = pids.into_iter().filter_map(|pid| self.process(pid, io)).collect();
        Reading::Value(ProcessCountersTable {
            processes,
            boot_time: self.boot_time.flatten(),
            ticks_per_second: self.ticks_per_second,
        })
    }

    fn details(&mut self, id: ProcessId) -> Option<Details> {
        if !self.load_process(id.pid, "stat") || parse_stat(&self.buf)?.start != id.start {
            return None;
        }
        let command = if self.load_process(id.pid, "cmdline") {
            let args: Vec<&str> = self.buf.split('\0').filter(|a| !a.is_empty()).collect();
            (!args.is_empty()).then(|| args.join(" "))
        } else {
            None
        };
        let user = owner(&self.root.join(format!("proc/{}", id.pid))).map(|uid| {
            let name = if self.load("etc/passwd") { user_name(&self.buf, uid) } else { None };
            name.unwrap_or_else(|| format!("uid {uid}"))
        });
        Some(Details {
            command: command.into(),
            user: user.into(),
        })
    }
}

/// The shared copy of process name `name` in `names`.
fn intern(names: &mut HashSet<Arc<str>>, name: &str) -> Arc<str> {
    if let Some(n) = names.get(name) {
        return n.clone();
    }
    if names.len() >= MAX_NAMES {
        names.clear();
    }
    let n: Arc<str> = name.into();
    names.insert(n.clone());
    n
}

/// The fields of `/proc/<pid>/stat` a table row needs.
struct Stat<'a> {
    name: &'a str,
    ppid: u32,
    cpu_ticks: u64,
    start: u64,
    rss_pages: Option<u64>,
}

/// `pid (name) state ppid ...`: the name may hold spaces and parentheses, so it runs to the
/// last `)`. User and system time are fields 14 and 15, the start time field 22, resident
/// pages field 24.
fn parse_stat(text: &str) -> Option<Stat<'_>> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?;
    // Field 3 (state) is the first after the name.
    let mut fields = text.get(close + 1..)?.split_ascii_whitespace();
    let mut at = 2;
    let mut field = |n: usize| -> Option<u64> {
        let value = fields.nth(n - at - 1)?.parse().ok();
        at = n;
        value
    };
    let ppid = u32::try_from(field(4)?).ok()?;
    let cpu_ticks = field(14)? + field(15)?;
    let start = field(22)?;
    Some(Stat {
        name,
        ppid,
        cpu_ticks,
        start,
        rss_pages: field(24),
    })
}

/// Storage bytes read and written, from `/proc/<pid>/io`.
fn parse_io(text: &str) -> Option<Io> {
    let mut read = None;
    let mut write = None;
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else { continue };
        let slot = match name {
            "read_bytes" => &mut read,
            "write_bytes" => &mut write,
            _ => continue,
        };
        *slot = value.trim().parse().ok();
    }
    Some(Io {
        read: read?,
        write: write?,
    })
}

/// The `btime` line of `/proc/stat`: when the machine started, in Unix seconds.
fn boot_time(text: &str) -> Option<i64> {
    text.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
}

/// The user owning `/proc/<pid>`, which is the user the process runs as.
fn owner(dir: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        fs::metadata(dir).ok().map(|m| m.uid())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// The name `/etc/passwd` gives user `uid`.
fn user_name(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        (fields.nth(1)?.parse::<u32>().ok()? == uid).then(|| name.to_owned())
    })
}

enum SysConf {
    TicksPerSecond,
    PageSize,
}

/// A `sysconf` value, or `fallback` where there is none.
fn sysconf_or(what: SysConf, fallback: u64) -> u64 {
    #[cfg(unix)]
    {
        let name = match what {
            SysConf::TicksPerSecond => libc::_SC_CLK_TCK,
            SysConf::PageSize => libc::_SC_PAGESIZE,
        };
        // SAFETY: sysconf only reads a system constant.
        let value = unsafe { libc::sysconf(name) };
        u64::try_from(value).ok().filter(|&v| v > 0).unwrap_or(fallback)
    }
    #[cfg(not(unix))]
    {
        let _ = what;
        fallback
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
    cores.sort_unstable_by_key(|&(id, _)| id);
    Some(CpuCounters { total: total?, cores })
}

/// `/proc/meminfo`, in bytes, read in one pass. Cache is what the kernel can give back
/// (buffers, page cache, reclaimable slab) less shared memory, which it cannot; used is what
/// is left. Without the fields used and cache come from, there is no composition to show;
/// swap is unknown without both of its fields.
fn memory(text: &str) -> Option<Memory> {
    const NAMES: [&str; 9] = [
        "MemTotal",
        "MemFree",
        "MemAvailable",
        "Buffers",
        "Cached",
        "SReclaimable",
        "Shmem",
        "SwapTotal",
        "SwapFree",
    ];
    let mut fields = [None::<u64>; NAMES.len()];
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let Some(i) = NAMES.iter().position(|&n| n == name) else { continue };
        fields[i] = rest
            .split_ascii_whitespace()
            .next()
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb * 1024);
    }
    let [total, free, available, buffers, cached, reclaimable, shmem, swap_total, swap_free] = fields;
    let (total, free, buffers, cached) = (total?, free?, buffers?, cached?);
    // Older kernels lack these two: they have no reclaimable slab or shared memory to count.
    let cache = (buffers + cached + reclaimable.unwrap_or(0)).saturating_sub(shmem.unwrap_or(0));
    let swap = match (swap_total, swap_free) {
        (Some(total), Some(free)) => Reading::Value(Swap {
            total,
            used: total.saturating_sub(free),
        }),
        _ => Reading::Unsupported,
    };
    Some(Memory {
        total,
        used: total.saturating_sub(free).saturating_sub(cache),
        cache,
        free,
        available: available.into(),
        swap,
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
