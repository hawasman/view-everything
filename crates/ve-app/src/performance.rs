//! The Performance page (#37): the system at the Moment (CPU per core, memory composition,
//! throughput per disk, the process table) and its lane on the Timeline dock, the last hour of
//! CPU use with the seconds nothing was recorded left blank. A click on a process opens it in
//! the Inspector (#50).

use crate::format;
use crate::inspector::{Panel, long_fact};
use crate::theme::{FS_BODY, FS_SMALL, GAP, RADIUS_CHIP, palette};
use crate::ui::{Tone, caption, empty_state, kpi_strip, panel, section_title, swatch, title_row};
use chrono::{Local, TimeZone as _};
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::cmp::Ordering;
use std::rc::Rc;
use std::sync::Arc;
use ve_engine::perf::{CoreKind, Cpu, Details, Disk, Memory, Perf, Process, ProcessId, Processes, RETAINED, Reading, SPAN, Sample};
use ve_engine::timeline::Moment;

/// What a reading that has no reader on this platform says.
const UNSUPPORTED: &str = "unsupported";
/// What a reading that is unknown this second says: never zero.
const UNKNOWN: &str = "—";

/// Height of a Module's lane on the dock.
const LANE_H: f32 = 28.;

/// Columns the hour is drawn in: three seconds each, about a pixel at the window's width.
pub const COLUMNS: usize = 1200;

/// Moves the Moment to the given second; the dock's lane calls it while scrubbing.
pub type Scrub = Rc<dyn Fn(i64, &mut App)>;

/// A second in local time: the clock time within the last day, the date before that.
pub fn when(time: i64, now: i64) -> String {
    let Some(at) = Local.timestamp_opt(time, 0).earliest() else {
        return UNKNOWN.to_owned();
    };
    if (now - time).abs() < 24 * 3600 {
        at.format("%H:%M:%S").to_string()
    } else {
        at.format("%Y-%m-%d").to_string()
    }
}

/// How the dock names the Moment.
pub fn moment_label(moment: Moment, now: i64) -> String {
    match moment {
        Moment::Live => "Moment: Live".to_owned(),
        Moment::At(time) => format!("At {}", when(time, now)),
    }
}

fn share(reading: &Reading<f32>) -> String {
    reading.value().map_or_else(|| UNKNOWN.to_owned(), |v| format!("{:.0} %", v * 100.))
}

fn rate(reading: &Reading<u64>) -> String {
    reading
        .value()
        .map_or_else(|| UNKNOWN.to_owned(), |&v| format!("{}/s", format::size(v)))
}

/// A data bar: `rule` track, teal fill for `value` (0 to 1); an unknown value leaves it empty.
fn bar(value: Option<f32>, cx: &App) -> Div {
    let p = palette(cx);
    div()
        .flex_1()
        .min_w_0()
        .h(px(6.))
        .rounded(px(3.))
        .bg(p.rule)
        .overflow_hidden()
        .children(value.map(|v| div().h_full().w(relative(v.clamp(0., 1.))).bg(p.accent)))
}

/// A label, value row of a panel.
fn fact_row(label: impl Into<SharedString>, value: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .justify_between()
        .gap(px(GAP))
        .text_size(px(FS_BODY))
        .child(div().text_color(palette(cx).muted).child(label.into()))
        .child(div().child(value.into()))
}

/// The Performance page at `moment`: `sample` is the sample then, `None` a gap; `processes`
/// the table of its processes, which the Shell keeps up to date.
pub fn page(sample: Option<&Sample>, moment: Moment, now: i64, processes: &Entity<TableState<ProcessList>>, cx: &App) -> AnyElement {
    let facts = match (moment, sample) {
        (Moment::Live, Some(s)) => format!("Live · sampled at {}", when(s.time, now)),
        (Moment::Live, None) => "Live · no sample yet".to_owned(),
        (Moment::At(t), _) => format!("At {}", when(t, now)),
    };
    let head = title_row("Performance", Some(facts), cx);
    let Some(s) = sample else {
        let why = match moment {
            Moment::Live => "No sample yet: the first arrives within two seconds of recording.".to_owned(),
            Moment::At(t) => format!("No sample at {}: nothing was recorded that second.", when(t, now)),
        };
        return v_flex().gap(px(GAP)).child(head).child(empty_state(why, cx)).into_any_element();
    };
    div()
        .id("performance")
        .size_full()
        .overflow_y_scroll()
        .child(
            v_flex()
                .pb(px(GAP))
                .gap(px(GAP))
                .child(head)
                .child(kpis(s, cx))
                .child(
                    h_flex().items_start().gap(px(GAP)).child(cpu_panel(&s.cpu, cx).flex_1()).child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(GAP))
                            .child(memory_panel(&s.memory, cx))
                            .child(disks_panel(&s.disks, cx))
                            .child(sensors_panel(s, cx)),
                    ),
                )
                .child(processes_panel(&s.processes, moment, processes, cx)),
        )
        .into_any_element()
}

