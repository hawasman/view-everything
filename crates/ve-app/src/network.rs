//! The Network page (#38): each interface's receive and send throughput at the Moment, and
//! what it moved over the last hour, from the samples Performance records; an interface that
//! went away keeps its row. Below, the open TCP and UDP connections (#52) with the process
//! holding each, "unknown" where it cannot be read; a click inspects that process. Per-app
//! traffic needs administrator rights and is not read: the page says so in one static line and
//! never asks.

use crate::format;
use crate::performance::{Lane, OnInspect, Scrub, lane_of, when};
use crate::theme::{FS_BODY, GAP, palette};
use crate::ui::{caption, empty_state, kpi_strip, panel, section_title, title_row};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;
use ve_engine::net::{Connection, Connections, Holder, Interface, InterfaceSummary, Net, NetSample, RETAINED, Summary};
use ve_engine::perf::{ProcessId, Processes, Reading, SPAN};
use ve_engine::timeline::Moment;

/// What a reading that has no reader on this platform says.
const UNSUPPORTED: &str = "unsupported";
/// What a reading that is unknown this second says: never zero.
const UNKNOWN: &str = "—";

/// The page's one line on per-app traffic: a fact, with nothing to click.
pub const PER_APP: &str = "Traffic per app needs administrator rights and is not available yet.";

fn rate(reading: &Reading<u64>) -> String {
    reading
        .value()
        .map_or_else(|| UNKNOWN.to_owned(), |&v| format!("{}/s", format::size(v)))
}

/// The sum of a rate over every interface; unknown if any interface's is.
fn total(interfaces: &[Interface], rate: impl Fn(&Interface) -> Reading<u64>) -> Reading<u64> {
    interfaces.iter().try_fold(0, |sum, i| rate(i).value().map(|v| sum + v)).into()
}

/// The Network page at `moment`: `sample` is the network then, `None` a gap; `hour` what each
/// interface moved over the last hour; `inspect` what a click on a connection opens.
pub fn page(sample: Option<&NetSample>, hour: &Summary, moment: Moment, now: i64, inspect: &Inspect, cx: &App) -> AnyElement {
    let facts = match (moment, sample) {
        (Moment::Live, Some(s)) => format!("Live · sampled at {}", when(s.time, now)),
        (Moment::Live, None) => "Live · no sample yet".to_owned(),
        (Moment::At(t), _) => format!("At {}", when(t, now)),
    };
    let head = v_flex()
        .gap(px(4.))
        .child(title_row("Network", Some(facts), cx))
        .child(caption(PER_APP, cx));
    let Some(s) = sample else {
        let why = match moment {
            Moment::Live => "No sample yet: the first arrives within two seconds of recording.".to_owned(),
            Moment::At(t) => format!("No sample at {}: nothing was recorded that second.", when(t, now)),
        };
        return v_flex().gap(px(GAP)).child(head).child(empty_state(why, cx)).into_any_element();
    };
    div()
        .id("network")
        .size_full()
        .overflow_y_scroll()
        .child(
            v_flex()
                .pb(px(GAP))
                .gap(px(GAP))
                .child(head)
                .child(kpis(&s.interfaces, hour, cx))
                .child(interfaces_panel(&s.interfaces, hour, moment, now, cx))
                .child(connections_panel(&s.connections, moment, inspect, cx)),
        )
        .into_any_element()
}

fn kpis(interfaces: &Reading<Vec<Interface>>, hour: &Summary, cx: &App) -> Div {
    let (receiving, sending) = match interfaces {
        Reading::Value(i) => (rate(&total(i, Interface::receive_rate)), rate(&total(i, Interface::send_rate))),
        Reading::Unsupported => (UNSUPPORTED.to_owned(), UNSUPPORTED.to_owned()),
    };
    let received = hour.interfaces.iter().filter_map(|i| i.received).reduce(|a, b| a + b);
    let sent = hour.interfaces.iter().filter_map(|i| i.sent).reduce(|a, b| a + b);
    let busiest = hour.busiest().map(|i| format!("most on {}", i.name));
    kpi_strip(
        vec![
            ("Receiving".into(), receiving.into(), None),
            ("Sending".into(), sending.into(), None),
            ("Received, last hour".into(), moved(received).into(), busiest.map(Into::into)),
            ("Sent, last hour".into(), moved(sent).into(), None),
        ],
        cx,
    )
}

