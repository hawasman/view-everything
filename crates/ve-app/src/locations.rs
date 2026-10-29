//! The sidebar's Locations row and panel: every Location's state, one health dot for all of
//! them, and a Sync now per Location.

use crate::dashboard_summary::ago;
use crate::engine::{Engine, Phase, Shared};
use crate::settings::Settings;
use crate::theme::{RADIUS_CARD, palette};
use crate::ui::{caption, dot, pill};
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;
use ve_scan::VolumeInfo;

/// What is known about one Location, enough to say how it is doing.
#[derive(Clone, Debug)]
pub struct Facts {
    pub online: bool,
    /// Why its last sync or check failed, while that still holds.
    pub failure: Option<String>,
    /// A running Live updates source keeps it current.
    pub followed: bool,
    pub syncing: bool,
    /// Last Synced, in seconds since the Unix epoch.
    pub last_synced: Option<i64>,
    /// How often it is synced, in minutes; 0 is Manual.
    pub interval_minutes: u64,
}

/// A Location's state, as the panel words it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Live,
    Syncing,
    Synced(i64),
    Offline,
    Stale,
    Failed(String),
}

impl State {
    /// Online, not failing and up to date.
    pub fn healthy(&self) -> bool {
        matches!(self, State::Live | State::Syncing | State::Synced(_))
    }

    /// "Live", "Synced 6 min ago", "Failed: reason".
    pub fn text(&self, now: i64) -> String {
        match self {
            State::Live => "Live".to_owned(),
            State::Syncing => "Syncing".to_owned(),
            State::Synced(at) => format!("Synced {}", ago(now, *at)),
            State::Offline => "Offline".to_owned(),
            State::Stale => "Stale".to_owned(),
            State::Failed(why) => format!("Failed: {why}"),
        }
    }
}

