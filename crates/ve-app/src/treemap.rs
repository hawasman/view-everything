//! The Treemap panel: Filtered Sizes as nested rectangles coloured by Category.

use crate::engine::Shared;
use crate::format;
use crate::theme::palette;
use crate::ui::{caption, swatch};
use crate::view::View;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::BinaryHeap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use ve_core::{Category, CategoryMap, EntryId, Filter, Index};

/// Stop laying out once this many rectangles exist; deeper detail is invisible anyway. Painting
/// costs about a microsecond a tile, so this keeps a frame within 16 ms beside a scrolling list.
const MAX_TILES: usize = 15_000;
/// Folders smaller than this (in pixels, either side) are drawn as one block.
const MIN_FOLDER_SIDE: f32 = 6.0;
/// How often the panel looks for a new View or finished layout.
const POLL: Duration = Duration::from_millis(50);

/// A rectangle in pixels, relative to the panel's drawing area.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Rect {
    fn width(self) -> f32 {
        self.x1 - self.x0
    }

    fn height(self) -> f32 {
        self.y1 - self.y0
    }

    fn area(self) -> f32 {
        self.width().max(0.0) * self.height().max(0.0)
    }

    fn shrink(self, by: f32) -> Rect {
        Rect {
            x0: self.x0 + by,
            y0: self.y0 + by,
            x1: self.x1 - by,
            y1: self.y1 - by,
        }
    }

    fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

struct Tile {
    rect: Rect,
    id: EntryId,
    is_dir: bool,
    /// A file's Category; for a folder shown as one block (see `layout`), the Category holding
    /// most of its size. Meaningless for a folder that is subdivided.
    category: Category,
    /// A folder whose children are laid out inside it.
    subdivided: bool,
}

/// What a set of tiles is laid out for: the View id, the zoom root and the pixel size.
type LayoutKey = (u64, EntryId, [i32; 2]);

struct Request {
    view: Arc<View>,
    root: EntryId,
    size: (f32, f32),
}

/// Lays out tiles on a background thread, so a new View or a zoom never stalls a frame. Only
/// the newest request is served; the panel keeps painting the previous tiles until it arrives.
struct Layouter {
    requests: Sender<Request>,
    results: Receiver<(LayoutKey, Vec<Tile>)>,
    requested: Option<LayoutKey>,
}