fn kpis(s: &Sample, cx: &App) -> Div {
    let (cpu, cores) = match &s.cpu {
        Reading::Value(c) => (share(&c.busy), Some(core_summary(c))),
        Reading::Unsupported => (UNSUPPORTED.to_owned(), None),
    };
    let (memory, of) = match &s.memory {
        Reading::Value(m) => (format::size(m.used), Some(format!("of {}", format::size(m.total)))),
        Reading::Unsupported => (UNSUPPORTED.to_owned(), None),
    };
    let swap = match s.memory.value().map(|m| &m.swap) {
        Some(Reading::Value(swap)) if swap.total == 0 => ("none".to_owned(), None),
        Some(Reading::Value(swap)) => (format::size(swap.used), Some(format!("of {}", format::size(swap.total)))),
        Some(Reading::Unsupported) | None => (UNSUPPORTED.to_owned(), None),
    };
    let (read, write) = match &s.disks {
        Reading::Value(d) => (total(d, |d| &d.read), total(d, |d| &d.write)),
        Reading::Unsupported => (Reading::Unsupported, Reading::Unsupported),
    };
    let disk = match &s.disks {
        Reading::Value(_) => (rate(&read), Some(format!("written {}", rate(&write)))),
        Reading::Unsupported => (UNSUPPORTED.to_owned(), None),
    };
    kpi_strip(
        vec![
            ("CPU".into(), cpu.into(), cores.map(Into::into)),
            ("Memory used".into(), memory.into(), of.map(Into::into)),
            ("Swap used".into(), swap.0.into(), swap.1.map(Into::into)),
            ("Disks read".into(), disk.0.into(), disk.1.map(Into::into)),
        ],
        cx,
    )
}

/// The sum of a rate over every disk; unknown if any disk's is.
fn total(disks: &[Disk], rate: impl Fn(&Disk) -> &Reading<u64>) -> Reading<u64> {
    disks.iter().try_fold(0, |sum, d| rate(d).value().map(|v| sum + v)).into()
}

/// "16 logical CPUs · 8 performance, 8 efficiency".
fn core_summary(cpu: &Cpu) -> String {
    let count = |kind| cpu.cores.iter().filter(|c| c.kind == kind).count();
    let (p, e) = (count(CoreKind::Performance), count(CoreKind::Efficiency));
    let n = cpu.cores.len();
    if p + e > 0 {
        format!("{n} logical CPUs · {p} performance, {e} efficiency")
    } else {
        format!("{n} logical CPUs")
    }
}

