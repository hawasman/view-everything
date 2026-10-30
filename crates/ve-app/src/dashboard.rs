//! The Overview: the answer first (what can be cleaned up), then the Index at a glance. It
//! ignores the Explorer's Query. What it shows comes from one pass over the Index, computed off
//! the UI thread and cached per Index generation (see `dashboard_summary`).

use crate::app::volume_name;
use crate::dashboard_summary::{Summary, ago, group_by_filesystem, oldest, summarize};
use crate::engine::{Phase, Shared};
use crate::format;
use crate::inspector::Inspected;
use crate::settings::Settings;
use crate::theme::{FS_BODY, FS_SMALL, GAP, ROW_H, palette};
use crate::trash::Trash;
use crate::ui::{ButtonKind, Tone, button, caption, empty_state, kpi_strip, link, panel, section_title, swatch, tag};
use chrono::{Local, TimeZone};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Sizable as _, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use ve_core::{Capacity, Category, CategoryMap, Query, QueryContext};
use ve_scan::VolumeInfo;

/// Never recompute the summary more often than this while the Index is being filled; a
/// computation that took long waits proportionally longer.
const MIN_GAP: Duration = Duration::from_secs(2);
/// Rows of Largest files and Recently modified.
const FILE_ROWS: usize = 5;

/// What a click on the Overview asks the window to do.
pub enum Act {
    /// Open the Explorer, with this Query when given.
    Explore(Option<String>),
    /// Open the Cleaner.
    OpenCleaner,
    /// Show this Entry or Location in the Inspector.
    Inspect(Inspected),
    /// Save the Recently modified Query; `None` restores the default.
    SaveQuery(Option<String>),
    /// Start the Live updates a login start held back.
    StartLive,
}

impl EventEmitter<Act> for DashboardPage {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Volume,
    Network,
    Remote,
}

/// One row of the Locations panel: a Remote, or a filesystem with the Volumes mounted from it
/// (btrfs subvolumes and bind mounts share one capacity, so they share one row).
struct Location {
    /// Identity of the first Volume, or the Remote.
    identity: String,
    name: String,
    kind: Kind,
    online: bool,
    /// Last Synced of the least recently synced member.
    last_synced: Option<i64>,
    indexed: u64,
    capacity: Option<Capacity>,
    /// Being synced now.
    syncing: bool,
    /// Volumes (or Remotes) in this row.
    members: usize,
}

/// "N Locations" (a Location is a Volume or a Remote, as in the sidebar).
fn locations_phrase(locations: &[Location]) -> String {
    let count: usize = locations.iter().map(|l| l.members).sum();
    format!("{count} {}", if count == 1 { "Location" } else { "Locations" })
}