/// The state of a Location at `now`. Offline outranks a failure, a failure outranks being
/// current. A Manual interval keeps a Location that has synced once up to date for good.
pub fn state(f: &Facts, now: i64) -> State {
    if !f.online {
        return State::Offline;
    }
    if let Some(why) = &f.failure {
        return State::Failed(why.clone());
    }
    if f.syncing {
        return State::Syncing;
    }
    if f.followed {
        return State::Live;
    }
    match f.last_synced {
        Some(at)
            if f.interval_minutes == 0
                || (at <= now && now - at < i64::try_from(f.interval_minutes.saturating_mul(60)).unwrap_or(i64::MAX)) =>
        {
            State::Synced(at)
        }
        _ => State::Stale,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Green,
    Yellow,
    Red,
}

/// Green when every Location is healthy, yellow when exactly one is not, red when two or more are not.
pub fn health(unhealthy: usize) -> Health {
    match unhealthy {
        0 => Health::Green,
        1 => Health::Yellow,
        _ => Health::Red,
    }
}

pub struct Row {
    pub identity: String,
    pub name: String,
    pub state: State,
    /// A Sync is running, whatever `state` says (a failure outranks it in the wording).
    pub syncing: bool,
    pub online: bool,
}

/// Every Location in the Index, in Index order.
pub fn rows(shared: &Shared, mounted: &[VolumeInfo], settings: &Settings, now: i64) -> Vec<Row> {
    let status = shared.status.lock().clone();
    let syncing: &[std::path::PathBuf] = match &status.phase {
        Phase::Syncing { volumes, .. } => volumes,
        _ => &[],
    };
    let followed = shared.followed.lock().clone();
    let index = shared.index.read();
    index
        .volumes()
        .map(|(id, v)| {
            let info = mounted.iter().find(|m| m.identity == v.identity);
            let name = match (v.remote_display_name(), info) {
                (Some(name), _) => name,
                (None, Some(m)) => crate::app::volume_name(m),
                (None, None) => v.mount.display().to_string(),
            };
            let cause = format!("volume {}", v.mount.display());
            let facts = Facts {
                online: v.online,
                failure: status.problems.iter().find(|(c, _)| *c == cause).map(|(_, m)| m.clone()),
                followed: followed.contains(&id),
                syncing: syncing.contains(&v.mount),
                last_synced: v.last_synced,
                interval_minutes: match (v.remote(), info) {
                    (Some(r), _) => settings.remote_refresh_minutes(&r),
                    (None, Some(m)) => settings.refresh_minutes_for(m),
                    (None, None) => settings.refresh_minutes,
                },
            };
            Row {
                identity: v.identity.clone(),
                name,
                state: state(&facts, now),
                syncing: facts.syncing,
                online: v.online,
            }
        })
        .collect()
}

/// The overall dot colour and the tooltip: "1 Location needs attention: /mnt/usb is Offline".
pub fn summary(rows: &[Row], now: i64) -> (Health, String) {
    let bad: Vec<&Row> = rows.iter().filter(|r| !r.state.healthy()).collect();
    let text = match bad.len() {
        0 if rows.is_empty() => "No Locations are indexed yet".to_owned(),
        0 => "Every Location is up to date".to_owned(),
        n => {
            let list: Vec<String> = bad.iter().take(3).map(|r| format!("{}: {}", r.name, r.state.text(now))).collect();
            let more = if n > 3 { format!(", and {} more", n - 3) } else { String::new() };
            format!(
                "{n} Location{} need{} attention: {}{more}",
                if n == 1 { "" } else { "s" },
                if n == 1 { "s" } else { "" },
                list.join(", ")
            )
        }
    };
    (health(bad.len()), text)
}

/// The sidebar's Locations row: a network icon, and a dot coloured by overall health where the
/// Tasks row beside it shows its count.
pub fn indicator_row(health: Health, tip: String, open: bool, cx: &App) -> Stateful<Div> {
    let p = palette(cx);
    let (muted, text, active) = (p.muted, p.text, p.active);
    let colour = match health {
        Health::Green => p.positive,
        Health::Yellow => p.warning,
        Health::Red => p.negative,
    };
    h_flex()
        .id("nav-locations")
        .h(px(32.))
        .my(px(1.))
        .px(px(12.))
        .gap(px(10.))
        .items_center()
        .rounded(px(10.))
        .cursor_pointer()
        .text_color(if open { text } else { muted })
        .when(open, |d| d.bg(active))
        .hover(move |s| s.text_color(text))
        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
        .child(Icon::empty().path("icons/network.svg").size(px(16.)))
        .child(div().flex_1().child("Locations"))
        .child(dot(colour).size(px(8.)))
}

/// Every Location with its state and a Sync now. Rebuilt on every render.
pub fn panel<T: 'static>(rows: Vec<Row>, now: i64, engine: &Arc<Engine>, cx: &mut Context<T>) -> Stateful<Div> {
    let p = palette(cx);
    let loading = engine.shared.is_loading();
    let mut body = v_flex().gap(px(10.));
    if rows.is_empty() {
        body = body.child(caption("No Locations are indexed yet.", cx));
    }
    for (ix, row) in rows.into_iter().enumerate() {
        let tone = match &row.state {
            State::Live | State::Synced(_) => p.positive,
            State::Syncing => p.accent,
            State::Offline => p.muted,
            State::Stale | State::Failed(_) => p.warning,
        };
        let busy = row.syncing;
        let reason = if loading {
            Some("Loading the saved Index")
        } else if !row.online {
            Some("Offline")
        } else {
            None
        };
        let (engine, identity) = (engine.clone(), row.identity);
        body = body.child(
            h_flex()
                .gap(px(10.))
                .items_center()
                .child(dot(tone))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().child(row.name))
                        .child(div().text_xs().text_color(p.muted).child(row.state.text(now))),
                )
                .child(
                    pill(("location-sync", ix), "Sync now")
                        .small()
                        .busy(busy)
                        .disabled(reason)
                        .build(cx, cx.listener(move |_, _, _, _| engine.sync_location(identity.clone()))),
                ),
        );
    }
    v_flex()
        .id("locations-panel")
        .w(px(380.))
        .max_h(px(480.))
        .gap(px(12.))
        .p(px(16.))
        .rounded(px(RADIUS_CARD))
        .border_1()
        .border_color(p.border)
        .bg(p.surface)
        .shadow_lg()
        .occlude()
        .child(div().font_weight(FontWeight::SEMIBOLD).child("Locations"))
        .child(v_flex().id("locations-scroll").flex_1().min_h_0().overflow_y_scroll().child(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            online: true,
            failure: None,
            followed: false,
            syncing: false,
            last_synced: Some(1000),
            interval_minutes: 30,
        }
    }

    #[::core::prelude::v1::test]
    fn health_boundaries() {
        assert_eq!(health(0), Health::Green);
        assert_eq!(health(1), Health::Yellow);
        assert_eq!(health(2), Health::Red);
        assert_eq!(health(9), Health::Red);
    }

    #[::core::prelude::v1::test]
    fn up_to_date_within_interval() {
        let f = facts();
        assert_eq!(state(&f, 1000 + 30 * 60 - 1), State::Synced(1000));
        assert_eq!(state(&f, 1000 + 30 * 60), State::Stale);
        // The clock moved back: not trusted.
        assert_eq!(state(&f, 999), State::Stale);
        assert_eq!(state(&Facts { last_synced: None, ..f }, 1000), State::Stale);
    }

    #[::core::prelude::v1::test]
    fn live_and_syncing_are_current_whatever_the_age() {
        let old = Facts {
            last_synced: Some(0),
            ..facts()
        };
        assert_eq!(
            state(
                &Facts {
                    followed: true,
                    ..old.clone()
                },
                10_000_000
            ),
            State::Live
        );
        assert_eq!(state(&Facts { syncing: true, ..old }, 10_000_000), State::Syncing);
    }

    #[::core::prelude::v1::test]
    fn manual_stays_current_once_synced() {
        let f = Facts {
            interval_minutes: 0,
            ..facts()
        };
        assert!(state(&f, 10_000_000).healthy());
        assert!(!state(&Facts { last_synced: None, ..f }, 10_000_000).healthy());
    }

    #[::core::prelude::v1::test]
    fn offline_and_failures_are_unhealthy_even_when_current() {
        assert_eq!(
            state(
                &Facts {
                    online: false,
                    followed: true,
                    ..facts()
                },
                1000
            ),
            State::Offline
        );
        let failing = Facts {
            failure: Some("timed out".into()),
            followed: true,
            ..facts()
        };
        assert!(!state(&failing, 1000).healthy());
    }

    fn row(name: &str, state: State) -> Row {
        Row {
            identity: name.into(),
            name: name.into(),
            syncing: state == State::Syncing,
            state,
            online: true,
        }
    }

    #[::core::prelude::v1::test]
    fn summary_counts_unhealthy_locations() {
        let ok = || row("/", State::Live);
        let (h, t) = summary(&[ok(), ok()], 0);
        assert_eq!(h, Health::Green);
        assert_eq!(t, "Every Location is up to date");
        let (h, t) = summary(&[ok(), row("/mnt/usb", State::Offline)], 0);
        assert_eq!(h, Health::Yellow);
        assert_eq!(t, "1 Location needs attention: /mnt/usb: Offline");
        let (h, t) = summary(&[row("/a", State::Stale), row("/b", State::Offline)], 0);
        assert_eq!(h, Health::Red);
        assert_eq!(t, "2 Locations need attention: /a: Stale, /b: Offline");
    }

    #[::core::prelude::v1::test]
    fn summary_without_locations_and_with_many() {
        let (h, t) = summary(&[], 0);
        assert_eq!(h, Health::Green);
        assert_eq!(t, "No Locations are indexed yet");
        let many: Vec<Row> = ["/a", "/b", "/c", "/d", "/e"].into_iter().map(|n| row(n, State::Stale)).collect();
        let (h, t) = summary(&many, 0);
        assert_eq!(h, Health::Red);
        assert_eq!(t, "5 Locations need attention: /a: Stale, /b: Stale, /c: Stale, and 2 more");
    }

    #[::core::prelude::v1::test]
    fn a_failing_location_that_is_syncing_shows_failed_but_busy() {
        let f = Facts {
            failure: Some("timed out".into()),
            syncing: true,
            ..facts()
        };
        assert_eq!(state(&f, 1000), State::Failed("timed out".into()));
        assert!(f.syncing);
    }

    #[::core::prelude::v1::test]
    fn huge_interval_does_not_overflow() {
        let f = Facts {
            interval_minutes: u64::MAX,
            ..facts()
        };
        assert_eq!(state(&f, 2000), State::Synced(1000));
    }
}