impl Layouter {
    fn start(shared: Arc<Shared>, categories: CategoryMap) -> Layouter {
        let (requests, inbox) = mpsc::channel::<Request>();
        let (outbox, results) = mpsc::channel();
        thread::Builder::new()
            .name("ve-treemap".into())
            .spawn(move || {
                while let Ok(first) = inbox.recv() {
                    let request = inbox.try_iter().last().unwrap_or(first);
                    let key = key_of(&request.view, request.root, request.size);
                    let tiles = {
                        let index = shared.index.read();
                        let area = Rect {
                            x0: 0.0,
                            y0: 0.0,
                            x1: request.size.0,
                            y1: request.size.1,
                        };
                        layout(
                            &index,
                            &request.view.filter,
                            &categories,
                            request.root,
                            area,
                        )
                    };
                    if outbox.send((key, tiles)).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn treemap thread");
        Layouter {
            requests,
            results,
            requested: None,
        }
    }

    /// Asks for tiles of `root` in a `size` area under `view`, unless already asked.
    fn request(&mut self, view: &Arc<View>, root: EntryId, size: (f32, f32)) {
        let key = key_of(view, root, size);
        if self.requested == Some(key) {
            return;
        }
        self.requested = Some(key);
        let _ = self.requests.send(Request {
            view: view.clone(),
            root,
            size,
        });
    }
}

fn key_of(view: &View, root: EntryId, size: (f32, f32)) -> LayoutKey {
    (
        view.id,
        root,
        [size.0.round() as i32, size.1.round() as i32],
    )
}

/// Tiles for `root` laid out in `rect`. Folders are subdivided largest first, so when the tile
/// budget runs out the detail missing is in the smallest folders.
fn layout(
    index: &Index,
    filter: &Filter,
    categories: &CategoryMap,
    root: EntryId,
    rect: Rect,
) -> Vec<Tile> {
    let mut tiles: Vec<Tile> = Vec::new();
    // A folder to subdivide: the folder, its area and the tile that stands for it.
    let mut folders = vec![(root, rect, None::<usize>)];
    let mut queue = BinaryHeap::from([(area_key(rect), 0usize)]);
    'layout: while let Some((_, slot)) = queue.pop() {
        let (dir, area, tile) = folders[slot];
        if let Some(tile) = tile {
            tiles[tile].subdivided = true;
        }
        let mut items: Vec<(f64, EntryId)> = index
            .children(dir)
            .iter()
            .filter(|&&c| filter.is_shown(c))
            .map(|&c| (filter.totals(index, c).on_disk as f64, c))
            .filter(|(size, _)| *size > 0.0)
            .collect();
        items.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        for (rect, id) in squarify(&items, area) {
            if tiles.len() >= MAX_TILES {
                break 'layout;
            }
            let is_dir = index.is_dir(id);
            let category = if is_dir {
                Category::Other
            } else {
                categories.of_name(index.name(id))
            };
            tiles.push(Tile {
                rect,
                id,
                is_dir,
                category,
                subdivided: false,
            });
            if is_dir && rect.width() >= MIN_FOLDER_SIDE && rect.height() >= MIN_FOLDER_SIDE {
                let inner = rect.shrink(1.0);
                queue.push((area_key(inner), folders.len()));
                folders.push((id, inner, Some(tiles.len() - 1)));
            }
        }
    }
    // Folders left as one block (too small, or the tile budget ran out before their turn) take
    // the colour of what is inside them rather than showing as empty space.
    for tile in tiles.iter_mut().filter(|t| t.is_dir && !t.subdivided) {
        tile.category = dominant_category(index, filter, categories, tile.id);
    }
    tiles
}

/// How many Entries under a folder are looked at to pick its colour.
const SAMPLE: usize = 200;

/// The Category with the most On-disk Size among the first files found under `dir`.
pub fn dominant_category(
    index: &Index,
    filter: &Filter,
    categories: &CategoryMap,
    dir: EntryId,
) -> Category {
    let mut bytes = [0u64; Category::ALL.len()];
    let mut stack = vec![dir];
    let mut seen = 0;
    while let Some(id) = stack.pop() {
        for &child in index.children(id) {
            if !filter.is_shown(child) {
                continue;
            }
            seen += 1;
            if index.is_dir(child) {
                stack.push(child);
            } else {
                let category = categories.of_name(index.name(child));
                bytes[category as usize] += index.on_disk(child);
            }
            if seen >= SAMPLE {
                stack.clear();
                break;
            }
        }
    }
    let best = (0..bytes.len()).max_by_key(|&i| bytes[i]).unwrap_or(0);
    Category::ALL[best]
}

fn area_key(rect: Rect) -> u64 {
    (rect.area() * 16.0) as u64
}

/// Squarified treemap layout (Bruls, Huizing, van Wijk): splits `area` among `items`, which
/// must be sorted largest first, keeping rectangles close to square.
fn squarify<T: Copy>(items: &[(f64, T)], area: Rect) -> Vec<(Rect, T)> {
    let total: f64 = items.iter().map(|(w, _)| w).sum();
    if total <= 0.0 || area.width() <= 0.0 || area.height() <= 0.0 {
        return Vec::new();
    }
    let scale = (area.width() as f64 * area.height() as f64) / total;
    let areas: Vec<f64> = items.iter().map(|(w, _)| w * scale).collect();
    let mut out = Vec::with_capacity(items.len());
    let mut free = area;
    let mut start = 0;
    while start < items.len() {
        let side = free.width().min(free.height()) as f64;
        let mut end = start + 1;
        let mut best = worst_ratio(&areas[start..end], side);
        while end < items.len() {
            let next = worst_ratio(&areas[start..=end], side);
            if next > best {
                break;
            }
            best = next;
            end += 1;
        }
        let row_area: f64 = areas[start..end].iter().sum();
        let horizontal = free.width() >= free.height();
        // The row fills a strip along the shorter side of the free space.
        let thickness = (row_area / side) as f32;
        let mut offset = 0.0f32;
        for i in start..end {
            let length = (areas[i] / row_area * side) as f32;
            let rect = if horizontal {
                Rect {
                    x0: free.x0,
                    y0: free.y0 + offset,
                    x1: free.x0 + thickness,
                    y1: free.y0 + offset + length,
                }
            } else {
                Rect {
                    x0: free.x0 + offset,
                    y0: free.y0,
                    x1: free.x0 + offset + length,
                    y1: free.y0 + thickness,
                }
            };
            out.push((rect, items[i].1));
            offset += length;
        }
        if horizontal {
            free.x0 += thickness;
        } else {
            free.y0 += thickness;
        }
        start = end;
    }
    out
}

/// The largest aspect ratio in a row of `areas` laid along a side of length `side`.
fn worst_ratio(areas: &[f64], side: f64) -> f64 {
    let sum: f64 = areas.iter().sum();
    let (min, max) = areas
        .iter()
        .fold((f64::MAX, 0.0f64), |(lo, hi), &a| (lo.min(a), hi.max(a)));
    let side2 = side * side;
    let sum2 = sum * sum;
    (side2 * max / sum2).max(sum2 / (side2 * min))
}

/// The deepest tile under (`x`, `y`).
fn hit(tiles: &[Tile], x: f32, y: f32) -> Option<&Tile> {
    // Children follow their parents, but siblings of different folders interleave because of
    // the work queue; pick the smallest containing tile, which is the deepest.
    tiles
        .iter()
        .filter(|t| t.rect.contains(x, y))
        .min_by(|a, b| a.rect.area().total_cmp(&b.rect.area()))
}

fn folder_of(index: &Index, id: EntryId) -> Option<EntryId> {
    if index.is_dir(id) {
        Some(id)
    } else {
        index.parent(id)
    }
}

pub struct Treemap {
    shared: Arc<Shared>,
    categories: CategoryMap,
    layouter: Layouter,
    view: Option<Arc<View>>,
    zoom: Option<EntryId>,
    selected: Option<EntryId>,
    /// The newest finished tiles; may trail the current View by one layout.
    tiles: Rc<Vec<Tile>>,
    /// What `tiles` were laid out for; hit-testing is off while it trails the request.
    tiles_key: Option<LayoutKey>,
    /// The View, zoom and size last turned into a layout request.
    asked: Option<(Option<u64>, Option<EntryId>, [i32; 2])>,
    /// The drawing area in window coordinates, as of the last paint.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The tile under the pointer, with the pointer's position inside the drawing area.
    hovered: Option<(EntryId, Point<Pixels>)>,
}

impl Treemap {
    pub fn new(shared: Arc<Shared>, categories: CategoryMap, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            layouter: Layouter::start(shared.clone(), categories.clone()),
            shared,
            categories,
            view: None,
            zoom: None,
            selected: None,
            tiles: Rc::new(Vec::new()),
            tiles_key: None,
            asked: None,
            bounds: Rc::default(),
            hovered: None,
        }
    }

    /// The folder shown: the zoom target, else the Volume with the most Filtered Size.
    fn root(&self, index: &Index, view: &View) -> Option<EntryId> {
        let usable =
            |id: EntryId| index.is_live(id) && index.is_dir(id) && view.filter.is_shown(id);
        self.zoom.filter(|&id| usable(id)).or_else(|| {
            index
                .volumes()
                .map(|(_, v)| v.root)
                .filter(|&id| usable(id))
                .max_by_key(|&id| view.filter.totals(index, id).on_disk)
                .or_else(|| index.volumes().map(|(_, v)| v.root).next())
        })
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let latest = self.shared.view.lock().clone();
        if !crate::view::same(&self.view, &latest) {
            self.view = latest;
            cx.notify();
        }
        let size = self.bounds.get().size;
        let (w, h) = (f32::from(size.width), f32::from(size.height));
        // Looking up the root locks the Index, so only do it when something it depends on moved.
        let asked = (self.view.as_ref().map(|v| v.id), self.zoom, [w.round() as i32, h.round() as i32]);
        if let Some(view) = &self.view
            && w > 0.0
            && h > 0.0
            && self.asked != Some(asked)
        {
            self.asked = Some(asked);
            let root = self.root(&self.shared.index.read(), view);
            if let Some(root) = root {
                self.layouter.request(view, root, (w, h));
            }
        }
        if let Some((key, tiles)) = self.layouter.results.try_iter().last() {
            self.tiles = Rc::new(tiles);
            self.tiles_key = Some(key);
            self.hovered = None;
            // Screenshots (VE_DEMO): rest the pointer on the second tile.
            #[cfg(feature = "bench")]
            if std::env::var_os("VE_DEMO").is_some() {
                self.hovered = self
                    .tiles
                    .iter()
                    .find(|t| !t.subdivided && t.rect.width() > 60. && t.rect.height() > 50.)
                    .map(|t| (t.id, point(px(t.rect.x0 + 20.), px(t.rect.y0 + 40.))));
            }
            cx.notify();
        }
    }

    fn zoom_to(&mut self, id: EntryId, cx: &mut Context<Self>) {
        self.zoom = Some(id);
        self.selected = None;
        self.hovered = None;
        cx.notify();
    }

    /// The tile at a window position, if it is over the drawing area.
    fn tile_at(&self, position: Point<Pixels>) -> Option<(EntryId, Point<Pixels>)> {
        if self.tiles_key != self.layouter.requested {
            return None;
        }
        let local = position - self.bounds.get().origin;
        hit(&self.tiles, f32::from(local.x), f32::from(local.y)).map(|t| (t.id, local))
    }

    /// Panel title, then the path of the folder shown: Index / home / mohammed 109 GB.
    fn header(&self, index: &Index, view: &View, root: EntryId, cx: &mut Context<Self>) -> Div {
        let (muted, text) = (palette(cx).muted, palette(cx).text);
        let mut chain = vec![root];
        while let Some(parent) = index.parent(*chain.last().unwrap()) {
            chain.push(parent);
        }
        let crumb = |label: String, last: bool| {
            div()
                .text_color(if last { text } else { muted })
                .when(last, |d| d.font_weight(FontWeight::MEDIUM))
                .child(label)
        };
        let mut row = h_flex().gap(px(6.)).items_center().text_xs().text_color(muted).mb(px(12.));
        row = row.child(crumb("Index".into(), false));
        let last = chain.len() - 1;
        for (i, &id) in chain.iter().rev().enumerate() {
            row = row.child("/").child(
                crumb(index.name(id).to_owned(), i == last)
                    .id(("crumb", id.0 as usize))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.zoom_to(id, cx))),
            );
        }
        let title = h_flex()
            .justify_between()
            .items_baseline()
            .mb(px(14.))
            .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child("Treemap"))
            .child(caption("Filtered Size · double-click a folder to zoom", cx));
        v_flex().child(title).child(
            row.child(div().ml(px(6.)).child(format::size(view.filter.totals(index, root).on_disk))),
        )
    }

    /// Legend of the Categories the tiles show, as `.legend`.
    fn legend(&self, cx: &App) -> Div {
        let p = palette(cx);
        let mut seen = [false; Category::ALL.len()];
        for tile in self.tiles.iter().filter(|t| !t.subdivided) {
            seen[tile.category as usize] = true;
        }
        let mut row = h_flex().flex_wrap().gap_x(px(14.)).gap_y(px(4.)).mt(px(12.)).text_xs().text_color(p.muted);
        for category in Category::ALL.into_iter().filter(|c| seen[*c as usize]) {
            row = row.child(
                h_flex()
                    .gap(px(6.))
                    .items_center()
                    .child(swatch(p.category(category)))
                    .child(category.name()),
            );
        }
        row.child(div().ml_auto().child("Tile colour = folder's largest Category"))
    }

    /// The pointer's tooltip (`.tip`): inverted, three short lines.
    fn tooltip(&self, index: &Index, view: &View, root: EntryId, id: EntryId, at: Point<Pixels>, cx: &App) -> Div {
        let totals = view.filter.totals(index, id);
        let whole = view.filter.totals(index, root).on_disk.max(1);
        let first = if index.is_dir(id) {
            format!("{} on disk · {} files", format::size(totals.on_disk), format::count(totals.files))
        } else {
            format!("{} on disk · {}", format::size(totals.on_disk), self.categories.of_name(index.name(id)).name())
        };
        let second = format!(
            "{} size · {:.1}%",
            format::size(totals.logical),
            totals.on_disk as f64 * 100.0 / whole as f64
        );
        let p = palette(cx);
        // Opens right of and below the pointer, flipping to the other side near an edge.
        let surface = self.bounds.get().size;
        let tip = v_flex().absolute().max_w(px(420.));
        let tip = if at.x + px(280.) > surface.width {
            tip.right(surface.width - at.x + px(14.))
        } else {
            tip.left(at.x + px(14.))
        };
        let tip = if at.y + px(80.) > surface.height {
            tip.bottom(surface.height - at.y + px(14.))
        } else {
            tip.top(at.y + px(14.))
        };
        tip.px(px(10.))
            .py(px(6.))
            .gap(px(2.))
            .rounded(px(8.))
            .bg(p.primary)
            .text_color(p.on_primary)
            .text_xs()
            .line_height(px(16.))
            .child(div().font_weight(FontWeight::MEDIUM).child(index.name(id).to_owned()))
            .child(first)
            .child(second)
    }
}