/// The machine's name, when the system says it.
fn hostname() -> Option<String> {
    let name = std::fs::read_to_string("/etc/hostname").ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

pub struct DashboardPage {
    shared: Arc<Shared>,
    cleaner: Entity<crate::cleaner::CleanerPage>,
    trash: Trash,
    /// The newest summary the worker computed.
    latest: Arc<Mutex<Option<Arc<Summary>>>>,
    shown: Option<Arc<Summary>>,
    queries: Sender<String>,
    volumes: Vec<VolumeInfo>,
    /// The Recently modified Query in force.
    recent_query: String,
    editing: Option<Entity<InputState>>,
    host: Option<String>,
}

impl DashboardPage {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shared: Arc<Shared>,
        cleaner: Entity<crate::cleaner::CleanerPage>,
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
            cleaner,
            trash,
            latest,
            shown: None,
            queries,
            volumes: Vec::new(),
            recent_query,
            editing: None,
            host: hostname(),
        };
        // The finding line shows what the Cleaner's background look found.
        cx.observe(&page.cleaner, |_, _, cx| cx.notify()).detach();
        page.set_context(settings, Vec::new(), cx);
        page
    }

    /// Takes the settings and mounted Volumes the page reads names from.
    pub fn set_context(&mut self, settings: &Settings, volumes: Vec<VolumeInfo>, cx: &mut Context<Self>) {
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

    /// Every Location in the Index, in Index order, the Volumes of one filesystem as one.
    fn locations(&self) -> Vec<Location> {
        let status = self.shared.status.lock().clone();
        let syncing: &[PathBuf] = match &status.phase {
            Phase::Syncing { volumes, .. } => volumes,
            _ => &[],
        };
        let index = self.shared.index.read();
        let singles: Vec<(Location, String)> = index
            .volumes()
            .map(|(_, v)| {
                let mounted = self.volumes.iter().find(|m| m.identity == v.identity);
                let network = mounted.is_some_and(|m| m.network);
                let indexed: u64 = self
                    .shown
                    .as_ref()
                    .and_then(|s| s.volume(&v.identity))
                    .map(|s| s.categories.iter().sum())
                    .unwrap_or_default();
                let remote = v.remote();
                let name = match (&remote, mounted) {
                    (Some(r), _) => r.display_name(v.name.as_deref()),
                    (None, Some(m)) => volume_name(m),
                    (None, None) => v.mount.display().to_string(),
                };
                let location = Location {
                    identity: v.identity.clone(),
                    name,
                    kind: if remote.is_some() {
                        Kind::Remote
                    } else if network {
                        Kind::Network
                    } else {
                        Kind::Volume
                    },
                    online: v.online,
                    last_synced: v.last_synced,
                    indexed,
                    capacity: v.capacity,
                    syncing: v.online && syncing.contains(&v.mount),
                    members: 1,
                };
                (location, mounted.map_or_else(String::new, |m| m.label.clone()))
            })
            .collect();
        let identities: Vec<&str> = singles.iter().map(|(l, _)| l.identity.as_str()).collect();
        let groups = group_by_filesystem(&identities);
        let mut singles: Vec<Option<(Location, String)>> = singles.into_iter().map(Some).collect();
        let mount_len = |identity: &str| {
            index
                .volume_with_identity(identity)
                .and_then(|id| index.volume(id))
                .map_or(0, |v| v.mount.as_os_str().len())
        };
        groups
            .into_iter()
            .map(|group| {
                let mut parts: Vec<(Location, String)> = group.into_iter().filter_map(|i| singles[i].take()).collect();
                if parts.len() == 1 {
                    return parts.remove(0).0;
                }
                // The shortest mount path (usually `/`) speaks for the filesystem.
                parts.sort_by_key(|(l, _)| mount_len(&l.identity));
                let device = parts[0].1.trim_start_matches("/dev/").to_owned();
                let mut whole = parts.remove(0).0;
                for (part, _) in parts {
                    whole.indexed += part.indexed;
                    whole.online |= part.online;
                    whole.last_synced = whole.last_synced.min(part.last_synced);
                    whole.syncing |= part.syncing;
                    // Every Volume measures the same filesystem: any one's figures will do.
                    whole.capacity = whole.capacity.or(part.capacity);
                    whole.members += part.members;
                }
                if !device.is_empty() {
                    whole.name = device;
                }
                whole
            })
            .collect()
    }

    /// Files in the Index on the Volume or Remote with this identity, once the summary is in.
    pub fn files_on(&self, identity: &str) -> Option<u64> {
        self.shown.as_ref()?.volume(identity).map(|v| v.files)
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
        let context = QueryContext::new(categories.clone(), Local::now()).with_duplicates(shared.duplicates.lock().set.clone());
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

/// "Today 21:58", "Yesterday 22:05", or the date.
fn day(nanos: i64) -> String {
    let Some(at) = Local.timestamp_nanos(nanos).into() else {
        return String::new();
    };
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

/// Where the Cleaner's look stands, as the Overview tells it.
enum Finding {
    /// Not looked yet, or looking.
    Looking {
        spinning: bool,
    },
    Found {
        reclaim: u64,
        count: usize,
    },
    Nothing,
}

impl Render for DashboardPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = crate::trace::span("DashboardPage::render");
        let p = palette(cx);
        let now = chrono::Utc::now().timestamp();
        let locations = self.locations();
        let oldest_loc = oldest(&locations.iter().map(|l| l.last_synced).collect::<Vec<_>>()).map(|i| &locations[i]);

        // The Cleaner looks again only when the Index changed, so this follows every Clean.
        let (reclaim, count, looking) = self.cleaner.update(cx, |c, cx| {
            c.refresh(cx);
            let (reclaim, count) = c.reclaimable();
            (reclaim, count, c.looking())
        });
        let finding = if looking {
            Finding::Looking { spinning: true }
        } else if self.shared.is_loading() || self.shown.is_none() {
            Finding::Looking { spinning: false }
        } else if count > 0 {
            Finding::Found { reclaim, count }
        } else {
            Finding::Nothing
        };

        let synced_fact = match oldest_loc {
            Some(l) => match l.last_synced {
                Some(at) => Some(format!("Index last synced {}", ago(now, at))),
                None => Some(format!("{} has not finished its first sync", l.name)),
            },
            None => None,
        };
        let os = match std::env::consts::OS {
            "linux" => "Linux",
            "macos" => "macOS",
            "windows" => "Windows",
            other => other,
        };
        let facts = self
            .host
            .iter()
            .cloned()
            .chain([os.to_owned()])
            .chain(synced_fact)
            .collect::<Vec<_>>()
            .join(" · ");

        let head = h_flex()
            .items_start()
            .justify_between()
            .gap(px(GAP))
            .child(crate::ui::title_row("Overview", Some(facts), cx))
            .when(matches!(finding, Finding::Found { .. }), |d| {
                d.child(
                    div().flex_none().child(
                        button("open-cleaner", "Open Cleaner")
                            .kind(ButtonKind::Primary)
                            .build(cx, cx.listener(|_, _, _, cx| cx.emit(Act::OpenCleaner))),
                    ),
                )
            });

        let sentence = match &finding {
            Finding::Looking { .. } => "Looking for Cleanables…".to_owned(),
            Finding::Found { reclaim, count } => format!(
                "{} can be cleaned up across {} Cleanable{}.",
                format::size(*reclaim),
                format::count(*count as u64),
                if *count == 1 { "" } else { "s" }
            ),
            Finding::Nothing => "Nothing to clean up.".to_owned(),
        };
        let finding_line = panel(cx)
            .flex()
            .items_center()
            .justify_between()
            .gap(px(GAP))
            .child(
                h_flex()
                    .gap(px(10.))
                    .items_center()
                    .text_size(px(FS_BODY))
                    .text_color(p.text)
                    .when(matches!(finding, Finding::Looking { spinning: true }), |d| {
                        d.child(Spinner::new().with_size(Size::Small).color(p.muted))
                    })
                    .child(sentence),
            )
            .when(matches!(finding, Finding::Found { .. }), |d| {
                d.child(
                    link("review-plan", "Review Cleanup Plan", cx)
                        .flex_none()
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(Act::OpenCleaner))),
                )
            });

        let dash = || SharedString::from("—");
        let indexed = match &self.shown {
            Some(s) => (
                format::size(s.indexed()).into(),
                Some(format!("{} Entries · {}", format::count(s.entries), locations_phrase(&locations)).into()),
            ),
            None => (dash(), Some("not read yet".into())),
        };
        let cleanable = match &finding {
            Finding::Looking { .. } => (dash(), Some("not looked yet".into())),
            Finding::Found { reclaim, count } => (
                format::size(*reclaim).into(),
                Some(format!("{} Cleanable{}", format::count(*count as u64), if *count == 1 { "" } else { "s" }).into()),
            ),
            Finding::Nothing => (format::size(0).into(), Some("nothing found".into())),
        };
        let trash = match self.trash.size() {
            Some(size) => (
                format::size(size).into(),
                self.trash
                    .items()
                    .map(|n| format!("{} item{}", format::count(n), if n == 1 { "" } else { "s" }).into()),
            ),
            None => (dash(), None),
        };
        let last = match oldest_loc {
            Some(l) => match l.last_synced {
                Some(at) => (
                    ago(now, at).into(),
                    Some(if locations.iter().any(|l| l.syncing) {
                        let name = locations.iter().find(|l| l.syncing).map_or("", |l| l.name.as_str());
                        format!("{name} syncing").into()
                    } else {
                        l.name.clone().into()
                    }),
                ),
                None => ("Not yet".into(), Some(format!("{} has not finished its first sync", l.name).into())),
            },
            None => (dash(), Some("no Locations yet".into())),
        };
        let kpis = kpi_strip(
            vec![
                ("Indexed".into(), indexed.0, indexed.1),
                ("Cleanable".into(), cleanable.0, cleanable.1),
                ("Trash".into(), trash.0, trash.1),
                ("Last synced".into(), last.0, last.1),
            ],
            cx,
        );

        v_flex().id("dashboard").size_full().overflow_y_scroll().child(
            v_flex()
                .gap(px(GAP))
                .child(head)
                .when(cx.try_global::<crate::host::Host>().is_some_and(|h| h.live_paused()), |d| {
                    d.child(
                        panel(cx)
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(GAP))
                            .child(
                                div()
                                    .text_size(px(FS_BODY))
                                    .child("Live updates are paused: not started at login, to spare you the password prompt. Changes are found when a Volume is synced."),
                            )
                            .child(
                                button("start-live", "Start")
                                    .build(cx, cx.listener(|_, _, _, cx| cx.emit(Act::StartLive))),
                            ),
                    )
                })
                .child(finding_line)
                .child(kpis)
                .child(
                    h_flex()
                        .items_start()
                        .gap(px(GAP))
                        .child(self.locations_panel(&locations, now, cx))
                        .child(self.categories_panel(cx)),
                )
                .children(self.owners_panel(cx))
                .child(
                    h_flex()
                        .items_start()
                        .gap(px(GAP))
                        .child(self.largest_panel(&locations, cx))
                        .child(self.recent_panel(&locations, cx)),
                ),
        )
    }
}

