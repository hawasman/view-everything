//! The Network page (#38): each interface's receive and send throughput at the Moment, and
//! what it moved over the last hour, from the samples Performance records; an interface that
//! went away keeps its row. Per-app traffic needs administrator rights and is not read: the page
//! says so in one static line and never asks.

use crate::format;
use crate::performance::{Lane, Scrub, lane_of, when};
use crate::theme::{FS_BODY, GAP, palette};
use crate::ui::{caption, empty_state, kpi_strip, panel, section_title, title_row};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;
use ve_engine::net::{Interface, InterfaceSummary, Net, NetSample, Summary};
use ve_engine::perf::{Reading, SPAN};
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
/// interface moved over the last hour.
pub fn page(sample: Option<&NetSample>, hour: &Summary, moment: Moment, now: i64, cx: &App) -> AnyElement {
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
                .child(interfaces_panel(&s.interfaces, hour, moment, now, cx)),
        )
        .into_any_element()
}

fn kpis(interfaces: &Reading<Vec<Interface>>, hour: &Summary, cx: &App) -> Div {
    let (receiving, sending) = match interfaces {
        Reading::Value(i) => (rate(&total(i, Interface::receive_rate)), rate(&total(i, Interface::send_rate))),
        Reading::Unsupported => (UNSUPPORTED.to_owned(), UNSUPPORTED.to_owned()),
    };
    let received: u64 = hour.interfaces.iter().map(|i| i.received).sum();
    let sent: u64 = hour.interfaces.iter().map(|i| i.sent).sum();
    let busiest = hour.busiest().map(|i| format!("most on {}", i.name));
    kpi_strip(
        vec![
            ("Receiving".into(), receiving.into(), None),
            ("Sending".into(), sending.into(), None),
            ("Received, last hour".into(), format::size(received).into(), busiest.map(Into::into)),
            ("Sent, last hour".into(), format::size(sent).into(), None),
        ],
        cx,
    )
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
                let totals = moved.map_or((UNKNOWN.to_owned(), UNKNOWN.to_owned()), |m| {
                    (format::size(m.received), format::size(m.sent))
                });
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
                    .child(cell(totals.0, moved.is_none()))
                    .child(cell(totals.1, moved.is_none()))
            };
            // Busiest over the hour first; the kernel's order after that.
            let moved = |name: &str| of(name).map_or(0, |s| s.received + s.sent);
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