/// Bytes moved, "—" while unknown.
fn moved(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| UNKNOWN.to_owned(), format::size)
}

/// Width of each number column of the interfaces table.
const NUMBER_W: f32 = 130.;

/// One row per interface at the Moment, then those seen in the last hour but not then.
fn interfaces_panel(interfaces: &Reading<Vec<Interface>>, hour: &Summary, moment: Moment, now: i64, cx: &App) -> Div {
    let p = palette(cx);
    let body = match interfaces {
        Reading::Unsupported => caption(UNSUPPORTED, cx).into_any_element(),
        Reading::Value(there) => {
            let of = |name: &str| hour.interfaces.iter().find(|s| &*s.name == name);
            let cell = |text: String, muted: bool| {
                div()
                    .w(px(NUMBER_W))
                    .flex_none()
                    .text_right()
                    .text_color(if muted { p.muted } else { p.text })
                    .child(text)
            };
            let row = |name: String, note: Option<String>, receive: Reading<u64>, send: Reading<u64>, moved: Option<&InterfaceSummary>| {
                let totals = moved.map_or((None, None), |m| (m.received, m.sent));
                h_flex()
                    .h(px(32.))
                    .items_center()
                    .text_size(px(FS_BODY))
                    .border_t_1()
                    .border_color(p.border)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(8.))
                            .child(div().truncate().child(name))
                            .children(note.map(|n| caption(n, cx))),
                    )
                    .child(cell(rate(&receive), receive.value().is_none()))
                    .child(cell(rate(&send), send.value().is_none()))
                    .child(cell(self::moved(totals.0), totals.0.is_none()))
                    .child(cell(self::moved(totals.1), totals.1.is_none()))
            };
            // Busiest over the hour first; the kernel's order after that.
            let moved = |name: &str| of(name).map_or(0, InterfaceSummary::moved);
            let mut order: Vec<&Interface> = there.iter().collect();
            order.sort_by_key(|i| std::cmp::Reverse(moved(&i.name)));
            let present = order
                .into_iter()
                .map(|i| row(i.name.to_string(), None, i.receive_rate(), i.send_rate(), of(&i.name)));
            let missing = match moment {
                Moment::Live => "gone",
                Moment::At(_) => "not there at this Moment",
            };
            let gone = hour.interfaces.iter().filter(|s| !there.iter().any(|i| i.name == s.name)).map(|s| {
                row(
                    s.name.to_string(),
                    Some(format!("{missing} · last seen {}", when(s.last_seen, now))),
                    Reading::Unsupported,
                    Reading::Unsupported,
                    Some(s),
                )
            });
            let header = h_flex()
                .h(px(24.))
                .text_size(px(FS_BODY))
                .text_color(p.muted)
                .child(div().flex_1().child("Interface"))
                .children(
                    ["Receive", "Send", "Received, hour", "Sent, hour"].map(|t| div().w(px(NUMBER_W)).flex_none().text_right().child(t)),
                );
            if there.is_empty() && hour.interfaces.is_empty() {
                caption("No interfaces besides loopback", cx).into_any_element()
            } else {
                v_flex().child(header).children(present).children(gone).into_any_element()
            }
        }
    };
    panel(cx).child(v_flex().gap(px(GAP)).child(section_title("Interfaces", cx)).child(body))
}

/// Widths of the connections table's fixed columns: protocol, state, process.
const PROTOCOL_W: f32 = 64.;
const STATE_W: f32 = 110.;
const PROCESS_W: f32 = 220.;

/// What a connection's process cell says, and whether that is a process (not muted).
fn holder_text(holder: &Holder) -> (String, bool) {
    match holder {
        Holder::Process { id, name } => (format!("{name} · {}", id.pid), true),
        Holder::Unknown => ("unknown".to_owned(), false),
        Holder::Nobody => ("none, closed".to_owned(), false),
    }
}

/// What a click on a connection opens: its process, when the Moment's process table has it.
pub struct Inspect<'a> {
    /// The process the Inspector shows.
    pub selected: Option<ProcessId>,
    /// The Moment's process table; a past second keeps only some processes.
    pub processes: Option<&'a Processes>,
    pub on_inspect: &'a OnInspect,
}

