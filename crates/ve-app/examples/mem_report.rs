//! Where the memory of a large Index goes: `mem_report <index.bin> [<folder>]`.
//!
//! Loads a saved Index (see examples/gen_index.rs) and builds each cache the engine keeps beside
//! it, printing the heap bytes each one holds, the peak while it is built, and RSS. With
//! `<folder>`, that folder is added as a second Location and Synced, to show what a Sync leaves
//! behind on top of the loaded Index. Heap bytes come from a counting allocator; the columns of
//! the loaded Index are listed as its large allocations, in the order they were made.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ve_core::{BitSet, CategoryMap, Index, Query, QueryContext};
use ve_engine::view::{FileOrder, View};
use ve_scan::SharedIndex;
use ve_scan::duplicates::{self, HashCache, HashProgress};

/// Allocations at least this large are listed one by one.
const LARGE: usize = 256 * 1024;
const SLOTS: usize = 512;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// Live large allocations: (address, bytes, order made); address 0 is a free slot.
static BIG: Mutex<[(usize, usize, usize); SLOTS]> = Mutex::new([(0, 0, 0); SLOTS]);
static BIG_SEQ: AtomicUsize = AtomicUsize::new(0);
static TRACK: AtomicBool = AtomicBool::new(false);

fn grew(by: usize) {
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

/// Records a large allocation moving from `old` (0: new) to `new` (0: freed). A moved one keeps
/// the order it was first made in.
fn track(old: usize, new: usize, size: usize) {
    if !TRACK.load(Ordering::Relaxed) {
        return;
    }
    let Ok(mut slots) = BIG.try_lock() else { return };
    let mut seq = None;
    if let Some(slot) = slots.iter_mut().find(|s| old != 0 && s.0 == old) {
        seq = Some(slot.2);
        *slot = (0, 0, 0);
    }
    if new != 0 && size >= LARGE {
        let seq = seq.unwrap_or_else(|| BIG_SEQ.fetch_add(1, Ordering::Relaxed));
        match slots.iter_mut().find(|s| s.0 == 0) {
            Some(slot) => *slot = (new, size, seq),
            None => panic!("more than {SLOTS} large allocations live; raise SLOTS"),
        }
    }
}

// SAFETY: forwards to System and only counts sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
            if layout.size() >= LARGE {
                track(0, p as usize, layout.size());
            }
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        if layout.size() >= LARGE {
            track(p as usize, 0, 0);
        }
        unsafe { System.dealloc(p, layout) }
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new_size) };
        if !q.is_null() {
            // Counted as if the old block lived until the new one was made, as a move does.
            grew(new_size);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            if layout.size() >= LARGE || new_size >= LARGE {
                track(if layout.size() >= LARGE { p as usize } else { 0 }, q as usize, new_size);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn rss() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<usize>().ok())
        .unwrap_or(0)
        * 1024
}

/// Gives freed pages back to the system, as the engine does after a Sync.
fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: takes no pointers; releases free pages inside the allocator.
        unsafe {
            malloc_trim(0);
        }
    }
}

struct Report {
    entries: usize,
    before: usize,
}

impl Report {
    /// Starts a phase: the peak counts from here.
    fn start(&mut self) {
        self.before = LIVE.load(Ordering::Relaxed);
        PEAK.store(self.before, Ordering::Relaxed);
    }