impl Render for Treemap {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (free, ink, text, muted) = (p.free, p.tileink, p.text, p.muted);
        let cats = Category::ALL.map(|c| p.category(c));
        let panel = crate::ui::panel(cx).size_full().flex().flex_col();
        let shared = self.shared.clone();
        let index = shared.index.read();
        let root = self.view.as_ref().and_then(|v| self.root(&index, v));
        let (Some(view), Some(root)) = (self.view.clone(), root) else {
            return panel.child(div().text_color(muted).child("No Volume indexed yet."));
        };

        let tiles = self.tiles.clone();
        let selected = self.selected;
        let bounds = self.bounds.clone();

        let surface = canvas(
            move |b, _, _| bounds.set(b),
            move |b, _, window, _| {
                let at = |r: Rect| {
                    Bounds::new(
                        b.origin + point(px(r.x0), px(r.y0)),
                        size(px(r.width()), px(r.height())),
                    )
                };
                for tile in tiles.iter() {
                    // Folders that are subdivided show only through the gaps between their
                    // children, so they take the panel's own colour.
                    let fill = if tile.subdivided { free } else { cats[tile.category as usize] };
                    // 1px seams in the panel colour separate tiles; tiny ones have no room.
                    let big = tile.rect.width() > 3.0 && tile.rect.height() > 3.0;
                    let radius = if big && !tile.subdivided && tile.rect.width() > 40.0 && tile.rect.height() > 40.0 { 4. } else { 0. };
                    window.paint_quad(quad(
                        at(tile.rect),
                        radius,
                        fill,
                        px(if big { 1. } else { 0. }),
                        free,
                        BorderStyle::Solid,
                    ));
                }
                if let Some(tile) = selected.and_then(|s| tiles.iter().find(|t| t.id == s)) {
                    let clear = gpui_kit::transparent_black();
                    window.paint_quad(quad(at(tile.rect.shrink(1.)), 4., clear, px(2.), text, BorderStyle::Solid));
                }
            },
        )
        .size_full();