fn cpu_panel(cpu: &Reading<Cpu>, cx: &App) -> Div {
    let body = match cpu {
        Reading::Unsupported => caption(UNSUPPORTED, cx).into_any_element(),
        Reading::Value(cpu) => {
            let told_apart = cpu.cores.iter().any(|c| c.kind != CoreKind::Unknown);
            let groups: Vec<(Option<&str>, Vec<&ve_engine::perf::Core>)> = if told_apart {
                [
                    ("Performance cores", CoreKind::Performance),
                    ("Efficiency cores", CoreKind::Efficiency),
                    ("Other cores", CoreKind::Unknown),
                ]
                .into_iter()
                .map(|(label, kind)| (Some(label), cpu.cores.iter().filter(|c| c.kind == kind).collect::<Vec<_>>()))
                .filter(|(_, cores)| !cores.is_empty())
                .collect()
            } else {
                vec![(None, cpu.cores.iter().collect())]
            };
            v_flex()
                .gap(px(GAP))
                .children(groups.into_iter().map(|(label, cores)| {
                    v_flex()
                        .gap(px(4.))
                        .when_some(label, |d, l| d.child(caption(l, cx)))
                        .child(h_flex().flex_wrap().children(cores.into_iter().map(|core| {
                            let prefix = match core.kind {
                                CoreKind::Performance => "P",
                                CoreKind::Efficiency => "E",
                                CoreKind::Unknown => "CPU ",
                            };
                            h_flex()
                                .w(relative(0.5))
                                .pr(px(GAP))
                                .h(px(24.))
                                .gap(px(8.))
                                .items_center()
                                .text_size(px(FS_BODY))
                                .child(
                                    div()
                                        .w(px(56.))
                                        .flex_none()
                                        .text_color(palette(cx).muted)
                                        .child(format!("{prefix}{}", core.id)),
                                )
                                .child(bar(core.busy.value().copied(), cx))
                                .child(div().w(px(44.)).flex_none().text_right().child(share(&core.busy)))
                        })))
                }))
                .into_any_element()
        }
    };
    panel(cx).child(v_flex().gap(px(GAP)).child(section_title("CPU", cx)).child(body))
}

fn memory_panel(memory: &Reading<Memory>, cx: &App) -> Div {
    let p = palette(cx);
    let body = match memory {
        Reading::Unsupported => caption(UNSUPPORTED, cx).into_any_element(),
        Reading::Value(m) => {
            let part = |bytes: u64| if m.total == 0 { 0. } else { bytes as f32 / m.total as f32 };
            let cache_tint = p.accent.opacity(0.4);
            v_flex()
                .gap(px(8.))
                .child(
                    h_flex()
                        .h(px(10.))
                        .rounded(px(RADIUS_CHIP))
                        .bg(p.rule)
                        .overflow_hidden()
                        .child(div().h_full().w(relative(part(m.used))).bg(p.accent))
                        .child(div().h_full().w(relative(part(m.cache))).bg(cache_tint)),
                )
                .child(legend_row(p.accent, "Used", m.used, cx))
                .child(legend_row(cache_tint, "Cache and buffers", m.cache, cx))
                .child(legend_row(p.rule, "Free", m.free, cx))
                .child(fact_row(
                    "Available",
                    m.available.value().map_or_else(|| UNKNOWN.to_owned(), |&a| format::size(a)),
                    cx,
                ))
                .child(fact_row(
                    "Swap used",
                    match &m.swap {
                        Reading::Value(swap) if swap.total == 0 => "no swap".to_owned(),
                        Reading::Value(swap) => format!("{} of {}", format::size(swap.used), format::size(swap.total)),
                        Reading::Unsupported => UNSUPPORTED.to_owned(),
                    },
                    cx,
                ))
                .into_any_element()
        }
    };
    panel(cx).child(v_flex().gap(px(GAP)).child(section_title("Memory", cx)).child(body))
}

/// A part of the memory bar: its swatch, name and size.
fn legend_row(color: Hsla, label: &'static str, bytes: u64, cx: &App) -> Div {
    h_flex()
        .gap(px(8.))
        .items_center()
        .child(swatch(color))
        .child(fact_row(label, format::size(bytes), cx).flex_1())
}

fn disks_panel(disks: &Reading<Vec<Disk>>, cx: &App) -> Div {
    let p = palette(cx);
    let body = match disks {
        Reading::Unsupported => caption(UNSUPPORTED, cx).into_any_element(),
        Reading::Value(d) if d.is_empty() => caption("No disks", cx).into_any_element(),
        Reading::Value(d) => v_flex()
            .child(
                h_flex()
                    .h(px(24.))
                    .text_size(px(FS_BODY))
                    .text_color(p.muted)
                    .child(div().flex_1().child("Device"))
                    .child(div().w(px(110.)).text_right().child("Read"))
                    .child(div().w(px(110.)).text_right().child("Write")),
            )
            .children(d.iter().map(|disk| {
                h_flex()
                    .h(px(28.))
                    .items_center()
                    .text_size(px(FS_BODY))
                    .border_t_1()
                    .border_color(p.border)
                    .child(div().flex_1().min_w_0().truncate().child(disk.name.to_string()))
                    .child(div().w(px(110.)).text_right().child(rate(&disk.read)))
                    .child(div().w(px(110.)).text_right().child(rate(&disk.write)))
            }))
            .into_any_element(),
    };
    panel(cx).child(v_flex().gap(px(GAP)).child(section_title("Disks", cx)).child(body))
}

