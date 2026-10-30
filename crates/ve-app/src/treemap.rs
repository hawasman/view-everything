//! The Treemap panel: Filtered Sizes as nested rectangles coloured by Category.

use crate::engine::Shared;
use crate::format;
use crate::theme::{Palette, palette};
use crate::ui::{caption, pill, swatch};
use crate::view::{Focus, View};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use ve_core::{Category, CategoryMap, EntryId, Filter, Index, OwnerKind};

/// Stop laying out once this many rectangles exist (a safety cap; the per-folder cap below keeps
/// ordinary layouts far under it). Painting costs about a microsecond a tile.
const MAX_TILES: usize = 15_000;
/// A folder shows its largest this-many children; the rest merge into one "N more" tile.
/// (Technique borrowed from disktree, MIT.)
const MAX_CHILDREN: usize = 96;
/// Tiles thinner than this (in pixels, either side) are not drawn.
const MIN_TILE: f32 = 5.0;
/// How often the panel looks for a new View or finished layout.
const POLL: Duration = Duration::from_millis(50);
/// A panel not painted for this long is off screen: it polls at `IDLE_POLL` and asks for no layout.
const HIDDEN_AFTER: Duration = Duration::from_millis(400);
const IDLE_POLL: Duration = Duration::from_millis(500);

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

impl Rect {
    /// The rectangle `t` of the way from `self` to `to`.
    fn lerp(self, to: Rect, t: f32) -> Rect {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Rect {
            x0: mix(self.x0, to.x0),
            y0: mix(self.y0, to.y0),
            x1: mix(self.x1, to.x1),
            y1: mix(self.y1, to.y1),
        }
    }
}

/// How far a transition that began at `since` has run, 0 to 1; 1 when motion is off.
fn fraction(since: Instant, total: Duration) -> f32 {
    if total.is_zero() {
        1.0
    } else {
        (since.elapsed().as_secs_f32() / total.as_secs_f32()).min(1.0)
    }
}

/// Where each tile of a zoom starts from: its rectangle in the previous layout, else the zoomed
/// folder's rectangle (tiles grow out of the folder that was clicked), else its own rectangle
/// fully transparent (zooming out shows new tiles fading in). The second field is the start opacity.
fn zoom_from(old: &[Tile], new: &[Tile], new_root: Focus) -> Vec<(Rect, f32)> {
    let before: HashMap<EntryId, Rect> = old.iter().filter(|t| t.more.is_none()).map(|t| (t.id, t.rect)).collect();
    let origin = new_root.folder().and_then(|id| before.get(&id).copied());
    new.iter()
        .map(|t| match (before.get(&t.id).filter(|_| t.more.is_none()), origin) {
            (Some(&r), _) | (None, Some(r)) => (r, 1.0),
            (None, None) => (t.rect, 0.0),
        })
        .collect()
}

/// A zoom in progress: the tiles are drawn between where they were and where they are.
struct Transition {
    from: Rc<Vec<(Rect, f32)>>,
    started: Instant,
}

struct Tile {
    rect: Rect,
    id: EntryId,
    is_dir: bool,
    /// A file's Category; for a folder, the Category holding most of its size.
    category: Category,
    /// The kind of the Entry's Owner, if it has one.
    owner: Option<OwnerKind>,
    /// A folder whose children are laid out inside it (under a header band).
    subdivided: bool,
    /// How deep under the zoom root: 1 directly under it; drives the shade ladder.
    level: u8,
    /// The merged tail of a folder's children: how many Entries and their On-disk Size. Its `id`
    /// is the folder, so selecting or zooming it acts on that folder.
    more: Option<(usize, u64)>,
    /// Index in the tile list of the folder this tile is drawn in; None directly under the zoom root.
    parent: Option<usize>,
}

/// Gap kept around a tile: wider between top-level folders, so they read as separate blocks.
const GAP_TOP: f32 = 1.5;
const GAP: f32 = 0.5;

impl Tile {
    /// The painted rectangle: the layout cell less the gap that separates it from its neighbours.
    fn visible(&self) -> Rect {
        self.rect.shrink(if self.parent.is_none() { GAP_TOP } else { GAP })
    }
}

/// The colour `t` of the way from `a` to `b`, mixed in RGB.
fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    Hsla::from(Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    })
}

/// A Category colour muted for a tile `level` folders deep: slightly less saturated, then one
/// step lighter (dark theme) or darker (light theme) per level, clamped to 5 steps.
fn shade(color: Hsla, level: u8, dark: bool) -> Hsla {
    let step = f32::from(level.saturating_sub(1).min(4));
    let l = if dark { color.l + 0.03 * step } else { color.l - 0.03 * step };
    Hsla {
        s: color.s * 0.8,
        l: l.clamp(0.0, 1.0),
        ..color
    }
}

/// What a set of tiles is laid out for: the View id, the zoom root, the pixel size and the Depth.
type LayoutKey = (u64, Focus, [i32; 2], u8);

struct Request {
    view: Arc<View>,
    root: Focus,
    size: (f32, f32),
    depth: u8,
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
                    let key = key_of(&request.view, request.root, request.size, request.depth);
                    let area = Rect {
                        x0: 0.0,
                        y0: 0.0,
                        x1: request.size.0,
                        y1: request.size.1,
                    };
                    let mut tiles = {
                        let index = shared.index.read();
                        layout_tiles(&index, &request.view.filter, &categories, request.root, area, request.depth)
                    };
                    // Folder colours walk many Entries: take the lock a slice of tiles at a time,
                    // so a sync writing to the Index waits for one slice, not the whole layout.
                    for slice in tiles.chunks_mut(64) {
                        let index = shared.index.read();
                        colour_folders(slice, &index, &request.view.filter, &categories);
                    }
                    if outbox.send((key, tiles)).is_err() {
                        return;
                    }
                    (shared.repaint)();
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
    fn request(&mut self, view: &Arc<View>, root: Focus, size: (f32, f32), depth: u8) {
        let key = key_of(view, root, size, depth);
        if self.requested == Some(key) {
            return;
        }
        self.requested = Some(key);
        let _ = self.requests.send(Request {
            view: view.clone(),
            root,
            size,
            depth,
        });
    }
}

fn key_of(view: &View, root: Focus, size: (f32, f32), depth: u8) -> LayoutKey {
    (view.id, root, [size.0.round() as i32, size.1.round() as i32], depth)
}

/// Height of the band carrying a folder's name and size: taller for top-level folders.
fn header_h(level: u8) -> f32 {
    if level <= 1 { 20.0 } else { 15.0 }
}
/// A folder gets a header band, and so is subdivided, only when its tile is at least this wide
/// and at least `MIN_BODY_H` of it is left under the band.
const MIN_HEADER_W: f32 = 44.0;
const MIN_BODY_H: f32 = 15.0;
/// The Depth control's range and default: how many levels of folders are subdivided.
pub const MIN_DEPTH: u8 = 1;
pub const MAX_DEPTH: u8 = 8;
pub const DEFAULT_DEPTH: u8 = 3;

/// What the Treemap's tiles are coloured by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ColourBy {
    #[default]
    Category,
    OwnerKind,
}