        // Names and sizes on the tiles that have room for them, largest first.
        let mut labels = Vec::new();
        for tile in self.tiles.iter().filter(|t| !t.subdivided && t.rect.width() > 70.0 && t.rect.height() > 40.0) {
            if labels.len() >= 60 || !index.is_live(tile.id) {
                continue;
            }
            let parent = index
                .parent(tile.id)
                .filter(|&p| p != root && !tile.is_dir)
                .map(|p| format!("{}/", index.name(p)));
            let on_disk = view.filter.totals(&index, tile.id).on_disk;
            labels.push(
                v_flex()
                    .absolute()
                    .left(px(tile.rect.x0 + 1.))
                    .top(px(tile.rect.y0 + 1.))
                    .w(px(tile.rect.width() - 2.))
                    .h(px(tile.rect.height() - 2.))
                    .overflow_hidden()
                    .px(px(8.))
                    .py(px(6.))
                    .text_xs()
                    .line_height(px(16.))
                    .text_color(ink)
                    .children(parent.map(|name| div().flex_none().text_size(px(11.)).opacity(0.8).child(name)))
                    .child(
                        div()
                            .flex_none()
                            .w_full()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .whitespace_nowrap()
                            .truncate()
                            .child(index.name(tile.id).to_owned()),
                    )
                    .child(div().flex_none().child(format::size(on_disk))),
            );
        }