/// GPU, temperatures and power: no reader yet, so each says so.
fn sensors_panel(s: &Sample, cx: &App) -> Div {
    let show = |r: &Reading<f32>, unit: &str| r.value().map_or_else(|| UNSUPPORTED.to_owned(), |v| format!("{v:.0} {unit}"));
    panel(cx).child(
        v_flex()
            .gap(px(8.))
            .child(section_title("Sensors", cx))
            .child(fact_row("GPU", show(&s.gpu, "%"), cx))
            .child(fact_row("Temperature", show(&s.temperature, "°C"), cx))
            .child(fact_row("Power", show(&s.power, "W"), cx)),
    )
}

/// The dock's lane on Performance: CPU use over the last hour, one column per pixel, blank
/// where no second was recorded. Pressing or dragging on it moves the Moment there.
pub fn lane(perf: &Perf, moment: Moment, now: i64, scrub: Scrub, cx: &App) -> AnyElement {
    let from = now - SPAN + 1;
    let lane = Lane {
        id: "perf-lane",
        // The busiest second of each column; the engine leaves a column with no sample empty.
        peaks: perf.peaks(from, now, COLUMNS, |s| s.cpu.value().and_then(|c| c.busy.value().copied())),
        recorded: perf.count(from, now),
        note: None,
    };
    lane_of(lane, moment, now, scrub, cx)
}

/// What a Module's lane draws over the last hour.
pub struct Lane {
    pub id: &'static str,
    /// One bar per column, 0 to 1; `None` is a column with no sample, left blank.
    pub peaks: Vec<Option<f32>>,
    /// Seconds of the hour with a sample.
    pub recorded: usize,
    /// What the bars' full height stands for, where it is not 100 %.
    pub note: Option<String>,
}

/// Draws `lane` on the dock: a bar per column, the Moment's second marked; pressing or
/// dragging on it moves the Moment there.
pub fn lane_of(lane: Lane, moment: Moment, now: i64, scrub: Scrub, cx: &App) -> AnyElement {
    let p = palette(cx);
    let from = now - SPAN + 1;
    let Lane { id, peaks, recorded, note } = lane;
    let columns = peaks.len().max(1);
    let bounds: Rc<Cell<Bounds<Pixels>>> = Rc::default();
    let time_at = {
        let bounds = bounds.clone();
        move |x: Pixels| {
            let b = bounds.get();
            let width = f32::from(b.size.width).max(1.);
            let fraction = (f32::from(x - b.origin.x) / width).clamp(0., 1.);
            (from + (fraction * (SPAN - 1) as f32).round() as i64).min(now)
        }
    };
    let cursor = match moment {
        Moment::At(t) if (from..=now).contains(&t) => Some(t),
        _ => None,
    };
    let (accent, base, ink) = (p.accent, p.border, p.text);
    let surface = canvas(
        {
            let bounds = bounds.clone();
            move |b, _, _| bounds.set(b)
        },
        move |b, _, window, _| {
            let width = f32::from(b.size.width);
            let height = f32::from(b.size.height);
            let column = width / columns as f32;
            for (i, v) in peaks.iter().enumerate() {
                // No sample in the column: a gap, left blank.
                let Some(v) = *v else { continue };
                let left = b.origin.x + px(i as f32 * column);
                window.paint_quad(fill(
                    Bounds::new(point(left, b.origin.y + px(height - 1.)), size(px(column), px(1.))),
                    base,
                ));
                let h = (v * (height - 2.)).max(1.);
                window.paint_quad(fill(
                    Bounds::new(point(left, b.origin.y + px(height - 1. - h)), size(px(column), px(h))),
                    accent,
                ));
            }
            if let Some(t) = cursor {
                let x = (((t - from) as f32 / SPAN as f32) * width).min(width - 2.);
                window.paint_quad(fill(
                    Bounds::new(point(b.origin.x + px(x), b.origin.y), size(px(2.), px(height))),
                    ink,
                ));
            }
        },
    )
    .size_full();
    let down = {
        let (scrub, time_at) = (scrub.clone(), time_at.clone());
        move |e: &MouseDownEvent, _: &mut Window, cx: &mut App| scrub(time_at(e.position.x), cx)
    };
    let drag = move |e: &MouseMoveEvent, _: &mut Window, cx: &mut App| {
        if e.pressed_button == Some(MouseButton::Left) && bounds.get().contains(&e.position) {
            scrub(time_at(e.position.x), cx);
        }
    };
    let recorded = format!("{} of the last hour recorded; blank is a gap", recorded_text(recorded));
    v_flex()
        .h(px(48.))
        .flex_none()
        .child(
            div()
                .id(id)
                .relative()
                .h(px(LANE_H))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, down)
                .on_mouse_move(drag)
                .child(surface),
        )
        .child(
            h_flex()
                .justify_between()
                .child(caption("1 h ago", cx))
                .child(caption(
                    match note {
                        Some(note) => format!("{recorded} · top is {note}"),
                        None => recorded,
                    },
                    cx,
                ))
                .child(caption("Now", cx)),
        )
        .into_any_element()
}