/// Every open connection at the Moment (all of them live, the kept top of a past second), open
/// conversations first; a click on one held by a known process inspects that process.
fn connections_panel(connections: &Reading<Arc<Connections>>, moment: Moment, inspect: &Inspect, cx: &App) -> Div {
    let p = palette(cx);
    let (body, note) = match connections {
        Reading::Unsupported => (caption(UNSUPPORTED, cx).into_any_element(), None),
        Reading::Value(c) if c.list.is_empty() => (caption("No open connections", cx).into_any_element(), None),
        Reading::Value(c) => {
            let n = c.list.len();
            let note = match moment {
                Moment::At(_) if n >= RETAINED => format!("{n} kept of that second, open conversations first"),
                Moment::At(_) => format!("{n} then"),
                Moment::Live => format!("{n} open"),
            };
            let header = h_flex()
                .h(px(24.))
                .px(px(4.))
                .gap(px(12.))
                .text_size(px(FS_BODY))
                .text_color(p.muted)
                .child(div().w(px(PROTOCOL_W)).flex_none().child("Protocol"))
                .child(div().flex_1().min_w_0().child("Local"))
                .child(div().flex_1().min_w_0().child("Remote"))
                .child(div().w(px(STATE_W)).flex_none().child("State"))
                .child(div().w(px(PROCESS_W)).flex_none().child("Process"));
            let rows = c
                .list
                .iter()
                .enumerate()
                .map(|(i, connection)| connection_row(i, connection, inspect, cx));
            (v_flex().child(header).children(rows).into_any_element(), Some(note))
        }
    };
    panel(cx).child(
        v_flex()
            .gap(px(GAP))
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_baseline()
                    .child(section_title("Connections", cx))
                    .children(note.map(|n| caption(n, cx))),
            )
            .child(body),
    )
}

fn connection_row(i: usize, c: &Connection, inspect: &Inspect, cx: &App) -> Stateful<Div> {
    let p = palette(cx);
    let id = match &c.holder {
        Holder::Process { id, .. } => Some(*id),
        Holder::Unknown | Holder::Nobody => None,
    };
    let inspectable = id.filter(|&id| inspect.processes.is_some_and(|t| t.get(id).is_some()));
    let (process, known) = holder_text(&c.holder);
    let (remote, has_remote) = if c.has_remote() {
        (c.remote.to_string(), true)
    } else {
        ("none".to_owned(), false)
    };
    let cell = |text: String, muted: bool| {
        div()
            .min_w_0()
            .truncate()
            .text_color(if muted { p.muted } else { p.text })
            .child(text)
    };
    h_flex()
        .id(("connection", i))
        .relative()
        .h(px(32.))
        .px(px(4.))
        .gap(px(12.))
        .items_center()
        .text_size(px(FS_BODY))
        .border_t_1()
        .border_color(p.border)
        .when(id.is_some() && id == inspect.selected, |d| {
            d.child(crate::selection::highlight(true, cx))
        })
        .child(cell(c.protocol.label().to_owned(), true).w(px(PROTOCOL_W)).flex_none())
        .child(cell(c.local.to_string(), false).flex_1())
        .child(cell(remote, !has_remote).flex_1())
        .child(cell(c.state.label(c.protocol).to_owned(), false).w(px(STATE_W)).flex_none())
        .child(cell(process, !known).w(px(PROCESS_W)).flex_none())
        .when_some(inspectable, |d, id| {
            let inspect = inspect.on_inspect.clone();
            d.cursor_pointer()
                .hover(|s| s.bg(p.raised))
                .on_click(move |_, _, cx| inspect(id, cx))
        })
}

/// The dock's lane on Network: the busiest second of each column of the last hour, received
/// plus sent, scaled to the hour's peak; blank where no second was recorded.
pub fn lane(net: &Net, moment: Moment, now: i64, scrub: Scrub, cx: &App) -> AnyElement {
    let from = now - SPAN + 1;
    let peaks = net.peaks(from, now, crate::performance::COLUMNS);
    let top = peaks.iter().flatten().copied().max();
    let scale = top.unwrap_or(0).max(1) as f32;
    let lane = Lane {
        id: "net-lane",
        peaks: peaks.into_iter().map(|p| p.map(|v| v as f32 / scale)).collect(),
        recorded: net.count(from, now),
        note: top.map(|t| format!("{}/s", format::size(t))),
    };
    lane_of(lane, moment, now, scrub, cx)
}