/// The area for the children of a folder drawn in `rect` at `level` (1 directly under the zoom
/// root), or None when it is past the `depth` or has no room for a header band.
fn plan_folder(rect: Rect, level: u8, depth: u8) -> Option<Rect> {
    if level > depth {
        return None;
    }
    let gap = if level <= 1 { GAP_TOP } else { GAP };
    let inner = rect.shrink(gap);
    let band = header_h(level);
    (rect.width() >= MIN_HEADER_W && inner.height() - band >= MIN_BODY_H).then_some(Rect {
        y0: inner.y0 + band,
        ..inner
    })
}

/// Rounds every edge of `r` to whole pixels (edges, not sizes, so neighbours stay flush) and
/// keeps it inside `area`.
fn snap(r: Rect, area: Rect) -> Rect {
    Rect {
        x0: r.x0.round().clamp(area.x0, area.x1),
        y0: r.y0.round().clamp(area.y0, area.y1),
        x1: r.x1.round().clamp(area.x0, area.x1),
        y1: r.y1.round().clamp(area.y0, area.y1),
    }
}

/// Tiles for `root` laid out in `rect`, level by level so every folder is subdivided down to
/// the Depth when it has room. The tile cap (a safety net) stops the layout mid-level: folders
/// reached earlier in the queue keep all their children, later ones get none.
#[cfg(test)]
fn layout(index: &Index, filter: &Filter, categories: &CategoryMap, root: Focus, rect: Rect, depth: u8) -> Vec<Tile> {
    let mut tiles = layout_tiles(index, filter, categories, root, rect, depth);
    colour_folders(&mut tiles, index, filter, categories);
    tiles
}

/// Folders take the colour of what is inside them: shown as a block, or as the tint behind
/// their children. Every tile learns its Owner kind here too.
fn colour_folders(tiles: &mut [Tile], index: &Index, filter: &Filter, categories: &CategoryMap) {
    for tile in tiles.iter_mut() {
        if tile.is_dir {
            tile.category = dominant_category(index, filter, categories, tile.id);
        }
        tile.owner = index.entry_owner(tile.id).map(|o| o.kind);
    }
}

/// [`layout`] without the folder colours (they cost the most, and need no consistent Index).
fn layout_tiles(index: &Index, filter: &Filter, categories: &CategoryMap, root: Focus, rect: Rect, depth: u8) -> Vec<Tile> {
    let mut tiles: Vec<Tile> = Vec::new();
    // A folder to subdivide: the folder, the area for its children, the tile that stands for it
    // and the level of its children.
    let mut queue = VecDeque::from([(root, rect, None::<usize>, 1u8)]);
    'layout: while let Some((dir, area, parent, level)) = queue.pop_front() {
        // `parent` is marked subdivided when its first child tile is pushed, below.
        let mut items: Vec<(f64, Option<EntryId>)> = dir
            .children(index)
            .iter()
            .filter(|&&c| filter.is_shown(c))
            .map(|&c| (filter.totals(index, c).on_disk as f64, Some(c)))
            .filter(|(size, _)| *size > 0.0)
            .collect();
        items.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        // The tail merges into one tile standing for its folder. The whole Index has no folder to
        // stand for, and keeps every Location.
        let mut more = None;
        let merged_into = dir.folder().filter(|_| items.len() > MAX_CHILDREN + 1);
        if merged_into.is_some() {
            let tail = items.split_off(MAX_CHILDREN);
            let size: f64 = tail.iter().map(|t| t.0).sum();
            more = Some((tail.len(), size as u64));
            items.push((size, None));
            items.sort_by(|a, b| b.0.total_cmp(&a.0));
        }
        for (cell, slot) in squarify(&items, area) {
            if cell.width() < MIN_TILE || cell.height() < MIN_TILE {
                continue;
            }
            if tiles.len() >= MAX_TILES {
                break 'layout;
            }
            let cell = snap(cell, area);
            if let Some(parent) = parent {
                tiles[parent].subdivided = true;
            }
            let id = match (slot, merged_into) {
                (Some(id), _) => id,
                (None, Some(folder)) => {
                    tiles.push(Tile {
                        rect: cell,
                        id: folder,
                        is_dir: false,
                        category: Category::Other,
                        owner: None,
                        subdivided: false,
                        level,
                        more,
                        parent,
                    });
                    continue;
                }
                (None, None) => unreachable!("only a merged tail has no Entry"),
            };
            let is_dir = index.is_dir(id);
            let category = if is_dir {
                Category::Other
            } else {
                categories.of_name(index.name(id))
            };
            tiles.push(Tile {
                rect: cell,
                id,
                is_dir,
                category,
                owner: None,
                subdivided: false,
                level,
                more: None,
                parent,
            });
            if is_dir && let Some(inner) = plan_folder(cell, level, depth) {
                queue.push_back((Focus::Folder(id), inner, Some(tiles.len() - 1), level + 1));
            }
        }
    }
    tiles
}

/// How many Entries under a folder are looked at to pick its colour.
const SAMPLE: usize = 200;

