//! The Performance page (#37): the system at the Moment (CPU per core, memory composition,
//! throughput per disk) and its lane on the Timeline dock, the last hour of CPU use with the
//! seconds nothing was recorded left blank.

use crate::format;
use crate::theme::{FS_BODY, GAP, RADIUS_CHIP, palette};
use crate::ui::{caption, empty_state, kpi_strip, panel, section_title, swatch, title_row};
use chrono::{Local, TimeZone as _};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;
use ve_engine::perf::{CoreKind, Cpu, Disk, Memory, Perf, Reading, SPAN, Sample};
use ve_engine::timeline::Moment;

/// What a reading that has no reader on this platform says.
const UNSUPPORTED: &str = "unsupported";
/// What a reading that is unknown this second says: never zero.
const UNKNOWN: &str = "—";

/// Height of the CPU lane on the dock.
const LANE_H: f32 = 28.;

/// Columns the hour is drawn in: three seconds each, about a pixel at the window's width.
const COLUMNS: usize = 1200;

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

/// The Performance page at `moment`: `sample` is the sample then, `None` a gap.
pub fn page(sample: Option<&Sample>, moment: Moment, now: i64, cx: &App) -> AnyElement {
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
            v_flex().pb(px(GAP)).gap(px(GAP)).child(head).child(kpis(s, cx)).child(
                h_flex().items_start().gap(px(GAP)).child(cpu_panel(&s.cpu, cx).flex_1()).child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(GAP))
                        .child(memory_panel(&s.memory, cx))
                        .child(disks_panel(&s.disks, cx))
                        .child(sensors_panel(s, cx)),
                ),
            ),
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
    let swap = match &s.memory {
        Reading::Value(m) if m.swap_total == 0 => ("none".to_owned(), None),
        Reading::Value(m) => (format::size(m.swap_used), Some(format!("of {}", format::size(m.swap_total)))),
        Reading::Unsupported => (UNSUPPORTED.to_owned(), None),
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
                    if m.swap_total == 0 {
                        "no swap".to_owned()
                    } else {
                        format!("{} of {}", format::size(m.swap_used), format::size(m.swap_total))
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
    let p = palette(cx);
    let from = now - SPAN + 1;
    // The busiest second of each column; the engine leaves a column with no sample empty.
    let peaks = perf.peaks(from, now, COLUMNS, |s| s.cpu.value().and_then(|c| c.busy.value().copied()));
    let recorded = perf.count(from, now);
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
            let column = width / COLUMNS as f32;
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
    v_flex()
        .h(px(48.))
        .flex_none()
        .child(
            div()
                .id("perf-lane")
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
                    format!("{} of the last hour recorded; blank is a gap", recorded_text(recorded)),
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
