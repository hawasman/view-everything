//! The Dashboard: a glance at the whole Index, built to `designs/final/dashboard.html`. It
//! ignores the Explorer's Query. What it shows comes from one pass over the Index, computed off
//! the UI thread and cached per Index generation (see `dashboard_summary`).

use crate::app::volume_name;
use crate::dashboard_summary::{Lane, Summary, ago, every, oldest, summarize};
use crate::engine::{Phase, Shared};
use crate::format;
use crate::settings::Settings;
use crate::theme::{SERIF_FAMILY, palette};
use crate::trash::{self, Trash};
use crate::ui::{Tone, caption, panel, pill, swatch, tag};
use chrono::{Local, TimeZone, Timelike};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use ve_core::{Capacity, Category, CategoryMap, EntryId, Query, QueryContext};
use ve_scan::VolumeInfo;

/// Never recompute the summary more often than this while the Index is being filled; a
/// computation that took long waits proportionally longer.
const MIN_GAP: Duration = Duration::from_secs(2);
/// A Location bar's segments narrower than this share of the whole are left out.
const SLIVER: f64 = 0.001;
const COLUMN: f32 = 1100.;

/// What a click on the Dashboard asks the window to do.
pub enum Act {
    /// Open the Explorer, with this Query when given.
    Explore(Option<String>),
    /// Forget the Offline Volume with this identity (and its last mount path).
    Forget(String, PathBuf),
    /// Bring every mounted Location up to date now.
    Refresh,
    MoveToTrash(EntryId),
    /// Save the Recently modified Query; `None` restores the default.
    SaveQuery(Option<String>),
}

impl EventEmitter<Act> for DashboardPage {}

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Remote is for Locations reached over a network protocol (issue 15)
enum Kind {
    Volume,
    Network,
    Remote,
}

/// One row of the Locations panel. Built from the Index's Volumes; a Remote is one more row.
struct Location {
    identity: String,
    name: String,
    detail: String,
    kind: Kind,
    online: bool,
    mount: PathBuf,
    last_synced: Option<i64>,
    /// "Every 15 min", for a Location kept current by re-reads.
    interval: Option<String>,
    lane: Lane,
    indexed: u64,
    capacity: Option<Capacity>,
    /// Entries read so far, while it is being synced.
    syncing: Option<u64>,
}

pub struct DashboardPage {
    shared: Arc<Shared>,
    trash: Trash,
    /// The newest summary the worker computed.
    latest: Arc<Mutex<Option<Arc<Summary>>>>,
    shown: Option<Arc<Summary>>,
    queries: Sender<String>,
    volumes: Vec<VolumeInfo>,
    /// Refresh interval in minutes by Volume identity.
    minutes: HashMap<String, u64>,
    /// The Recently modified Query in force.
    recent_query: String,
    editing: Option<Entity<InputState>>,
}

impl DashboardPage {
    pub fn new(
        shared: Arc<Shared>,
        trash: Trash,
        categories: CategoryMap,
        settings: &Settings,
        repaint: crate::engine::Repaint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let (queries, requests) = mpsc::channel();
        let recent_query = settings.recent_query_text();
        let _ = queries.send(recent_query.clone());
        {
            let (shared, latest) = (shared.clone(), latest.clone());
            std::thread::Builder::new()
                .name("ve-dashboard".into())
                .spawn(move || run_summary(&shared, &categories, &requests, &latest, &repaint))
                .expect("spawn dashboard thread");
        }
        #[cfg(feature = "bench")]
        if std::env::var_os("VE_DASH_BENCH").is_some() {
            bench::start(latest.clone(), window, cx);
        }
        #[cfg(not(feature = "bench"))]
        let _ = window;
        let mut page = Self {
            shared,
            trash,
            latest,
            shown: None,
            queries,
            volumes: Vec::new(),
            minutes: HashMap::new(),
            recent_query,
            editing: None,
        };
        page.set_context(settings, Vec::new(), cx);
        page
    }