/// The Category with the most On-disk Size among the first files found under `dir`.
pub fn dominant_category(index: &Index, filter: &Filter, categories: &CategoryMap, dir: EntryId) -> Category {
    let mut bytes = [0u64; Category::ALL.len()];
    let mut stack = vec![dir];
    let mut seen = 0;
    while let Some(id) = stack.pop() {
        for child in index.children(id) {
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

/// Squarified treemap layout (Bruls, Huizing, van Wijk): splits `area` among `items`, which
/// must be sorted largest first, keeping rectangles close to square.
fn squarify<T: Copy>(items: &[(f64, T)], area: Rect) -> Vec<(Rect, T)> {
    let total: f64 = items.iter().map(|(w, _)| w).sum();
    if !(total > 0.0 && total.is_finite() && area.width() > 0.0 && area.height() > 0.0) {
        return Vec::new();
    }
    let scale = (area.width() as f64 * area.height() as f64) / total;
    let areas: Vec<f64> = items.iter().map(|(w, _)| w * scale).collect();
    let mut out = Vec::with_capacity(items.len());
    let mut free = area;
    let mut start = 0;
    while start < items.len() {
        let side = free.width().min(free.height()) as f64;
        // Rounding can leave a sliver of free space; what is left would be stretched over it.
        if side.is_nan() || side <= 1e-3 {
            break;
        }
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
            // Never outside the area, whatever the rounding did.
            let rect = Rect {
                x0: rect.x0.clamp(area.x0, area.x1),
                y0: rect.y0.clamp(area.y0, area.y1),
                x1: rect.x1.clamp(area.x0, area.x1),
                y1: rect.y1.clamp(area.y0, area.y1),
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
    let (min, max) = areas.iter().fold((f64::MAX, 0.0f64), |(lo, hi), &a| (lo.min(a), hi.max(a)));
    let side2 = side * side;
    let sum2 = sum * sum;
    (side2 * max / sum2).max(sum2 / (side2 * min))
}

/// Tiles bucketed by the grid cells they overlap, so a pointer position looks at a handful of
/// tiles instead of every one.
#[derive(Default)]
struct HitGrid {
    cols: usize,
    rows: usize,
    cells: Vec<Vec<u32>>,
}

/// Side of a grid cell in pixels.
const GRID_CELL: f32 = 32.0;

impl HitGrid {
    /// A grid over a layout area of `w` by `h` pixels; tiles are clipped to it.
    fn build(tiles: &[Tile], w: f32, h: f32) -> HitGrid {
        let (w, h) = (
            if w.is_finite() { w.clamp(1.0, 16384.0) } else { 1.0 },
            if h.is_finite() { h.clamp(1.0, 16384.0) } else { 1.0 },
        );
        let cols = (w / GRID_CELL).ceil().max(1.0) as usize;
        let rows = (h / GRID_CELL).ceil().max(1.0) as usize;
        let mut cells = vec![Vec::new(); cols * rows];
        for (i, t) in tiles.iter().enumerate() {
            let r = t.rect;
            if r.x1 <= r.x0 || r.y1 <= r.y0 {
                continue;
            }
            let col = |x: f32| ((x / GRID_CELL).floor().max(0.0) as usize).min(cols - 1);
            let row = |y: f32| ((y / GRID_CELL).floor().max(0.0) as usize).min(rows - 1);
            for cy in row(r.y0)..=row(r.y1) {
                for cx in col(r.x0)..=col(r.x1) {
                    cells[cy * cols + cx].push(i as u32);
                }
            }
        }
        HitGrid { cols, rows, cells }
    }

    /// The deepest tile under (`x`, `y`), as an index into `tiles`.
    fn hit(&self, tiles: &[Tile], x: f32, y: f32) -> Option<usize> {
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let (cx, cy) = ((x / GRID_CELL) as usize, (y / GRID_CELL) as usize);
        if cx >= self.cols || cy >= self.rows {
            return None;
        }
        // Children follow their parents, but siblings of different folders interleave because of
        // the work queue; pick the smallest containing tile, which is the deepest.
        self.cells[cy * self.cols + cx]
            .iter()
            .map(|&i| i as usize)
            .filter(|&i| tiles[i].rect.contains(x, y))
            .min_by(|&a, &b| tiles[a].rect.area().total_cmp(&tiles[b].rect.area()))
    }
}

/// The folders drawn around tile `i`, nearest first, as indices into `tiles`.
fn enclosing(tiles: &[Tile], i: usize) -> Vec<usize> {
    std::iter::successors(tiles[i].parent, |&p| tiles[p].parent).collect()
}

fn folder_of(index: &Index, id: EntryId) -> Option<EntryId> {
    if index.is_dir(id) { Some(id) } else { index.parent(id) }
}

/// What the Treemap draws for `view`: its focus while that folder is still there and shown,
/// else the whole Index.
pub fn zoom_root(index: &Index, view: &View) -> Focus {
    match view.focus {
        Focus::Folder(id) if index.is_live(id) && index.is_dir(id) && view.filter.is_shown(id) => view.focus,
        _ => Focus::Index,
    }
}

/// A tile's name: an Entry's, or for a Location its name, marked when it is an Offline Volume.
/// A Remote is never mounted, so it is never Offline.
fn tile_name(index: &Index, id: EntryId) -> Cow<'_, str> {
    let offline = index
        .volume_with_root(id)
        .and_then(|v| index.volume(v))
        .is_some_and(|v| !v.online && !v.is_remote());
    if offline {
        format!("{} (offline)", index.display_name(id)).into()
    } else {
        index.display_name(id).into()
    }
}

/// What a click on the Treemap asks of the rest of the Explorer.
pub enum TreemapEvent {
    /// A single click on a tile: select that Entry in the lists too.
    Select(EntryId),
    /// A double-click or a path crumb: zoom the Query into this folder, or out of any (None).
    Scope(Option<EntryId>),
    /// The Depth control moved: persist the new Depth.
    Depth(u8),
    /// The Colour by control moved: persist the new choice.
    ColourBy(ColourBy),
}

impl EventEmitter<TreemapEvent> for Treemap {}

pub struct Treemap {
    shared: Arc<Shared>,
    categories: CategoryMap,
    layouter: Layouter,
    view: Option<Arc<View>>,
    /// The Entry selected here or in a list; outlined on its tile or the nearest folder drawn.
    selected: Option<EntryId>,
    /// The rest of a multiple selection in a list, outlined more faintly.
    also: Vec<EntryId>,
    /// The newest finished tiles; may trail the current View by one layout.
    tiles: Rc<Vec<Tile>>,
    /// Spatial index over `tiles` for hit-testing.
    grid: Rc<HitGrid>,
    /// When the panel last painted; a panel unseen for `HIDDEN_AFTER` polls slowly and lays out nothing.
    last_paint: Instant,
    /// What `tiles` were laid out for; hit-testing is off while it trails the request.
    tiles_key: Option<LayoutKey>,
    /// The View and size last turned into a layout request.
    asked: Option<(Option<u64>, [i32; 2], u8)>,
    /// How many levels of folders are subdivided; see `plan_folder`.
    depth: u8,
    /// What the tiles are coloured by.
    colour_by: ColourBy,
    /// The drawing area in window coordinates, as of the last paint.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The index of the tile under the pointer, with the pointer's position inside the drawing area.
    hovered: Option<(usize, Point<Pixels>)>,
    /// Length of a transition; zero under Reduce motion. Set on every render and poll.
    motion: Duration,
    /// The zoom being drawn, until it settles.
    transition: Option<Transition>,
    /// The Entry whose tile is hovered, since when, and the outlines drawn for it last frame.
    hover_id: Option<EntryId>,
    hover_since: Instant,
    last_outlines: Vec<(Rect, Hsla)>,
    /// The outlines of the previously hovered tile, fading out since the instant given.
    ghost: Option<(Vec<(Rect, Hsla)>, Instant)>,
}

impl Treemap {
    pub fn new(shared: Arc<Shared>, categories: CategoryMap, depth: u8, colour_by: ColourBy, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            let mut wait = POLL;
            loop {
                cx.background_executor().timer(wait).await;
                match this.update(cx, |this, cx| this.poll(cx)) {
                    Ok(next) => wait = next,
                    Err(_) => break,
                }
            }
        })
        .detach();
        Self {
            layouter: Layouter::start(shared.clone(), categories.clone()),
            shared,
            categories,
            view: None,
            selected: None,
            also: Vec::new(),
            tiles: Rc::new(Vec::new()),
            grid: Rc::default(),
            last_paint: Instant::now() - HIDDEN_AFTER * 2,
            tiles_key: None,
            asked: None,
            depth: depth.clamp(MIN_DEPTH, MAX_DEPTH),
            colour_by,
            bounds: Rc::default(),
            hovered: None,
            motion: Duration::ZERO,
            transition: None,
            hover_id: None,
            hover_since: Instant::now(),
            last_outlines: Vec::new(),
            ghost: None,
        }
    }

    fn set_depth(&mut self, depth: u8, cx: &mut Context<Self>) {
        let depth = depth.clamp(MIN_DEPTH, MAX_DEPTH);
        if depth != self.depth {
            self.depth = depth;
            self.hovered = None;
            cx.emit(TreemapEvent::Depth(depth));
            cx.notify();
        }
    }

    fn set_colour_by(&mut self, colour_by: ColourBy, cx: &mut Context<Self>) {
        if colour_by != self.colour_by {
            self.colour_by = colour_by;
            cx.emit(TreemapEvent::ColourBy(colour_by));
            cx.notify();
        }
    }

    /// Outlines `id`, selected in a list; None clears the outline.
    pub fn set_selected(&mut self, id: Option<EntryId>, cx: &mut Context<Self>) {
        if self.selected != id {
            self.selected = id;
            cx.notify();
        }
    }

    /// Outlines the other Entries of a multiple selection; empty clears them.
    pub fn set_also_selected(&mut self, ids: Vec<EntryId>, cx: &mut Context<Self>) {
        if self.also != ids {
            self.also = ids;
            cx.notify();
        }
    }

    /// One look for a new View or finished layout; the wait until the next one.
    fn poll(&mut self, cx: &mut Context<Self>) -> Duration {
        self.motion = crate::motion::duration(cx);
        let visible = self.last_paint.elapsed() <= HIDDEN_AFTER;
        let changed = self.sync();
        if changed {
            cx.notify();
        }
        if visible || self.tiles_key != self.layouter.requested {
            POLL
        } else {
            IDLE_POLL
        }
    }

    /// Takes the newest View and layout, and asks for a layout when the View, size or Depth
    /// changed (only while the panel is on screen). True when something changed.
    fn sync(&mut self) -> bool {
        let mut changed = false;
        let latest = self.shared.view.lock().clone();
        if !crate::view::same(&self.view, &latest) {
            self.view = latest;
            changed = true;
        }
        let size = self.bounds.get().size;
        let (w, h) = (f32::from(size.width), f32::from(size.height));
        // Looking up the root locks the Index, so only do it when something it depends on moved.
        let asked = (self.view.as_ref().map(|v| v.id), [w.round() as i32, h.round() as i32], self.depth);
        if let Some(view) = &self.view
            && self.last_paint.elapsed() <= HIDDEN_AFTER
            && w > 0.0
            && h > 0.0
            && self.asked != Some(asked)
        {
            self.asked = Some(asked);
            let root = zoom_root(&self.shared.index.read(), view);
            self.layouter.request(view, root, (w, h), self.depth);
        }
        if let Some((key, tiles)) = self.layouter.results.try_iter().last() {
            // Zooming (same size and Depth, another root) glides from the old tiles to the new.
            self.transition = self
                .tiles_key
                .filter(|old| !self.motion.is_zero() && old.1 != key.1 && old.2 == key.2 && old.3 == key.3)
                .filter(|_| !self.tiles.is_empty() && !tiles.is_empty())
                .map(|_| Transition {
                    from: Rc::new(zoom_from(&self.tiles, &tiles, key.1)),
                    started: Instant::now(),
                });
            self.grid = Rc::new(HitGrid::build(&tiles, key.2[0] as f32, key.2[1] as f32));
            self.tiles = Rc::new(tiles);
            self.tiles_key = Some(key);
            self.hovered = None;
            // The outlines of the old tiles mean nothing on the new ones: no fade-out ghost.
            self.last_outlines.clear();
            self.ghost = None;
            // Screenshots (VE_DEMO): rest the pointer on the second tile.
            #[cfg(feature = "bench")]
            if std::env::var_os("VE_DEMO").is_some() {
                self.hovered = self
                    .tiles
                    .iter()
                    .position(|t| !t.subdivided && t.rect.width() > 60. && t.rect.height() > 50.)
                    .map(|i| (i, point(px(self.tiles[i].rect.x0 + 20.), px(self.tiles[i].rect.y0 + 40.))));
            }
            changed = true;
        }
        changed
    }

    /// Zooms the Query into `folder`, or out of any folder.
    fn scope(&mut self, folder: Option<EntryId>, cx: &mut Context<Self>) {
        self.hovered = None;
        cx.emit(TreemapEvent::Scope(folder));
    }

    /// The tile at a window position, if it is over the drawing area.
    fn tile_at(&self, position: Point<Pixels>) -> Option<(usize, Point<Pixels>)> {
        if self.tiles_key != self.layouter.requested || self.transition.is_some() {
            return None;
        }
        let local = position - self.bounds.get().origin;
        self.grid
            .hit(&self.tiles, f32::from(local.x), f32::from(local.y))
            .map(|i| (i, local))
    }

    /// Writes the tiles as drawn to a PNG file: the panel's colours, without the text labels.
    fn export_png(&self, window: &mut Window, cx: &mut Context<Self>) {
        let size = self.bounds.get().size;
        let (w, h) = (f32::from(size.width), f32::from(size.height));
        if self.tiles.is_empty() || w < 1. || h < 1. {
            window.push_notification(Notification::warning("Nothing to export yet."), cx);
            return;
        }
        let p = palette(cx);
        let rgb = |c: Hsla| {
            let c = Rgba::from(c);
            [c.r, c.g, c.b].map(|v| (v * 255.).round() as u8)
        };
        let free = rgb(p.free);
        // Subdivided folders show only as the gaps between their children: the background.
        let hues = self.hues(&p);
        let blocks: Vec<_> = self
            .tiles
            .iter()
            .zip(&hues)
            .filter(|(t, _)| !t.subdivided)
            .flat_map(|(t, &hue)| {
                let r = t.rect;
                let inner = t.visible();
                [
                    crate::export::Block {
                        rect: [r.x0, r.y0, r.x1, r.y1],
                        rgb: free,
                    },
                    crate::export::Block {
                        rect: [inner.x0, inner.y0, inner.x1, inner.y1],
                        rgb: rgb(hue),
                    },
                ]
            })
            .collect();
        crate::export::save(window, cx, "treemap.png", move |path| {
            std::fs::write(path, crate::export::encode_png(w, h, free, &blocks)?)
        });
    }

    /// Panel title, then the path of what is shown: Index / home / mohammed 109 GB.
    fn header(&self, index: &Index, view: &View, root: Focus, cx: &mut Context<Self>) -> Div {
        let (muted, text) = (palette(cx).muted, palette(cx).text);
        let chain: Vec<EntryId> = std::iter::successors(root.folder(), |&id| index.parent(id)).collect();
        // On a deep path the folders above the last shrink and truncate; the last one and the
        // size always show.
        let crumb = |label: String, last: bool| {
            div()
                .whitespace_nowrap()
                .when(last, |d| d.flex_none().text_color(text).font_weight(FontWeight::MEDIUM))
                .when(!last, |d| d.min_w_0().truncate().text_color(muted))
                .child(label)
        };
        let mut row = h_flex()
            .gap(px(6.))
            .items_center()
            .min_w_0()
            .overflow_hidden()
            .text_xs()
            .text_color(muted)
            .mb(px(12.));
        row = row.child(
            crumb("Index".into(), chain.is_empty())
                .flex_none()
                .id("crumb-index")
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.scope(None, cx))),
        );
        for (i, &id) in chain.iter().rev().enumerate() {
            row = row.child("/").child(
                crumb(index.display_name(id).to_owned(), i + 1 == chain.len())
                    .id(("crumb", id.0 as usize))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.scope(Some(id), cx))),
            );
        }
        let title = h_flex()
            .gap(px(12.))
            .items_center()
            .min_w_0()
            .mb(px(14.))
            .child(
                div()
                    .flex_none()
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .child("Treemap"),
            )
            .child(
                caption("Filtered Size · double-click a folder to zoom", cx)
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .truncate(),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .items_center()
                    .child(caption("Depth", cx))
                    .child(
                        pill("depth-less", "−")
                            .small()
                            .build(cx, cx.listener(|this, _, _, cx| this.set_depth(this.depth.saturating_sub(1), cx))),
                    )
                    .child(div().w(px(12.)).text_xs().text_color(text).child(self.depth.to_string()))
                    .child(
                        pill("depth-more", "+")
                            .small()
                            .build(cx, cx.listener(|this, _, _, cx| this.set_depth(this.depth + 1, cx))),
                    )
                    .child(
                        pill("export-png", "Export PNG")
                            .small()
                            .build(cx, cx.listener(|this, _, window, cx| this.export_png(window, cx))),
                    ),
            );
        v_flex().child(title).child(
            row.child(
                div()
                    .flex_none()
                    .ml(px(6.))
                    .child(format::size(root.totals(index, &view.filter).on_disk)),
            ),
        )
    }

    /// Legend of the colours the tiles show, as `.legend`, with the Colour by control at its end.
    fn legend(&self, cx: &mut Context<Self>) -> Div {
        let p = palette(cx);
        let mut row = h_flex()
            .flex_wrap()
            .gap_x(px(14.))
            .gap_y(px(4.))
            .mt(px(12.))
            .text_xs()
            .text_color(p.muted);
        let item = |color: Hsla, name: &'static str| h_flex().gap(px(6.)).items_center().child(swatch(color)).child(name);
        match self.colour_by {
            ColourBy::Category => {
                let mut seen = [false; Category::ALL.len()];
                for tile in self.tiles.iter().filter(|t| !t.subdivided && t.more.is_none()) {
                    seen[tile.category as usize] = true;
                }
                for category in Category::ALL.into_iter().filter(|c| seen[*c as usize]) {
                    row = row.child(item(p.category(category), category.name()));
                }
            }
            ColourBy::OwnerKind => {
                for kind in OwnerKind::ALL {
                    row = row.child(item(p.owner_kind(Some(kind)), kind.name()));
                }
                row = row.child(item(p.owner_kind(None), "No Owner"));
            }
        }
        row.child(
            h_flex()
                .ml_auto()
                .gap(px(6.))
                .items_center()
                .child(caption("Colour by", cx))
                .child(
                    pill("colour-category", "Category")
                        .small()
                        .selected(self.colour_by == ColourBy::Category)
                        .build(cx, cx.listener(|this, _, _, cx| this.set_colour_by(ColourBy::Category, cx))),
                )
                .child(
                    pill("colour-owner-kind", "Owner kind")
                        .small()
                        .selected(self.colour_by == ColourBy::OwnerKind)
                        .build(cx, cx.listener(|this, _, _, cx| this.set_colour_by(ColourBy::OwnerKind, cx))),
                ),
        )
    }

    /// The colour of every tile under the active colouring (a folder's Owner kind is exact, not sampled).
    fn hues(&self, p: &Palette) -> Vec<Hsla> {
        self.tiles
            .iter()
            .map(|t| match self.colour_by {
                ColourBy::Category => p.category(t.category),
                ColourBy::OwnerKind => p.owner_kind(t.owner),
            })
            .collect()
    }

    /// The pointer's tooltip (`.tip`): inverted; the Volume-relative path as a breadcrumb, the
    /// tile's sizes and share of the zoomed folder, then the folders around it with theirs.
    fn tooltip(&self, index: &Index, view: &View, root: Focus, tile: usize, at: Point<Pixels>, cx: &App) -> Div {
        let id = self.tiles[tile].id;
        let totals = view.filter.totals(index, id);
        let whole = root.totals(index, &view.filter).on_disk.max(1);
        let share = |on_disk: u64| on_disk as f64 * 100.0 / whole as f64;
        let mut names: Vec<&str> = std::iter::successors(Some(id), |&e| index.parent(e))
            .map(|e| index.display_name(e))
            .collect();
        names.reverse();
        let crumb = names.join(" › ");
        let more = self.tiles[tile].more;
        let first = if let Some((count, size)) = more {
            format!("{} on disk · {} smaller items", format::size(size), format::count(count as u64))
        } else if index.is_dir(id) {
            format!("{} on disk · {} files", format::size(totals.on_disk), format::count(totals.files))
        } else {
            format!(
                "{} on disk · {}",
                format::size(totals.on_disk),
                self.categories.of_name(index.name(id)).name()
            )
        };
        let second = match more {
            Some((_, size)) => format!("{:.1}% of {}", share(size), root.name(index)),
            None => format!(
                "{} size · {:.1}% of {}",
                format::size(totals.logical),
                share(totals.on_disk),
                root.name(index)
            ),
        };
        let around: Vec<String> = enclosing(&self.tiles, tile)
            .into_iter()
            .rev()
            .map(|i| {
                let t = &self.tiles[i];
                let on_disk = view.filter.totals(index, t.id).on_disk;
                format!("{} · {} · {:.1}%", tile_name(index, t.id), format::size(on_disk), share(on_disk))
            })
            .collect();
        let p = palette(cx);
        // Opens right of and below the pointer, flipping to the other side near an edge.
        let surface = self.bounds.get().size;
        let tip = v_flex().absolute().max_w(px(460.));
        let tip = if at.x + px(280.) > surface.width {
            tip.right(surface.width - at.x + px(14.))
        } else {
            tip.left(at.x + px(14.))
        };
        let tip = if at.y + px(130.) > surface.height {
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
            .child(div().opacity(0.8).child(crumb))
            .child(div().font_weight(FontWeight::MEDIUM).child(match more {
                Some((count, _)) => format!("{} more", format::count(count as u64)),
                None => tile_name(index, id).into_owned(),
            }))
            .child(first)
            .child(second)
            .children(index.entry_owner(id).map(|owner| {
                let detail = if owner.detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", owner.detail)
                };
                div().child(format!("Owner: {} · {}{detail}", owner.name, owner.kind.name()))
            }))
            .children(around.into_iter().map(|line| div().opacity(0.8).child(format!("in {line}"))))
    }
}