impl DashboardPage {
    /// One row per Location: name (and its kind when not a local Volume), used bar, figures,
    /// Last Synced. A click shows it in the Inspector.
    fn locations_panel(&self, locations: &[Location], now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let mut rows = v_flex().mt(px(GAP));
        for (ix, l) in locations.iter().enumerate() {
            let identity = l.identity.clone();
            let figures = match l.capacity {
                Some(c) => h_flex()
                    .justify_end()
                    .child(div().text_color(p.text).child(format::size(c.total.saturating_sub(c.free))))
                    .child(format!("\u{a0}/ {}", format::size(c.total))),
                None => h_flex()
                    .justify_end()
                    .child(div().text_color(p.text).child(format::size(l.indexed)))
                    .child("\u{a0}indexed"),
            };
            let bar = l.capacity.filter(|c| c.total > 0).map(|c| {
                let used = c.total.saturating_sub(c.free) as f64 / c.total as f64;
                div()
                    .w(px(120.))
                    .flex_none()
                    .h(px(6.))
                    .rounded(px(3.))
                    .overflow_hidden()
                    .bg(p.free)
                    .child(div().h_full().w(relative(used.clamp(0., 1.) as f32)).bg(p.accent))
            });
            let synced = if l.syncing {
                div().text_color(p.text).child("Syncing")
            } else {
                div().text_color(p.muted).child(match (l.last_synced, l.online) {
                    (Some(at), true) => ago(now, at),
                    (Some(at), false) => format!("Last seen {}", ago(now, at)),
                    (None, _) => "Never".to_owned(),
                })
            };
            rows = rows.child(
                h_flex()
                    .id(("location", ix))
                    .h(px(ROW_H))
                    .gap(px(GAP))
                    .items_center()
                    .cursor_pointer()
                    .when(ix > 0, |d| d.border_t_1().border_color(p.rule))
                    .hover(move |s| s.bg(p.raised))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(Act::Inspect(Inspected::Location(identity.clone())));
                    }))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(8.))
                            .items_center()
                            .child(div().truncate().text_color(p.text).child(l.name.clone()))
                            .when(l.kind == Kind::Remote, |d| d.child(tag("Remote", Tone::Neutral, cx)))
                            .when(l.kind == Kind::Network, |d| d.child(tag("Network", Tone::Neutral, cx)))
                            .when(!l.online, |d| d.child(tag("Offline", Tone::Warning, cx))),
                    )
                    .children(bar)
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .whitespace_nowrap()
                            .text_color(p.muted)
                            .text_right()
                            .child(figures),
                    )
                    .child(div().w(px(100.)).flex_none().whitespace_nowrap().text_right().child(synced)),
            );
        }
        if locations.is_empty() {
            rows = rows.child(empty_state("No Locations are indexed yet. Add one in Settings.", cx));
        }
        panel(cx)
            .flex_grow(9.)
            .flex_basis(relative(0.))
            .text_size(px(FS_BODY))
            .child(
                h_flex()
                    .items_baseline()
                    .justify_between()
                    .child(section_title("Locations", cx))
                    .child(caption("Used · Last Synced", cx)),
            )
            .child(rows)
    }

    /// The eight Categories: swatch, name, bar, size, share of the Index.
    fn categories_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let summary = self.shown.as_ref();
        let whole = summary.map_or(0, |s| s.indexed());
        let mut rows: Vec<(Category, Option<u64>)> = Category::ALL
            .iter()
            .map(|&c| (c, summary.map(|s| s.categories[c as usize].0)))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1.unwrap_or(0)));
        let mut list = v_flex().mt(px(GAP));
        for (ix, (category, size)) in rows.into_iter().enumerate() {
            let color = p.category(category);
            let name = category.name();
            let query = format!("type:{}", name.to_lowercase());
            let share = size.filter(|_| whole > 0).map(|s| s as f64 / whole as f64);
            list = list.child(
                h_flex()
                    .id(("category", ix))
                    .h(px(ROW_H))
                    .gap(px(10.))
                    .items_center()
                    .cursor_pointer()
                    .when(ix > 0, |d| d.border_t_1().border_color(p.rule))
                    .hover(move |s| s.bg(p.raised))
                    .on_click(cx.listener(move |_, _, _, cx| Self::explore(Some(query.clone()), cx)))
                    .child(swatch(color))
                    .child(div().w(px(76.)).flex_none().truncate().text_color(p.text).child(name))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(6.))
                            .rounded(px(3.))
                            .overflow_hidden()
                            .bg(p.free)
                            .child(div().h_full().w(relative(share.unwrap_or(0.) as f32)).bg(color)),
                    )
                    .child(
                        div()
                            .w(px(72.))
                            .flex_none()
                            .whitespace_nowrap()
                            .text_right()
                            .text_color(p.text)
                            .child(size.map_or("—".to_owned(), format::size)),
                    )
                    .child(
                        div()
                            .w(px(44.))
                            .flex_none()
                            .whitespace_nowrap()
                            .text_right()
                            .text_color(p.muted)
                            .child(share.map_or("—".to_owned(), |s| format!("{:.0}%", s * 100.))),
                    ),
            );
        }
        panel(cx)
            .flex_grow(4.)
            .flex_basis(relative(0.))
            .text_size(px(FS_BODY))
            .child(section_title("Categories", cx))
            .child(list)
    }

    /// Up to `FILE_ROWS` files: Category swatch, name over folder and Location, then the size
    /// (and when modified). A click shows the Entry in the Inspector.
    fn file_rows(
        &self,
        prefix: &'static str,
        rows: &[crate::dashboard_summary::FileRow],
        locations: &[Location],
        modified: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let p = palette(cx);
        let mut list = v_flex().mt(px(GAP));
        if rows.is_empty() {
            return list.child(empty_state("Nothing yet.", cx));
        }
        for (ix, r) in rows.iter().take(FILE_ROWS).enumerate() {
            let id = r.id;
            let place = locations
                .iter()
                .find(|l| l.identity == r.volume)
                .map_or(String::new(), |l| l.name.clone());
            let folder = tilde(&r.folder);
            list = list.child(
                h_flex()
                    .id((prefix, ix))
                    .h(px(ROW_H))
                    .gap(px(10.))
                    .items_center()
                    .cursor_pointer()
                    .when(ix > 0, |d| d.border_t_1().border_color(p.rule))
                    .hover(move |s| s.bg(p.raised))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(Act::Inspect(Inspected::Entry(id)))))
                    .child(swatch(p.category(r.category)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().text_color(p.text).child(r.name.clone()))
                            .child(
                                div()
                                    .text_size(px(FS_SMALL))
                                    .text_color(p.muted)
                                    .truncate()
                                    .child(format!("{folder} · {place}")),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_none()
                            .items_end()
                            .whitespace_nowrap()
                            .child(div().text_color(p.text).child(format::size(r.size)))
                            .when(modified, |d| {
                                d.child(div().text_size(px(FS_SMALL)).text_color(p.muted).child(day(r.modified)))
                            }),
                    ),
            );
        }
        list
    }

    fn largest_panel(&self, locations: &[Location], cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.shown.as_ref().map(|s| s.largest.clone()).unwrap_or_default();
        let list = self.file_rows("largest", &rows, locations, false, cx);
        panel(cx)
            .flex_grow(1.)
            .flex_basis(relative(0.))
            .text_size(px(FS_BODY))
            .child(
                h_flex()
                    .items_baseline()
                    .justify_between()
                    .child(section_title("Largest files", cx))
                    .child(
                        link("over-1gb", "View all", cx).on_click(cx.listener(|_, _, _, cx| Self::explore(Some("size:>1gb".into()), cx))),
                    ),
            )
            .child(list)
    }

    /// The Largest Owners card: top Owners by Folder Size, each opening the Explorer on its Owner.
    /// Nothing when the Index has no Owner roots.
    fn owners_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = palette(cx);
        let rows = self.shown.as_ref().map(|s| s.owners.clone()).filter(|r| !r.is_empty())?;
        let list = rows.into_iter().enumerate().map(|(ix, r)| {
            let query = format!("owner:\"{}\"", r.name.replace('"', ""));
            h_flex()
                .id(("owner", ix))
                .items_center()
                .gap(px(10.))
                .py(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(p.control))
                .child(div().flex_grow(1.).min_w_0().truncate().child(r.name))
                .child(div().flex_none().text_xs().text_color(p.muted).child(r.kind.name()))
                .child(div().flex_none().w(px(80.)).text_right().child(format::size(r.size)))
                .on_click(cx.listener(move |_, _, _, cx| Self::explore(Some(query.clone()), cx)))
        });
        Some(
            panel(cx)
                .child(section_title("Largest Owners", cx))
                .child(v_flex().mt(px(GAP)).children(list))
                .into_any_element(),
        )
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
        let link = |id: &'static str, label: &'static str| crate::ui::link(id, label, &*cx);
        let meta = h_flex()
            .gap(px(6.))
            .min_w_0()
            .text_size(px(FS_SMALL))
            .text_color(p.muted)
            .child(div().truncate().child(described))
            .child("·")
            .child(link("edit-query", "Edit Query").on_click(cx.listener(|this, _, window, cx| this.edit_query(window, cx))));
        let editor = self.editing.as_ref().map(|input| {
            let input = input.clone();
            let field = input.clone();
            v_flex()
                .gap(px(8.))
                .mt(px(GAP))
                .child(
                    h_flex()
                        .h(px(crate::theme::CONTROL_H_SMALL))
                        .px(px(14.))
                        .items_center()
                        .rounded(px(crate::theme::RADIUS_CONTROL))
                        .border_1()
                        .border_color(p.control)
                        .bg(p.ground)
                        .child(Input::new(&field).appearance(false).bordered(false).small()),
                )
                .child(
                    h_flex()
                        .gap(px(16.))
                        .text_size(px(FS_SMALL))
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
        let list = self.file_rows("recent", &rows, locations, true, cx);
        panel(cx)
            .flex_grow(1.)
            .flex_basis(relative(0.))
            .text_size(px(FS_BODY))
            .child(
                h_flex()
                    .items_baseline()
                    .justify_between()
                    .gap(px(GAP))
                    .child(section_title("Recently modified", cx).flex_none())
                    .child(meta),
            )
            .children(editor)
            .when_some(error, |d, e| {
                d.child(
                    div()
                        .mt(px(GAP))
                        .text_size(px(FS_SMALL))
                        .text_color(p.warning)
                        .child(format!("This Query cannot be used: {e}")),
                )
            })
            .child(list)
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
                    let _ = cx.update(|_, cx| crate::host::Host::quit(cx));
                    return;
                }
            }
        })
        .detach();
    }
}