/// "42 min", "18 s".
fn recorded_text(seconds: usize) -> String {
    if seconds >= 60 {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

/// Height of the process table: about a dozen rows, the page scrolling around it.
const PROCESSES_H: f32 = 420.;

fn processes_panel(reading: &Reading<Arc<Processes>>, moment: Moment, table: &Entity<TableState<ProcessList>>, cx: &App) -> Div {
    let body = match reading {
        Reading::Unsupported => caption(UNSUPPORTED, cx).into_any_element(),
        Reading::Value(_) => div()
            .h(px(PROCESSES_H))
            .child(
                DataTable::new(table)
                    .small()
                    .bordered(false)
                    .stripe(false)
                    .header_row_height(px(30.)),
            )
            .into_any_element(),
    };
    let note = match (moment, reading) {
        (_, Reading::Unsupported) => None,
        (Moment::Live, _) => Some(format!(
            "{} running · I/O is read while this page is open",
            table.read(cx).delegate().order.len()
        )),
        (Moment::At(_), Reading::Value(p)) => Some(format!("The top {RETAINED} by CPU and memory, as read at {}", when(p.time, p.time))),
    };
    panel(cx).child(
        v_flex()
            .gap(px(GAP))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(section_title("Processes", cx))
                    .children(note.map(|n| caption(n, cx))),
            )
            .child(body),
    )
}

/// A click on a process: inspect it.
pub type OnInspect = Rc<dyn Fn(ProcessId, &mut App)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Name,
    Pid,
    Cpu,
    Memory,
    Read,
    Write,
}

/// (title, width, sort, right-aligned)
const PROCESS_COLUMNS: [(&str, f32, SortBy, bool); 6] = [
    ("Name", 260., SortBy::Name, false),
    ("PID", 80., SortBy::Pid, true),
    ("CPU", 80., SortBy::Cpu, true),
    ("Memory", 100., SortBy::Memory, true),
    ("Read", 100., SortBy::Read, true),
    ("Write", 100., SortBy::Write, true),
];

/// The process table of the Moment, sortable by any column.
pub struct ProcessList {
    table: Option<Arc<Processes>>,
    /// Indices into the table's list, in the order shown.
    order: Vec<usize>,
    sort: SortBy,
    descending: bool,
    /// The process the Inspector shows.
    selected: Option<ProcessId>,
    on_inspect: OnInspect,
}

/// Unknown values sort after known ones whichever the direction.
fn known_first<T: PartialOrd>(a: Option<T>, b: Option<T>, descending: bool) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) if descending => b.partial_cmp(&a).unwrap_or(Ordering::Equal),
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        (a, b) => b.is_some().cmp(&a.is_some()),
    }
}

impl ProcessList {
    /// A list showing nothing until [`ProcessList::show`]; busiest first.
    pub fn new(on_inspect: OnInspect) -> Self {
        Self {
            table: None,
            order: Vec::new(),
            sort: SortBy::Cpu,
            descending: true,
            selected: None,
            on_inspect,
        }
    }