impl Render for Treemap {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = crate::trace::span("Treemap::render");
        // A repaint is what the engine and the layout thread ask for after publishing, so this is
        // where the panel takes up a new View or finished layout.
        self.last_paint = Instant::now();
        self.motion = crate::motion::duration(cx);
        self.sync();
        // A zoom in progress, as how far along it is; it asks for frames only until it settles.
        let zoom = match &self.transition {
            Some(t) if fraction(t.started, self.motion) < 1.0 => {
                window.request_animation_frame();
                Some((t.from.clone(), crate::motion::ease_out(fraction(t.started, self.motion))))
            }
            _ => None,
        };
        if zoom.is_none() {
            self.transition = None;
        }
        let animating = zoom.is_some();
        // The pointer may have left without a mouse event reaching the panel (`poll` keeps
        // re-rendering while something is hovered, so this check runs).
        if self.hovered.is_some() && !self.bounds.get().contains(&window.mouse_position()) {
            self.hovered = None;
        }
        let p = palette(cx);
        let (free, ink, text, muted) = (p.free, p.tileink, p.text, p.muted);
        let panel = crate::ui::panel(cx).size_full().flex().flex_col();
        let shared = self.shared.clone();
        let index = shared.index.read();
        let Some(view) = self.view.clone().filter(|_| index.volumes().next().is_some()) else {
            return panel.child(div().text_color(muted).child("No Volume indexed yet."));
        };
        let root = zoom_root(&index, &view);