    /// Takes the settings and mounted Volumes the page reads names and intervals from.
    pub fn set_context(&mut self, settings: &Settings, volumes: Vec<VolumeInfo>, cx: &mut Context<Self>) {
        self.minutes = volumes.iter().map(|v| (v.identity.clone(), settings.refresh_minutes_for(v))).collect();
        self.volumes = volumes;
        let query = settings.recent_query_text();
        if query != self.recent_query {
            self.recent_query = query.clone();
            let _ = self.queries.send(query);
        }
        cx.notify();
    }

    /// Redraws when the worker has a newer summary. Called whenever the window is woken.
    pub fn poll(&mut self, cx: &mut Context<Self>) {
        let latest = self.latest.lock().clone();
        let changed = match (&latest, &self.shown) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (None, None) => false,
            _ => true,
        };
        if changed {
            self.shown = latest;
        }
        cx.notify();
    }

    /// Every Location in the Index, in Index order.
    fn locations(&self) -> Vec<Location> {
        let status = self.shared.status.lock().clone();
        let syncing: &[PathBuf] = match &status.phase {
            Phase::Syncing { volumes, .. } => volumes,
            _ => &[],
        };
        let index = self.shared.index.read();
        index
            .volumes()
            .map(|(_, v)| {
                let mounted = self.volumes.iter().find(|m| m.identity == v.identity);
                let network = mounted.is_some_and(|m| m.network);
                let name = mounted.map_or_else(|| v.mount.display().to_string(), volume_name);
                let categories = self.shown.as_ref().and_then(|s| s.volume(&v.identity)).map(|s| s.categories).unwrap_or_default();
                let lane = Lane::new(categories, v.capacity);
                Location {
                    identity: v.identity.clone(),
                    detail: if network {
                        format!("Network share · {}", v.mount.display())
                    } else {
                        format!("{} · {}", v.fs_type, v.mount.display())
                    },
                    name,
                    kind: if network { Kind::Network } else { Kind::Volume },
                    online: v.online,
                    mount: v.mount.clone(),
                    last_synced: v.last_synced,
                    interval: self.minutes.get(&v.identity).map(|&m| every(m)),
                    indexed: categories.iter().sum(),
                    lane,
                    capacity: v.capacity,
                    syncing: (v.online && syncing.contains(&v.mount)).then(|| self.shared.progress.entries()),
                }
            })
            .collect()
    }

    fn explore(query: Option<String>, cx: &mut Context<Self>) {
        cx.emit(Act::Explore(query));
    }

    fn edit_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.recent_query.clone();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
        cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let text = input.read(cx).value().to_string();
                this.save_query(Some(text), cx);
            }
        })
        .detach();
        input.update(cx, |i, cx| i.focus(window, cx));
        self.editing = Some(input);
        cx.notify();
    }

    fn save_query(&mut self, text: Option<String>, cx: &mut Context<Self>) {
        self.editing = None;
        cx.emit(Act::SaveQuery(text.map(|t| t.trim().to_owned())));
        cx.notify();
    }

    fn greeting(&self, now: i64) -> (String, String) {
        let hour = Local.timestamp_opt(now, 0).single().map_or(12, |t| t.hour());
        let part = match hour {
            5..12 => "Morning",
            12..17 => "Afternoon",
            _ => "Evening",
        };
        let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
        let name: String = user.chars().next().map(|c| c.to_uppercase().chain(user.chars().skip(1)).collect()).unwrap_or_default();
        let hello = if name.is_empty() { part.to_owned() } else { format!("{part}, {name}") };
        let Some(summary) = &self.shown else {
            return (format!("{hello}."), "Reading the Index…".into());
        };
        let fact = match summary.by_size().first() {
            Some(&(category, size, _)) => {
                let whole = summary.indexed().max(1);
                format!("{category} is {:.0}% of your Index.", size as f64 * 100.0 / whole as f64)
            }
            None => "Nothing is indexed yet.".to_owned(),
        };
        (format!("{hello}. {fact}"), String::new())
    }
}