        let tooltip = self
            .hovered
            .filter(|&(id, _)| index.is_live(id))
            .map(|(id, at)| self.tooltip(&index, &view, root, id, at, cx));
        let header = self.header(&index, &view, root, cx);
        let legend = self.legend(cx);
        panel
            .child(header)
            .child(
                div()
                    .id("treemap-surface")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .rounded(px(12.))
                    .overflow_hidden()
                    .bg(free)
                    .child(surface)
                    .children(labels)
                    .children(tooltip)
                    .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                        let next = this.tile_at(e.position);
                        if next.is_some() || this.hovered.is_some() {
                            this.hovered = next;
                            cx.notify();
                        }
                    }))
                    .on_hover(cx.listener(|this, over: &bool, _, cx| {
                        if !over && this.hovered.take().is_some() {
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(|this, e: &ClickEvent, _, cx| {
                        let Some((id, _)) = this.tile_at(e.position()) else {
                            return;
                        };
                        if e.click_count() >= 2 {
                            let shared = this.shared.clone();
                            let folder = folder_of(&shared.index.read(), id);
                            if let Some(folder) = folder {
                                this.zoom_to(folder, cx);
                            }
                        } else {
                            this.selected = Some(id);
                            cx.notify();
                        }
                    })),
            )
            .child(legend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn squarify_fills_the_area_without_overlap() {
        let items: Vec<(f64, u32)> = [50.0, 30.0, 10.0, 6.0, 4.0]
            .iter()
            .enumerate()
            .map(|(i, &w)| (w, i as u32))
            .collect();
        let area = Rect {
            x0: 10.0,
            y0: 20.0,
            x1: 410.0,
            y1: 220.0,
        };
        let tiles = squarify(&items, area);
        assert_eq!(tiles.len(), items.len());
        let sum: f32 = tiles.iter().map(|(r, _)| r.area()).sum();
        assert!((sum - area.area()).abs() < 1.0, "areas sum to {sum}");
        for (i, (r, _)) in tiles.iter().enumerate() {
            assert!(r.x0 >= area.x0 - 0.01 && r.x1 <= area.x1 + 0.01);
            assert!(r.y0 >= area.y0 - 0.01 && r.y1 <= area.y1 + 0.01);
            // Each tile's share of the area matches its weight.
            let share = r.area() / area.area();
            assert!((share as f64 - items[i].0 / 100.0).abs() < 0.001);
        }
    }

    #[::core::prelude::v1::test]
    fn hit_finds_the_deepest_tile() {
        let outer = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 100.0,
            y1: 100.0,
        };
        let tile = |id, rect, is_dir| Tile {
            rect,
            id: EntryId(id),
            is_dir,
            category: Category::Other,
            subdivided: is_dir,
        };
        let tiles = vec![tile(1, outer, true), tile(2, outer.shrink(10.0), false)];
        assert_eq!(hit(&tiles, 50.0, 50.0).unwrap().id, EntryId(2));
        assert_eq!(hit(&tiles, 5.0, 5.0).unwrap().id, EntryId(1));
        assert!(hit(&tiles, 150.0, 5.0).is_none());
    }
}