        let tiles = self.tiles.clone();
        let hues = self.hues(&p);
        // The selected Entry's own tile, else the nearest folder of it that is drawn.
        let selected = self.selected.filter(|&id| index.is_live(id)).and_then(|id| {
            let chain: Vec<EntryId> = std::iter::successors(Some(id), |&e| index.parent(e)).collect();
            self.tiles
                .iter()
                .filter(|t| t.more.is_none())
                .filter_map(|t| chain.iter().position(|&c| c == t.id).map(|depth| (depth, t.id)))
                .min()
                .map(|(_, tile)| tile)
        });
        // Tiles of the rest of the selection: each Entry's own tile, else its nearest drawn folder.
        let also: Vec<usize> = if self.also.is_empty() {
            Vec::new()
        } else {
            let by_id: HashMap<EntryId, usize> = self
                .tiles
                .iter()
                .enumerate()
                .filter(|(_, t)| t.more.is_none())
                .map(|(i, t)| (t.id, i))
                .collect();
            let mut found: Vec<usize> = self
                .also
                .iter()
                .filter(|&&id| index.is_live(id))
                .filter_map(|&id| std::iter::successors(Some(id), |&e| index.parent(e)).find_map(|e| by_id.get(&e).copied()))
                .collect();
            found.sort_unstable();
            found.dedup();
            found
        };
        let bounds = self.bounds.clone();
        let outlines: Vec<(Rect, Hsla)> = match self.hovered.filter(|&(i, _)| i < self.tiles.len() && !animating) {
            Some((i, _)) => enclosing(&self.tiles, i)
                .into_iter()
                .map(|a| (self.tiles[a].rect, p.accent))
                .chain([(self.tiles[i].rect, text)])
                .collect(),
            None => Vec::new(),
        };
        // Outlines fade in on the tile just hovered and out on the one just left.
        let hover_id = self
            .hovered
            .filter(|&(i, _)| i < self.tiles.len() && !animating)
            .map(|(i, _)| self.tiles[i].id);
        if hover_id != self.hover_id {
            let left = std::mem::take(&mut self.last_outlines);
            self.ghost = (!self.motion.is_zero() && !left.is_empty()).then(|| (left, Instant::now()));
            self.hover_id = hover_id;
            self.hover_since = Instant::now();
        }
        self.last_outlines.clone_from(&outlines);
        let fade_in = crate::motion::ease_out(fraction(self.hover_since, self.motion));
        let fade_out = self
            .ghost
            .as_ref()
            .map(|(rects, since)| (rects.clone(), 1.0 - crate::motion::ease_out(fraction(*since, self.motion))));
        if self.ghost.as_ref().is_some_and(|(_, since)| fraction(*since, self.motion) >= 1.0) {
            self.ghost = None;
        }
        if (hover_id.is_some() && fade_in < 1.0) || self.ghost.is_some() {
            window.request_animation_frame();
        }
        let selected = selected.filter(|_| !animating);