    /// Shows `table` (the Moment's) with `selected` marked; true if anything changed.
    pub fn show(&mut self, table: Option<Arc<Processes>>, selected: Option<ProcessId>) -> bool {
        let same = match (&self.table, &table) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same && self.selected == selected {
            return false;
        }
        self.selected = selected;
        if !same {
            self.table = table;
            self.sorted();
        }
        true
    }

    /// The row process `id` is shown at, if the table has it.
    pub fn row_of(&self, id: ProcessId) -> Option<usize> {
        let list = &self.table.as_ref()?.list;
        self.order.iter().position(|&i| list[i].id == id)
    }

    fn sorted(&mut self) {
        let Some(table) = &self.table else {
            self.order.clear();
            return;
        };
        let list = &table.list;
        let (sort, descending) = (self.sort, self.descending);
        // Compared without allocating: ties fall back to the name for every column.
        fn lower(p: &Process) -> impl Iterator<Item = char> + '_ {
            p.name.chars().flat_map(char::to_lowercase)
        }
        let by_name = |a: &Process, b: &Process| lower(a).cmp(lower(b)).then(a.id.cmp(&b.id));
        let io = |p: &Process, f: fn(&ve_engine::perf::Io) -> u64| p.io.value().map(f);
        self.order = (0..list.len()).collect();
        self.order.sort_by(|&a, &b| {
            let (a, b) = (&list[a], &list[b]);
            match sort {
                SortBy::Name if descending => by_name(b, a),
                SortBy::Name => by_name(a, b),
                SortBy::Pid if descending => b.id.cmp(&a.id),
                SortBy::Pid => a.id.cmp(&b.id),
                SortBy::Cpu => known_first(a.cpu.value(), b.cpu.value(), descending),
                SortBy::Memory => known_first(a.rss.value(), b.rss.value(), descending),
                SortBy::Read => known_first(io(a, |i| i.read), io(b, |i| i.read), descending),
                SortBy::Write => known_first(io(a, |i| i.write), io(b, |i| i.write), descending),
            }
            .then_with(|| by_name(a, b))
        });
    }

    fn process_at(&self, row: usize) -> Option<&Process> {
        Some(&self.table.as_ref()?.list[*self.order.get(row)?])
    }
}

impl TableDelegate for ProcessList {
    fn columns_count(&self, _: &App) -> usize {
        PROCESS_COLUMNS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn column(&self, ix: usize, _: &App) -> Column {
        let (name, width, by, right) = PROCESS_COLUMNS[ix];
        let column = Column::new(name, name).width(width);
        let column = if right { column.text_right() } else { column };
        column.sort(match (by == self.sort, self.descending) {
            (false, _) => ColumnSort::Default,
            (true, true) => ColumnSort::Descending,
            (true, false) => ColumnSort::Ascending,
        })
    }

    fn perform_sort(&mut self, col: usize, sort: ColumnSort, _: &mut Window, _: &mut Context<TableState<Self>>) {
        self.sort = PROCESS_COLUMNS[col].2;
        self.descending = sort != ColumnSort::Ascending;
        self.sorted();
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        div()
            .size_full()
            .text_size(px(FS_SMALL))
            .child(self.column(col_ix, cx).name.clone())
    }

    fn render_last_empty_col(&mut self, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        div().w_0()
    }

    fn render_tr(&mut self, row: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let id = self.process_at(row).map(|p| p.id);
        let selected = id.is_some() && id == self.selected;
        div()
            .id(("process-row", row))
            .when(selected, |d| d.child(crate::selection::highlight(true, cx)))
            .on_click(cx.listener(move |table, _: &ClickEvent, _, cx| {
                let list = table.delegate_mut();
                let Some(id) = list.process_at(row).map(|p| p.id) else {
                    return;
                };
                list.selected = Some(id);
                let inspect = list.on_inspect.clone();
                inspect(id, cx);
                cx.notify();
            }))
    }

    fn render_td(&mut self, row: usize, col: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let Some(p) = self.process_at(row) else {
            return div().into_any_element();
        };
        let palette = palette(cx);
        let (text, known) = match col {
            0 => (p.name.to_string(), true),
            1 => (p.id.pid.to_string(), true),
            2 => (process_share(&p.cpu), p.cpu.value().is_some()),
            3 => (bytes(&p.rss), p.rss.value().is_some()),
            4 => (io_rate(&p.io, |i| i.read), p.io.value().is_some()),
            _ => (io_rate(&p.io, |i| i.write), p.io.value().is_some()),
        };
        h_flex()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(FS_BODY))
            .text_color(if known && col != 1 { palette.text } else { palette.muted })
            .when(PROCESS_COLUMNS[col].3, |d| d.justify_end())
            .child(div().min_w_0().truncate().child(text))
            .into_any_element()
    }
}