/// Recomputes the summary whenever the Index or the Recently modified Query changed.
fn run_summary(
    shared: &Shared,
    categories: &CategoryMap,
    requests: &Receiver<String>,
    latest: &Mutex<Option<Arc<Summary>>>,
    repaint: &crate::engine::Repaint,
) {
    let mut text = String::new();
    let mut computed: Option<(String, u64)> = None;
    let mut last = Instant::now();
    let mut gap = MIN_GAP;
    loop {
        match requests.recv_timeout(Duration::from_millis(250)) {
            Ok(first) => {
                text = requests.try_iter().last().unwrap_or(first);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let generation = shared.index.read().generation();
        let same_query = computed.as_ref().is_some_and(|(t, _)| *t == text);
        if computed.as_ref().is_some_and(|(t, g)| *t == text && *g == generation) {
            continue;
        }
        // The Index keeps changing while it is scanned: don't chase every change.
        if same_query && last.elapsed() < gap {
            continue;
        }
        // Parsed per computation: a relative date such as `dm:today` means today.
        let context = QueryContext::new(categories.clone(), Local::now());
        let (query, error) = match Query::parse(&text, &context) {
            Ok(q) => (Some(q), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let summary = {
            let index = shared.index.read();
            summarize(&index, categories, query.as_ref(), error.clone())
        };
        gap = MIN_GAP.max(summary.took * 5);
        computed = Some((text.clone(), summary.generation));
        *latest.lock() = Some(Arc::new(summary));
        last = Instant::now();
        repaint();
    }
}

/// The stroke of the Unindexed Space hatch (`--hatch` in ve.css).
fn hatch(cx: &App) -> Rgba {
    if cx.theme().is_dark() { rgb(0x666866) } else { rgb(0xa9acab) }
}

/// "Today 21:58", "Yesterday 22:05", or the date.
fn day(nanos: i64) -> String {
    let Some(at) = Local.timestamp_nanos(nanos).into() else { return String::new() };
    let at: chrono::DateTime<Local> = at;
    let days = (Local::now().date_naive() - at.date_naive()).num_days();
    match days {
        0 => format!("Today {}", at.format("%H:%M")),
        1 => format!("Yesterday {}", at.format("%H:%M")),
        _ => at.format("%Y-%m-%d").to_string(),
    }
}

/// `folder` with the home folder written as `~`.
fn tilde(folder: &str) -> String {
    match dirs::home_dir() {
        Some(home) => match folder.strip_prefix(&*home.to_string_lossy()) {
            Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => format!("~{rest}"),
            _ => folder.to_owned(),
        },
        None => folder.to_owned(),
    }
}

impl Render for DashboardPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let now = chrono::Utc::now().timestamp();
        let locations = self.locations();
        let (title, sub) = self.greeting(now);
        let oldest_ix = oldest(&locations.iter().map(|l| l.last_synced).collect::<Vec<_>>());

        let sub = match (&self.shown, oldest_ix.map(|i| &locations[i])) {
            (Some(s), Some(old)) => {
                let synced = match old.last_synced {
                    Some(at) => format!("Index last synced {}, at {}.", ago(now, at), old.name),
                    None => format!("{} has not finished its first sync.", old.name),
                };
                format!(
                    "{} indexed across {} Location{} and {} Entries. {synced}",
                    format::size(s.indexed()),
                    locations.len(),
                    if locations.len() == 1 { "" } else { "s" },
                    format::count(s.entries),
                )
            }
            _ => sub,
        };

        let head = h_flex()
            .items_start()
            .justify_between()
            .gap(px(24.))
            .mb(px(24.))
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .font_family(SERIF_FAMILY)
                            .text_size(px(48.))
                            .line_height(px(54.))
                            .child(title),
                    )
                    .child(div().mt(px(6.)).text_color(p.muted).child(sub)),
            )
            .child(
                div().flex_none().pt(px(8.)).child(
                    pill("explore", "Explore by size")
                        .primary()
                        .build(cx, cx.listener(|_, _, _, cx| Self::explore(None, cx))),
                ),
            );

        let body = v_flex()
            .child(head)
            .child(self.categories_row(cx))
            .child(
                h_flex()
                    .items_stretch()
                    .gap(px(16.))
                    .mt(px(16.))
                    .child(self.locations_panel(&locations, now, cx))
                    .child(self.side_column(&locations, oldest_ix, now, cx)),
            )
            .child(
                h_flex()
                    .items_start()
                    .gap(px(16.))
                    .mt(px(16.))
                    .child(self.largest_panel(&locations, cx))
                    .child(self.recent_panel(&locations, cx)),
            );
        div()
            .id("dashboard")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .max_w(px(COLUMN + 64.))
                    .mx_auto()
                    .pt(px(32.))
                    .px(px(32.))
                    .pb(px(48.))
                    .child(body),
            )
    }
}