    /// Ends a phase: what it added to the heap, its peak above the start, and RSS.
    fn row(&mut self, what: &str) {
        let (live, peak) = (LIVE.load(Ordering::Relaxed), PEAK.load(Ordering::Relaxed));
        let held = live as isize - self.before as isize;
        let rss_now = rss();
        trim();
        println!(
            "| {what} | {:+.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
            held as f64 / (1024.0 * 1024.0),
            held as f64 / self.entries.max(1) as f64,
            mib(peak - self.before),
            mib(live),
            mib(rss_now),
            mib(rss()),
        );
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: mem_report <index.bin> [<folder to Sync>]");
    let folder = args.next();
    let categories = CategoryMap::builtin();

    let mut report = Report { entries: 0, before: 0 };
    println!("baseline: heap {:.1} MiB, rss {:.1} MiB", mib(LIVE.load(Ordering::Relaxed)), mib(rss()));
    println!();
    println!("| Phase | Heap held, MiB | Bytes per Entry | Peak above start, MiB | Heap live, MiB | RSS, MiB | RSS after trim, MiB |");
    println!("|---|---|---|---|---|---|---|");

    report.start();
    TRACK.store(true, Ordering::Relaxed);
    let mut index = Index::load(Path::new(&path)).expect("load the Index");
    TRACK.store(false, Ordering::Relaxed);
    report.entries = index.live_count();
    report.row("Index::load");
    let mut columns: Vec<(usize, usize)> = BIG.lock().unwrap().iter().filter(|s| s.0 != 0).map(|s| (s.2, s.1)).collect();
    columns.sort();

    report.start();
    index.idle();
    report.row("Index::idle (drops load slack)");

    let shared = SharedIndex::new(index);
    let mut order = FileOrder::default();
    report.start();
    order.ensure(&shared.read(), &|| false);
    report.row("FileOrder");

    report.start();
    let empty = View::compute(&shared.read(), "", None, &categories, &mut order);
    report.row(&format!("View, empty Query ({} rows)", empty.flat.len()));

    let context = QueryContext::new(categories.clone(), chrono::Local::now());
    let query = Query::parse("ext:mp4", &context).expect("parse");
    report.start();
    let narrow = View::compute(&shared.read(), "ext:mp4", Some(&query), &categories, &mut order);
    report.row(&format!("View, ext:mp4 ({} rows)", narrow.flat.len()));
    let category_bytes: usize = empty
        .categories
        .iter()
        .map(|c| std::mem::size_of_val(c) + c.extensions.iter().map(|(e, _)| 40 + e.capacity()).sum::<usize>())
        .sum();
    report.start();
    drop(narrow);
    report.row("drop the ext:mp4 View");

    let mut view = empty;
    if let Some(folder) = folder {
        report.start();
        let volume = ve_scan::add_volume(&shared, Path::new(&folder), "ext4", "mem-report").expect("add the folder");
        ve_scan::sync(&shared, volume, &ve_scan::Progress::default()).expect("sync the folder");
        report.row("Sync of the folder (before idle)");
        report.start();
        shared.write().idle();
        report.row("Index::idle after the Sync");
        report.start();
        order.ensure(&shared.read(), &|| false);
        report.row("FileOrder of the new generation");
        report.start();
        view = View::compute(&shared.read(), "", None, &categories, &mut order);
        report.row(&format!("View after the Sync, old one dropped ({} rows)", view.flat.len()));
    }

    report.start();
    let mut cache = HashCache::default();
    let excluded = BitSet::new(shared.read().capacity());
    let (groups, _) = duplicates::find(
        &shared,
        &mut cache,
        duplicates::DEFAULT_MIN_SIZE,
        &excluded,
        &HashProgress::default(),
        &AtomicBool::new(false),
    );
    let mut set = BitSet::new(shared.read().capacity());
    for id in groups.iter().flat_map(|g| &g.files) {
        set.insert(id.index());
    }
    drop(excluded);
    report.row(&format!(
        "Duplicates: {} groups of {} files, their set and {} cached hashes",
        groups.len(),
        groups.iter().map(|g| g.files.len()).sum::<usize>(),
        cache.len()
    ));
    drop(view);

    // The loaded Volume is an Offline Volume (a Volume is online only once matched to a mount),
    // so the search above skipped it. Online, each of its files of at least the minimum size is a
    // candidate, held while sizes are compared; the synthetic sizes all differ, so none is read.
    let candidates = {
        let index = shared.read();
        (0..index.capacity() as u32)
            .map(ve_core::EntryId)
            .filter(|&id| index.is_live(id) && !index.is_dir(id) && index.logical(id) >= duplicates::DEFAULT_MIN_SIZE)
            .count()
    };
    {
        let mut index = shared.write();
        let (id, mount) = index.volumes().next().map(|(id, v)| (id, v.mount.clone())).expect("a Location");
        index.set_mount(id, Some(&mount));
    }
    report.start();
    let (more, _) = duplicates::find(
        &shared,
        &mut HashCache::default(),
        duplicates::DEFAULT_MIN_SIZE,
        &BitSet::new(shared.read().capacity()),
        &HashProgress::default(),
        &AtomicBool::new(false),
    );
    let peak = PEAK.load(Ordering::Relaxed) - report.before;
    drop(more);
    report.row(&format!("Duplicates search with the loaded Volume online ({candidates} candidates)"));
    println!("  peak per candidate: {:.0} bytes", peak as f64 / candidates.max(1) as f64);

    println!();
    println!("Category sums of the empty-Query View: {category_bytes} bytes");
    println!();
    println!("Large allocations made by Index::load, in order (columns in field order):");
    let index = shared.read();
    for (seq, bytes) in columns {
        println!(
            "  #{seq:<4} {:>8.1} MiB  {:>5.2} B/Entry  {:>5.2} B/id",
            mib(bytes),
            bytes as f64 / report.entries as f64,
            bytes as f64 / index.capacity() as f64
        );
    }
    println!("capacity {} ids, {} live, {} folder slots", index.capacity(), index.live_count(), index.folder_slots());
}