        let surface = canvas(
            move |b, _, _| bounds.set(b),
            move |b, _, window, _| {
                let scale = window.scale_factor();
                let snap = |v: f32| (v * scale).round() / scale;
                let at = |r: Rect| {
                    let (x0, y0, x1, y1) = (snap(r.x0), snap(r.y0), snap(r.x1), snap(r.y1));
                    Bounds::new(b.origin + point(px(x0), px(y0)), size(px(x1 - x0), px(y1 - y0)))
                };
                let dark = free.l < 0.5;
                for (i, tile) in tiles.iter().enumerate() {
                    let step = f32::from(tile.level.saturating_sub(1).min(4));
                    let color = if tile.more.is_some() {
                        mix(free, muted, 0.3)
                    } else if tile.subdivided {
                        // Shows between the children: a faint tint of the folder's Category.
                        mix(free, hues[i], 0.14 + 0.025 * step)
                    } else {
                        shade(hues[i], tile.level, dark)
                    };
                    // During a zoom the tile is between where it was and where it is.
                    let (cell, opacity) = match &zoom {
                        Some((from, t)) => {
                            let (start, alpha) = from[i];
                            (start.lerp(tile.rect, *t), alpha + (1.0 - alpha) * t)
                        }
                        None => (tile.rect, 1.0),
                    };
                    let vis = cell.shrink(if tile.parent.is_none() { GAP_TOP } else { GAP });
                    if vis.width() <= 0.0 || vis.height() <= 0.0 {
                        continue;
                    }
                    let radius = if !tile.subdivided && vis.width() > 24.0 && vis.height() > 24.0 {
                        2.
                    } else {
                        0.
                    };
                    window.paint_quad(quad(at(vis), radius, color.opacity(opacity), px(0.), free, BorderStyle::Solid));
                    // A strip in the folder's colour across the top of each top-level folder.
                    if tile.subdivided && tile.parent.is_none() {
                        let strip = Rect { y1: vis.y0 + 2.0, ..vis };
                        window.paint_quad(fill(at(strip), hues[i].opacity(opacity)));
                    }
                }
                let clear = gpui_kit::transparent_black();
                for &i in &also {
                    window.paint_quad(quad(
                        at(tiles[i].rect.shrink(1.)),
                        4.,
                        clear,
                        px(2.),
                        text.opacity(0.55),
                        BorderStyle::Solid,
                    ));
                }
                if let Some(tile) = selected.and_then(|s| tiles.iter().find(|t| t.id == s)) {
                    window.paint_quad(quad(at(tile.rect.shrink(1.)), 4., clear, px(2.), text, BorderStyle::Solid));
                }
                // Hovered tile and every folder around it, outermost faintest.
                for (rect, ink) in outlines.iter() {
                    window.paint_quad(quad(
                        at(rect.shrink(0.5)),
                        0.,
                        clear,
                        px(1.5),
                        ink.opacity(fade_in),
                        BorderStyle::Solid,
                    ));
                }
                if let Some((rects, alpha)) = &fade_out {
                    for (rect, ink) in rects.iter() {
                        window.paint_quad(quad(
                            at(rect.shrink(0.5)),
                            0.,
                            clear,
                            px(1.5),
                            ink.opacity(*alpha),
                            BorderStyle::Solid,
                        ));
                    }
                }
            },
        )
        .size_full();