impl DashboardPage {
    /// The four largest Categories as cards, then a line naming the rest.
    fn categories_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let Some(summary) = &self.shown else {
            return div().into_any_element();
        };
        let rows = summary.by_size();
        let whole = summary.indexed().max(1) as f64;
        let share = |size: u64, digits: usize| format!("{:.digits$}%", size as f64 * 100.0 / whole);
        let cards = rows.iter().take(4).enumerate().map(|(ix, &(category, size, files))| {
            let color = p.category(category);
            let name = category.name();
            let query = format!("type:{}", name.to_lowercase());
            let tip = format!("Open the Explorer with {query}");
            panel(cx)
                .id(("category-card", ix))
                .flex_1()
                .flex_basis(relative(0.))
                .min_h(px(116.))
                .pb(px(16.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .border_1()
                .border_color(gpui_kit::transparent_black())
                .cursor_pointer()
                .hover(move |s| s.border_color(p.control))
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .on_click(cx.listener(move |_, _, _, cx| Self::explore(Some(query.clone()), cx)))
                .child(h_flex().gap(px(8.)).items_center().mb(px(4.)).child(swatch(color)).child(name))
                .child(div().text_size(px(32.)).line_height(px(38.)).child(format::size(size)))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(p.muted)
                        .mb(px(14.))
                        .child(format!("{} files · {} of the Index", format::count(files), share(size, 1))),
                )
                .child(
                    div()
                        .mt_auto()
                        .h(px(4.))
                        .rounded(px(2.))
                        .bg(p.free)
                        .overflow_hidden()
                        .child(div().h_full().w(relative((size as f64 / whole) as f32)).bg(color)),
                )
        });
        let rest = rows.len().saturating_sub(4);
        let more = rows.iter().skip(4).enumerate().map(|(ix, &(category, size, _))| {
            let query = format!("type:{}", category.name().to_lowercase());
            h_flex()
                .id(("more-category", ix))
                .gap(px(6.))
                .items_center()
                .cursor_pointer()
                .whitespace_nowrap()
                .hover(move |s| s.text_color(p.accent))
                .on_click(cx.listener(move |_, _, _, cx| Self::explore(Some(query.clone()), cx)))
                .child(swatch(p.category(category)))
                .child(category.name())
                .child(div().font_weight(FontWeight::MEDIUM).text_color(p.text).child(format::size(size)))
        });
        v_flex()
            .child(h_flex().gap(px(16.)).children(cards))
            .when(rest > 0, |d| {
                d.child(
                    h_flex()
                        .mt(px(12.))
                        .gap(px(16.))
                        .flex_wrap()
                        .items_center()
                        .text_size(px(13.))
                        .text_color(p.muted)
                        .child(format!("{rest} more Categor{}", if rest == 1 { "y" } else { "ies" }))
                        .children(more),
                )
            })
            .into_any_element()
    }

