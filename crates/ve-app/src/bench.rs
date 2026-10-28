//! `VE_BENCH=1`: scrolls the flat list through a 10M-Entry Index and prints frame timings,
//! then quits. Measurement aid; see examples/gen_index.rs.

use crate::explorer::FlatList;
use gpui_kit::component::table::TableState;
use gpui_kit::*;
use std::time::{Duration, Instant};

const FRAMES_PER_STOP: usize = 300;
const ROWS_PER_FRAME: usize = 2;
/// Depths into the list at which scrolling is timed.
const STOPS: [usize; 3] = [0, 5_000_000, 9_990_000];

pub struct Bench {
    table: Entity<TableState<FlatList>>,
    started: Instant,
    ready: Option<Instant>,
    last: Option<Instant>,
    frames: Vec<Vec<Duration>>,
    n: usize,
}

pub fn start(table: Entity<TableState<FlatList>>, window: &mut Window) {
    let bench = Bench {
        table,
        started: Instant::now(),
        ready: None,
        last: None,
        frames: vec![Vec::new(); STOPS.len()],
        n: 0,
    };
    window.on_next_frame(move |window, cx| step(bench, window, cx));
}

fn rss_mb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .unwrap_or(0)
        / 1024
}

fn step(mut b: Bench, window: &mut Window, cx: &mut App) {
    let now = Instant::now();
    if b.ready.is_none() {
        let rows = b.table.read(cx).delegate().view.as_ref().map(|v| v.flat.len());
        if rows.is_some_and(|r| r > 0) {
            b.ready = Some(now);
            println!(
                "first frame with rows: {} ms after start, view computed in {} ms, rss {} MB",
                b.started.elapsed().as_millis(),
                b.table.read(cx).delegate().view.as_ref().unwrap().took.as_millis(),
                rss_mb()
            );
        } else {
            window.refresh();
            window.on_next_frame(move |w, cx| step(b, w, cx));
            return;
        }
    }
    if let Some(last) = b.last {
        let stop = (b.n - 1) / FRAMES_PER_STOP;
        b.frames[stop].push(now - last);
    }
    b.last = Some(now);
    let stop = b.n / FRAMES_PER_STOP;
    if stop >= STOPS.len() {
        for (depth, frames) in STOPS.iter().zip(&mut b.frames) {
            frames.sort();
            let avg = frames.iter().sum::<Duration>().as_secs_f64() / frames.len() as f64;
            let ms = |q: f64| frames[(frames.len() as f64 * q) as usize].as_secs_f64() * 1e3;
            println!(
                "depth {depth:>9}: {:.1} fps avg, frame ms p50 {:.1} p99 {:.1} max {:.1}",
                1.0 / avg,
                ms(0.5),
                ms(0.99),
                frames.last().unwrap().as_secs_f64() * 1e3
            );
        }
        println!("rss at end {} MB", rss_mb());
        cx.quit();
        return;
    }
    let row = STOPS[stop] + (b.n % FRAMES_PER_STOP) * ROWS_PER_FRAME;
    b.n += 1;
    b.table.update(cx, |t, cx| t.scroll_to_row(row, cx));
    window.on_next_frame(move |w, cx| step(b, w, cx));
}