/// A process's CPU share, to a tenth of a percent: most are well under one.
fn process_share(reading: &Reading<f32>) -> String {
    reading.value().map_or_else(|| UNKNOWN.to_owned(), |v| format!("{:.1} %", v * 100.))
}

fn bytes(reading: &Reading<u64>) -> String {
    reading.value().map_or_else(|| UNKNOWN.to_owned(), |&b| format::size(b))
}

fn io_rate(reading: &Reading<ve_engine::perf::Io>, part: fn(&ve_engine::perf::Io) -> u64) -> String {
    reading
        .value()
        .map_or_else(|| UNKNOWN.to_owned(), |io| format!("{}/s", format::size(part(io))))
}

/// "2 h 05 min", "3 min 12 s", "4.2 s".
fn duration(seconds: f64) -> String {
    let whole = seconds as u64;
    match whole {
        0..60 => format!("{seconds:.1} s"),
        60..3600 => format!("{} min {:02} s", whole / 60, whole % 60),
        _ => format!("{} h {:02} min", whole / 3600, whole % 3600 / 60),
    }
}

/// The process the Inspector shows: the table it was clicked in, and its command line and user
/// once read (`None` while reading; `Some(None)` once it has exited).
pub struct Inspecting {
    pub id: ProcessId,
    pub seen: Arc<Processes>,
    pub details: Option<Option<Details>>,
}

/// The Inspector's view of a process: its identity and details, and its readings in `table`
/// (the Moment's), "—" where it is not in it.
pub fn inspector_panel(inspecting: &Inspecting, table: Option<&Processes>, now: i64) -> Option<Panel> {
    let id = inspecting.id;
    let current = table.and_then(|t| Some((t, t.get(id)?)));
    let (table, process) = current.or_else(|| Some((&*inspecting.seen, inspecting.seen.get(id)?)))?;
    let fact = |label: &str, value: String| (SharedString::from(label.to_owned()), SharedString::from(value));
    let unknown = || UNKNOWN.to_owned();
    let details = inspecting.details.as_ref().and_then(Option::as_ref);
    let parent = match process.ppid {
        0 => "none".to_owned(),
        ppid => match table.by_pid(ppid) {
            Some(parent) => format!("{} · PID {ppid}", parent.name),
            None => format!("PID {ppid}"),
        },
    };
    let readings = current.map(|(_, p)| p);
    let facts = vec![
        long_fact(
            "Command line",
            details.and_then(|d| d.command.value().cloned()).unwrap_or_else(unknown),
        ),
        fact("User", details.and_then(|d| d.user.value().cloned()).unwrap_or_else(unknown)),
        fact("Started", table.started(process).map_or_else(unknown, |t| when(t, now))),
        fact("Parent", parent),
        fact("CPU time", duration(table.cpu_seconds(process))),
        fact("CPU", readings.map_or_else(unknown, |p| process_share(&p.cpu))),
        fact("Memory", readings.map_or_else(unknown, |p| bytes(&p.rss))),
        fact("Read", readings.map_or_else(unknown, |p| io_rate(&p.io, |i| i.read))),
        fact("Write", readings.map_or_else(unknown, |p| io_rate(&p.io, |i| i.write))),
    ];
    let tag = match inspecting.details {
        Some(None) => Some(("Exited".into(), Tone::Neutral)),
        _ => None,
    };
    Some(Panel {
        kind: "Process",
        tag,
        title: process.name.to_string().into(),
        subtitle: Some(format!("PID {}", id.pid).into()),
        facts,
        actions: Vec::new(),
    })
}