    fn locations_panel(&self, locations: &[Location], now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let capacity: u64 = locations.iter().filter_map(|l| l.capacity).map(|c| c.total).sum();
        let free: u64 = locations.iter().filter_map(|l| l.capacity).map(|c| c.free).sum();
        let hatch = hatch(cx);
        let outlined = move |d: Div| d.border_1().border_color(p.control);
        let mut legend = h_flex().flex_wrap().gap_x(px(14.)).gap_y(px(4.)).mb(px(14.)).text_xs().text_color(p.muted);
        for category in Category::ALL {
            legend = legend.child(h_flex().gap(px(6.)).items_center().child(swatch(p.category(category))).child(category.name()));
        }
        legend = legend
            .child(h_flex().gap(px(6.)).items_center().child(outlined(swatch(p.surface).overflow_hidden().child(div().size_full().bg(pattern_slash(hatch, 1., 6.))))).child("Unindexed Space"))
            .child(h_flex().gap(px(6.)).items_center().child(outlined(swatch(p.free))).child("Free"));

        let mut lanes = v_flex().bg(p.raised).rounded(px(crate::theme::RADIUS_INNER)).px(px(20.)).py(px(4.));
        for (ix, l) in locations.iter().enumerate() {
            lanes = lanes.child(self.lane(ix, l, now, cx));
        }
        if locations.is_empty() {
            lanes = lanes.child(div().py(px(16.)).text_color(p.muted).child("No Locations are indexed yet. Add one in Settings."));
        }
        panel(cx)
            .flex_grow(9.)
            .flex_basis(relative(0.))
            .child(
                h_flex()
                    .items_baseline()
                    .justify_between()
                    .mb(px(14.))
                    .child(div().font_weight(FontWeight::MEDIUM).child("Locations"))
                    .child(caption(
                        if capacity > 0 {
                            format!("{} capacity · {} free", format::size(capacity), format::size(free))
                        } else {
                            String::new()
                        },
                        cx,
                    )),
            )
            .child(legend)
            .child(lanes)
    }

    /// One Location: who, a capacity bar split into Categories, Unindexed Space and Free, and
    /// when it was last synced.
    fn lane(&self, ix: usize, l: &Location, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let hatch = hatch(cx);
        let total = l.lane.total().max(1) as f64;
        let wide = |bytes: u64| bytes as f64 / total >= SLIVER;
        let seg = |bytes: u64| div().h_full().min_w_0().flex_grow(bytes as f32).flex_basis(relative(0.));
        let mut bar = h_flex().h(px(12.)).gap(px(1.)).rounded(px(3.)).overflow_hidden().bg(p.free).when(!l.online, |d| d.opacity(0.6));
        for category in Category::ALL.into_iter().filter(|&c| wide(l.lane.categories[c as usize])) {
            let bytes = l.lane.categories[category as usize];
            let tip = format!("{} {}", category.name(), format::size(bytes));
            bar = bar.child(
                seg(bytes)
                    .id(("segment", ix * 8 + category as usize))
                    .bg(p.category(category))
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
            );
        }
        if wide(l.lane.unindexed) {
            let tip = format!("Unindexed Space {}", format::size(l.lane.unindexed));
            bar = bar.child(
                seg(l.lane.unindexed)
                    .id(("unindexed", ix))
                    .bg(p.surface)
                    .child(div().size_full().bg(pattern_slash(hatch, 1., 6.)))
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
            );
        }
        if l.lane.free > 0 {
            let tip = format!("Free {}", format::size(l.lane.free));
            bar = bar.child(
                seg(l.lane.free)
                    .id(("free", ix))
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
            );
        }
        let strong = |text: String| div().text_color(p.text).child(text);
        let figures = match l.capacity {
            Some(c) if l.kind != Kind::Remote => h_flex()
                .child(strong(format::size(c.total.saturating_sub(c.free))))
                .child(format!("\u{a0}used of {}", format::size(c.total)))
                .into_any_element(),
            // A Remote reports no capacity: only what is indexed.
            _ => h_flex().child(strong(format::size(l.indexed))).child("\u{a0}indexed").into_any_element(),
        };
        let free = l.capacity.map(|c| format!("{} free", format::size(c.free)));

        let who = v_flex()
            .min_w_0()
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(div().font_weight(FontWeight::MEDIUM).truncate().child(l.name.clone()))
                    .when(l.kind == Kind::Remote, |d| d.child(tag("Remote", Tone::Neutral, cx)))
                    .when(!l.online, |d| d.child(tag("Offline", Tone::Warning, cx)))
                    .when(l.syncing.is_some(), |d| d.child(tag("Catch-up", Tone::Accent, cx))),
            )
            .child(div().text_size(px(13.)).text_color(p.muted).truncate().child(l.detail.clone()));