        // Names and sizes on the tiles that have room for them, shallowest level first.
        let mut labels = Vec::new();
        for tile in self
            .tiles
            .iter()
            .filter(|t| !animating && !t.subdivided && t.more.is_none() && t.rect.width() > 70.0 && t.rect.height() > 40.0)
        {
            if labels.len() >= 60 || !index.is_live(tile.id) {
                continue;
            }
            let parent = index
                .parent(tile.id)
                .filter(|&p| Some(p) != root.folder() && !tile.is_dir)
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
                            .child(tile_name(&index, tile.id).into_owned()),
                    )
                    .child(div().flex_none().child(format::size(on_disk))),
            );
        }

        // Header bands: the name and size of each subdivided folder.
        for tile in self.tiles.iter().filter(|t| t.subdivided && !animating).take(400) {
            if !index.is_live(tile.id) {
                continue;
            }
            let on_disk = view.filter.totals(&index, tile.id).on_disk;
            let vis = tile.visible();
            labels.push(
                h_flex()
                    .absolute()
                    .left(px(vis.x0))
                    .top(px(vis.y0))
                    .w(px(vis.width()))
                    .h(px(header_h(tile.level)))
                    .overflow_hidden()
                    .px(px(6.))
                    .pt(px(if tile.level <= 1 { 2. } else { 0. }))
                    .gap(px(6.))
                    .items_center()
                    .text_size(px(if tile.level <= 1 { 11. } else { 10. }))
                    .text_color(text)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(FontWeight::MEDIUM)
                            .whitespace_nowrap()
                            .truncate()
                            .child(tile_name(&index, tile.id).into_owned()),
                    )
                    .when(vis.width() >= 110.0, |band| {
                        band.child(div().flex_none().text_color(muted).child(format::size(on_disk)))
                    }),
            );
        }

        let tooltip = self
            .hovered
            .filter(|&(i, _)| i < self.tiles.len() && index.is_live(self.tiles[i].id))
            .map(|(i, at)| self.tooltip(&index, &view, root, i, at, cx));
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
                        let Some((i, _)) = this.tile_at(e.position()) else {
                            return;
                        };
                        let id = this.tiles[i].id;
                        if e.click_count() >= 2 {
                            let shared = this.shared.clone();
                            let folder = folder_of(&shared.index.read(), id);
                            if folder.is_some() {
                                this.scope(folder, cx);
                            }
                        } else {
                            this.selected = Some(id);
                            cx.emit(TreemapEvent::Select(id));
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
    fn squarified_tiles_stay_inside_the_area_however_skewed_the_sizes() {
        let area = rect(1403.7, 901.3);
        let mut seed = 12345u64;
        for round in 0..200 {
            let n = 2 + round * 7;
            let items: Vec<(f64, usize)> = {
                let mut v: Vec<(f64, usize)> = (0..n)
                    .map(|i| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        (((seed >> 40) as f64 + 1.0).powi(if round % 2 == 0 { 3 } else { 1 }), i)
                    })
                    .collect();
                v.sort_by(|a, b| b.0.total_cmp(&a.0));
                v
            };
            for (r, _) in squarify(&items, area) {
                assert!([r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite()));
                assert!(r.x0 >= area.x0 && r.y0 >= area.y0 && r.x1 <= area.x1 && r.y1 <= area.y1);
            }
        }
        assert!(squarify(&[(1.0, 0)], rect(f32::NAN, 10.0)).is_empty());
    }

    #[::core::prelude::v1::test]
    fn a_grid_stays_bounded_whatever_a_tile_claims() {
        let huge = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: f32::INFINITY,
            y1: 1e30,
        };
        let tile = Tile {
            rect: huge,
            id: EntryId(1),
            is_dir: false,
            category: Category::Other,
            owner: None,
            subdivided: false,
            level: 1,
            more: None,
            parent: None,
        };
        let grid = HitGrid::build(&[tile], 200.0, 100.0);
        assert!(grid.cells.len() <= 7 * 4);
    }

    #[::core::prelude::v1::test]
    fn hit_finds_the_deepest_tile() {
        let outer = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 100.0,
            y1: 100.0,
        };
        let tile = |id, rect, is_dir, parent| Tile {
            rect,
            id: EntryId(id),
            is_dir,
            category: Category::Other,
            owner: None,
            subdivided: is_dir,
            level: 1,
            more: None,
            parent,
        };
        let tiles = vec![tile(1, outer, true, None), tile(2, outer.shrink(10.0), false, Some(0))];
        let grid = HitGrid::build(&tiles, 100.0, 100.0);
        assert_eq!(grid.hit(&tiles, 50.0, 50.0), Some(1));
        assert_eq!(grid.hit(&tiles, 5.0, 5.0), Some(0));
        assert!(grid.hit(&tiles, 150.0, 5.0).is_none());
        assert_eq!(enclosing(&tiles, 1), vec![0]);
        assert!(enclosing(&tiles, 0).is_empty());
    }

    fn rect(w: f32, h: f32) -> Rect {
        Rect {
            x0: 0.0,
            y0: 0.0,
            x1: w,
            y1: h,
        }
    }

    #[::core::prelude::v1::test]
    fn folders_past_the_depth_are_one_block() {
        assert!(plan_folder(rect(200.0, 200.0), 3, 3).is_some());
        assert!(plan_folder(rect(200.0, 200.0), 4, 3).is_none());
        assert!(plan_folder(rect(200.0, 200.0), 1, 1).is_some());
        assert!(plan_folder(rect(200.0, 200.0), 2, 1).is_none());
    }

    #[::core::prelude::v1::test]
    fn header_needs_width_and_a_body_under_the_band() {
        // Top level: 20 px band, 1.5 px gap each side, at least 15 px of body.
        let tall = 2.0 * GAP_TOP + header_h(1) + MIN_BODY_H;
        let body = plan_folder(rect(MIN_HEADER_W, tall), 1, 3).unwrap();
        assert_eq!(body.y0, GAP_TOP + header_h(1));
        assert_eq!(body.y1, tall - GAP_TOP);
        assert!(plan_folder(rect(MIN_HEADER_W - 0.1, tall), 1, 3).is_none());
        assert!(plan_folder(rect(MIN_HEADER_W, tall - 0.1), 1, 3).is_none());
        // Deeper folders have a shorter band and a thinner gap.
        let tall = 2.0 * GAP + header_h(2) + MIN_BODY_H;
        assert!(plan_folder(rect(MIN_HEADER_W, tall), 2, 3).is_some());
        assert!(plan_folder(rect(MIN_HEADER_W, tall - 0.1), 2, 3).is_none());
    }

    fn world(build: impl FnOnce(&mut Index, EntryId)) -> (Index, EntryId) {
        use ve_core::{EntryMeta, SyncState};
        let dir = EntryMeta {
            is_dir: true,
            ..EntryMeta::default()
        };
        let mut index = Index::new();
        let vol = index.add_volume(std::path::Path::new("/m/v"), "ext4", "v", dir, SyncState::Walk);
        let root = index.volume(vol).unwrap().root;
        build(&mut index, root);
        (index, root)
    }

    fn file(size: u64) -> ve_core::EntryMeta {
        ve_core::EntryMeta {
            logical: size,
            on_disk: size,
            ..Default::default()
        }
    }

    fn lay(index: &Index, root: EntryId, w: f32, h: f32, depth: u8) -> Vec<Tile> {
        let filter = Filter::compute(index, None);
        layout(index, &filter, &CategoryMap::default(), Focus::Folder(root), rect(w, h), depth)
    }

    #[::core::prelude::v1::test]
    fn every_folder_with_room_is_subdivided_to_the_depth() {
        let dir = ve_core::EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let (index, root) = world(|index, root| {
            for a in 0..5 {
                let mut parent = index.insert(root, &format!("d{a}"), dir);
                for level in 0..5 {
                    index.insert(parent, "f.bin", file(10_000));
                    parent = index.insert(parent, &format!("n{level}"), dir);
                }
                index.insert(parent, "leaf.bin", file(10_000));
            }
        });
        let tiles = lay(&index, root, 1600.0, 1000.0, 3);
        let dirs: Vec<&Tile> = tiles.iter().filter(|t| t.is_dir).collect();
        assert!(dirs.iter().any(|t| t.level == 3));
        for t in &dirs {
            assert_eq!(t.subdivided, t.level <= 3, "level {}", t.level);
        }
        // Parents come before their children, and children lie inside them.
        for (i, t) in tiles.iter().enumerate() {
            if let Some(p) = t.parent {
                assert!(p < i);
                let (o, r) = (tiles[p].rect, t.rect);
                assert!(r.x0 >= o.x0 && r.y0 >= o.y0 && r.x1 <= o.x1 && r.y1 <= o.y1);
            }
        }
    }

    #[::core::prelude::v1::test]
    fn the_whole_index_draws_one_tile_per_location_by_its_size() {
        let (mut index, a) = world(|index, root| {
            index.insert(root, "big.bin", file(3000));
        });
        let dir = ve_core::EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let other = index.add_volume(std::path::Path::new("/m/w"), "ext4", "w", dir, ve_core::SyncState::Walk);
        let b = index.volume(other).unwrap().root;
        index.insert(b, "small.bin", file(1000));
        let filter = Filter::compute(&index, None);
        let tiles = layout(&index, &filter, &CategoryMap::default(), Focus::Index, rect(400.0, 100.0), 3);
        let top: Vec<(EntryId, f32)> = tiles.iter().filter(|t| t.parent.is_none()).map(|t| (t.id, t.rect.area())).collect();
        assert_eq!(top.iter().map(|t| t.0).collect::<Vec<_>>(), vec![a, b]);
        // Each Location's tile is subdivided into what it holds, sized 3:1.
        assert!((top[0].1 / top[1].1 - 3.0).abs() < 0.1);
        assert!(tiles.iter().any(|t| t.parent == Some(0) && !t.is_dir));
    }

    #[::core::prelude::v1::test]
    fn the_tail_of_a_big_folder_merges_into_one_tile() {
        let (index, root) = world(|index, root| {
            for i in 0..150u64 {
                index.insert(root, &format!("f{i}.bin"), file((i + 1) * 1000));
            }
        });
        let tiles = lay(&index, root, 3000.0, 3000.0, 3);
        let merged: Vec<&Tile> = tiles.iter().filter(|t| t.more.is_some()).collect();
        assert_eq!(merged.len(), 1);
        let tail: u64 = (1..=54u64).map(|i| i * 1000).sum();
        assert_eq!(merged[0].more, Some((54, tail)));
        assert_eq!(merged[0].id, root);
        assert_eq!(tiles.iter().filter(|t| t.more.is_none()).count(), MAX_CHILDREN);
    }

    #[::core::prelude::v1::test]
    fn slivers_are_dropped_and_nothing_is_not_a_number() {
        let (index, root) = world(|index, root| {
            index.insert(root, "big.bin", file(1_000_000_000_000));
            index.insert(root, "tiny.bin", file(1));
        });
        let tiles = lay(&index, root, 300.0, 300.0, 3);
        assert_eq!(tiles.len(), 1);
        let (index, root) = world(|_, _| {});
        assert!(lay(&index, root, 300.0, 300.0, 3).is_empty());
        for t in lay(&index, root, f32::NAN, 10.0, 3) {
            assert!([t.rect.x0, t.rect.y0, t.rect.x1, t.rect.y1].iter().all(|v| v.is_finite()));
        }
    }

    #[::core::prelude::v1::test]
    fn a_huge_tile_is_clamped_into_the_edge_cells() {
        let tile = Tile {
            rect: Rect {
                x0: -1e30,
                y0: 0.0,
                x1: f32::INFINITY,
                y1: 1e30,
            },
            id: EntryId(1),
            is_dir: false,
            category: Category::Other,
            owner: None,
            subdivided: false,
            level: 1,
            more: None,
            parent: None,
        };
        let grid = HitGrid::build(&[tile], 200.0, 100.0);
        assert_eq!((grid.cols, grid.rows), (7, 4));
        assert!(grid.cells.iter().all(|c| c == &[0]), "every cell holds the one tile");
    }

    #[::core::prelude::v1::test]
    fn laid_out_edges_are_whole_pixels() {
        let (index, root) = world(|index, root| {
            for i in 0..40u64 {
                index.insert(root, &format!("f{i}.bin"), file((i + 1) * 777));
            }
        });
        for t in lay(&index, root, 1003.0, 701.0, 3) {
            for v in [t.rect.x0, t.rect.y0, t.rect.x1, t.rect.y1] {
                assert_eq!(v, v.round(), "{v}");
            }
        }
    }

    #[::core::prelude::v1::test]
    fn zoom_starts_tiles_from_where_they_were_or_the_folder_they_grow_out_of() {
        let tile = |id: u32, rect: Rect, more: Option<(usize, u64)>| Tile {
            rect,
            id: EntryId(id),
            is_dir: false,
            category: Category::Other,
            owner: None,
            subdivided: false,
            level: 1,
            more,
            parent: None,
        };
        let (a, b) = (rect(100.0, 100.0), rect(50.0, 50.0));
        let old = vec![tile(1, a, None), tile(2, b, None)];
        // Zooming into 1: 1 itself and 2 start where they were; 9 is new and grows out of 1.
        let new = vec![
            tile(2, rect(10.0, 10.0), None),
            tile(9, rect(20.0, 20.0), None),
            tile(7, rect(30.0, 30.0), Some((3, 5))),
        ];
        let from = zoom_from(&old, &new, Focus::Folder(EntryId(1)));
        assert_eq!(from, vec![(b, 1.0), (a, 1.0), (a, 1.0)]);
        // Zooming out to a root that was not drawn: unknown tiles fade in where they are.
        let from = zoom_from(&old, &new, Focus::Folder(EntryId(5)));
        assert_eq!(from, vec![(b, 1.0), (rect(20.0, 20.0), 0.0), (rect(30.0, 30.0), 0.0)]);
    }

    #[::core::prelude::v1::test]
    fn the_tile_cap_stops_the_layout_and_keeps_tiles_consistent() {
        let dir = ve_core::EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let (index, root) = world(|index, root| {
            for d in 0..96 {
                let folder = index.insert(root, &format!("d{d}"), dir);
                for e in 0..96 {
                    let inner = index.insert(folder, &format!("e{e}"), dir);
                    for f in 0..2u64 {
                        index.insert(inner, &format!("f{f}.bin"), file(10_000 + f));
                    }
                }
            }
        });
        let tiles = lay(&index, root, 8000.0, 8000.0, 3);
        assert_eq!(tiles.len(), MAX_TILES);
        for (i, t) in tiles.iter().enumerate() {
            if let Some(p) = t.parent {
                assert!(p < i && tiles[p].subdivided);
            }
        }
    }
}