        let (main, second) = match (l.last_synced, l.online) {
            (Some(at), false) => (format!("Last seen {}", ago(now, at)), None),
            (Some(at), true) => (ago(now, at), l.interval.clone()),
            (None, _) => ("Never".to_owned(), Some("Waiting for the first sync".to_owned())),
        };
        let mut sync = v_flex().items_end().child(div().text_color(p.text).child(main));
        sync = if let Some(read) = l.syncing {
            sync.child(div().text_size(px(13.)).text_color(p.accent).child(format!("Catching up — {} Entries read", format::count(read))))
        } else if !l.online {
            let (identity, mount) = (l.identity.clone(), l.mount.clone());
            sync.child(
                div()
                    .id(("forget", ix))
                    .text_size(px(13.))
                    .text_color(p.accent)
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .tooltip(|window, cx| {
                        Tooltip::new("Remove this Offline Volume and its Entries from the Index").build(window, cx)
                    })
                    .child("Forget")
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(Act::Forget(identity.clone(), mount.clone())))),
            )
        } else {
            sync.child(div().text_size(px(13.)).text_color(p.muted).child(second.unwrap_or_default()))
        };

        h_flex()
            .min_h(px(60.))
            .py(px(8.))
            .gap(px(24.))
            .items_center()
            .when(ix > 0, |d| d.border_t_1().border_color(p.rule))
            .child(div().w(px(176.)).flex_none().child(who))
            .child(
                v_flex().flex_1().min_w_0().child(bar).child(
                    h_flex()
                        .mt(px(6.))
                        .gap(px(8.))
                        .justify_between()
                        .text_size(px(13.))
                        .text_color(p.muted)
                        .whitespace_nowrap()
                        .child(figures)
                        .children(free),
                ),
            )
            .child(div().w(px(216.)).flex_none().child(sync))
    }

    /// Trash, and the Index's Last Synced (the oldest Location's).
    fn side_column(&self, locations: &[Location], oldest_ix: Option<usize>, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let size = self.trash.size();
        let nothing = size == Some(0);
        let why = if !trash::CAN_EMPTY {
            Some("The Trash cannot be emptied from here on this system")
        } else if nothing {
            Some("The Trash is empty")
        } else {
            None
        };
        let trash = panel(cx)
            .child(div().font_weight(FontWeight::MEDIUM).child("Trash"))
            .child(div().mt(px(12.)).text_size(px(32.)).line_height(px(38.)).child(size.map_or("…".to_owned(), format::size)))
            .child(div().text_xs().text_color(p.muted).mb(px(16.)).child("Trashed bytes still occupy their Volume."))
            .child(
                div().child(
                    pill("empty-trash", "Empty Trash")
                        .disabled(why)
                        .build(cx, cx.listener(|this, _, window, cx| this.trash.ask_empty(window, cx))),
                ),
            );

        let old = oldest_ix.map(|i| &locations[i]);
        let (big, note) = match old {
            Some(l) => match l.last_synced {
                Some(at) => (
                    ago(now, at),
                    format!(
                        "{}, {}: the oldest of {} Locations.",
                        l.name,
                        Local.timestamp_opt(at, 0).single().map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M").to_string()),
                        locations.len()
                    ),
                ),
                None => ("Not yet".to_owned(), format!("{} has not finished its first sync.", l.name)),
            },
            None => ("—".to_owned(), "No Locations yet.".to_owned()),
        };
        let refresh_why = match old {
            None => Some("There is nothing to refresh"),
            Some(l) if !l.online => Some("An Offline Volume cannot be refreshed"),
            _ => None,
        };
        let label = old.map_or("Refresh".to_owned(), |l| format!("Refresh {}", l.name));
        let synced = panel(cx)
            .flex_1()
            .child(div().font_weight(FontWeight::MEDIUM).child("Index last synced"))
            .child(div().mt(px(12.)).text_size(px(32.)).line_height(px(38.)).child(big))
            .child(div().text_xs().text_color(p.muted).mb(px(16.)).child(note))
            .child(
                div().child(
                    pill("refresh", label)
                        .disabled(refresh_why)
                        .build(cx, cx.listener(|_, _, _, cx| cx.emit(Act::Refresh))),
                ),
            );
        v_flex().flex_grow(3.).flex_basis(relative(0.)).gap(px(16.)).child(trash).child(synced)
    }

    /// The right-hand cell of a Largest files row: Move to Trash, or why it cannot be.
    fn row_action(&self, ix: usize, id: EntryId, volume: &str, locations: &[Location], cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let location = locations.iter().find(|l| l.identity == volume);
        let blocked = self.trash.blocker_of(&[id], &self.volumes);
        match (location, blocked) {
            (_, None) => pill(("move-to-trash", ix), "Move to Trash")
                .small()
                .build(cx, cx.listener(move |_, _, _, cx| cx.emit(Act::MoveToTrash(id))))
                .into_any_element(),
            (location, Some(reason)) => {
                let label = match location {
                    Some(l) if !l.online => "Offline",
                    _ => "Read-only",
                };
                div()
                    .id(("blocked", ix))
                    .text_xs()
                    .text_color(p.muted)
                    .child(label)
                    .tooltip(move |window, cx| Tooltip::new(reason.clone()).build(window, cx))
                    .into_any_element()
            }
        }
    }

    /// A table of files: a Category dot, name over folder and Location, size, and a last cell.
    fn file_table(
        &self,
        rows: &[crate::dashboard_summary::FileRow],
        locations: &[Location],
        third: &'static str,
        last: impl Fn(usize, &crate::dashboard_summary::FileRow, &mut Context<Self>) -> AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = palette(cx);
        let cell = |w: f32| div().w(px(w)).flex_none().text_right().whitespace_nowrap();
        let mut table = v_flex().child(
            h_flex()
                .pb(px(8.))
                .text_xs()
                .text_color(p.muted)
                .child(div().flex_1().child("Name"))
                .child(cell(76.).child("Size"))
                .child(cell(132.).pl(px(12.)).child(third)),
        );
        for (ix, r) in rows.iter().enumerate() {
            let place = locations.iter().find(|l| l.identity == r.volume).map_or(String::new(), |l| l.name.clone());
            let folder = tilde(&r.folder);
            table = table.child(
                h_flex()
                    .h(px(54.))
                    .items_center()
                    .border_t_1()
                    .border_color(p.rule)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .pr(px(16.))
                            .gap(px(10.))
                            .items_start()
                            .child(swatch(p.category(r.category)).mt(px(6.)))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().truncate().child(r.name.clone()))
                                    .child(div().text_xs().text_color(p.muted).truncate().child(format!("{folder} · {place}"))),
                            ),
                    )
                    .child(cell(76.).child(format::size(r.size)))
                    .child(cell(132.).pl(px(12.)).flex().justify_end().child(last(ix, r, cx))),
            );
        }
        table.into_any_element()
    }

    fn largest_panel(&self, locations: &[Location], cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let rows = self.shown.as_ref().map(|s| s.largest.clone()).unwrap_or_default();
        let table = self.file_table(
            &rows,
            locations,
            "",
            |ix, r, cx| self.row_action(ix, r.id, &r.volume, locations, cx),
            cx,
        );
        panel(cx)
            .flex_grow(1.)
            .flex_basis(relative(0.))
            .child(
                h_flex()
                    .items_baseline()
                    .justify_between()
                    .mb(px(14.))
                    .child(div().font_weight(FontWeight::MEDIUM).child("Largest files"))
                    .child(
                        div()
                            .id("over-1gb")
                            .text_color(p.accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child("All files over 1 GB")
                            .on_click(cx.listener(|_, _, _, cx| Self::explore(Some("size:>1gb".into()), cx))),
                    ),
            )
            .child(table)
    }

    fn recent_panel(&self, locations: &[Location], cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let rows = self.shown.as_ref().map(|s| s.recent.clone()).unwrap_or_default();
        let error = self.shown.as_ref().and_then(|s| s.recent_error.clone());
        let default = crate::settings::default_recent_query();
        let described = if self.recent_query == default {
            "under ~, outside hidden folders".to_owned()
        } else {
            self.recent_query.clone()
        };
        let link = |id: &'static str, label: &'static str| {
            div().id(id).text_color(p.accent).cursor_pointer().hover(|s| s.underline()).child(label)
        };
        let meta = h_flex()
            .gap(px(6.))
            .min_w_0()
            .text_xs()
            .text_color(p.muted)
            .child(div().truncate().child(described))
            .child("·")
            .child(link("edit-query", "Edit Query").on_click(cx.listener(|this, _, window, cx| this.edit_query(window, cx))));
        let editor = self.editing.as_ref().map(|input| {
            let input = input.clone();
            let field = input.clone();
            v_flex()
                .gap(px(8.))
                .mb(px(14.))
                .child(
                    h_flex()
                        .h(px(32.))
                        .px(px(14.))
                        .items_center()
                        .rounded_full()
                        .border_1()
                        .border_color(p.control)
                        .bg(p.ground)
                        .child(Input::new(&field).appearance(false).bordered(false).small()),
                )
                .child(
                    h_flex()
                        .gap(px(16.))
                        .text_xs()
                        .child(link("save-query", "Save").on_click(cx.listener(move |this, _, _, cx| {
                            let text = input.read(cx).value().to_string();
                            this.save_query(Some(text), cx);
                        })))
                        .child(link("default-query", "Use the default").on_click(cx.listener(|this, _, _, cx| this.save_query(None, cx))))
                        .child(link("cancel-query", "Cancel").on_click(cx.listener(|this, _, _, cx| {
                            this.editing = None;
                            cx.notify();
                        }))),
                )
        });
        let table = self.file_table(
            &rows,
            locations,
            "Modified",
            |_, r, _| div().text_color(p.muted).child(day(r.modified)).into_any_element(),
            cx,
        );
        panel(cx)
            .flex_grow(1.)
            .flex_basis(relative(0.))
            .child(
                h_flex()
                    .items_baseline()
                    .justify_between()
                    .gap(px(12.))
                    .mb(px(14.))
                    .child(div().flex_none().font_weight(FontWeight::MEDIUM).child("Recently modified"))
                    .child(meta),
            )
            .children(editor)
            .when_some(error, |d, e| d.child(div().mb(px(10.)).text_xs().text_color(p.warning).child(format!("This Query cannot be used: {e}"))))
            .child(table)
    }
}

/// `VE_DASH_BENCH=1`: while the Dashboard's first summary of a loaded Index is computed, a 5 ms
/// timer on the UI thread records how late it fires (the longest time the UI thread was blocked),
/// prints that with the computation time, then quits.
#[cfg(feature = "bench")]
mod bench {
    use super::*;

    const TICK: Duration = Duration::from_millis(5);
    /// How long to keep sampling once the summary is in.
    const AFTER: Duration = Duration::from_secs(2);

    pub fn start(latest: Arc<Mutex<Option<Arc<Summary>>>>, window: &mut Window, cx: &mut Context<DashboardPage>) {
        let started = Instant::now();
        cx.spawn_in(window, async move |_, cx| {
            let mut late: Vec<Duration> = Vec::new();
            let mut done_at: Option<Instant> = None;
            loop {
                let before = Instant::now();
                cx.background_executor().timer(TICK).await;
                late.push(before.elapsed().saturating_sub(TICK));
                // The first summary may be of the still-empty Index.
                let summary = latest.lock().clone().filter(|s| s.entries > 0);
                if let (Some(summary), None) = (&summary, done_at) {
                    done_at = Some(Instant::now());
                    println!(
                        "dashboard summary of {} Entries: computed in {} ms, {} ms after launch",
                        summary.entries,
                        summary.took.as_millis(),
                        started.elapsed().as_millis()
                    );
                }
                if done_at.is_some_and(|t| t.elapsed() >= AFTER) {
                    late.sort();
                    let ms = |q: f64| late[((late.len() as f64 * q) as usize).min(late.len() - 1)].as_secs_f64() * 1e3;
                    println!(
                        "UI-thread timer lateness over {} ticks: p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
                        late.len(),
                        ms(0.5),
                        ms(0.99),
                        late.last().unwrap().as_secs_f64() * 1e3
                    );
                    let _ = cx.update(|_, cx| cx.quit());
                    return;
                }
            }
        })
        .detach();
    }
}
