//! The Treemap panel: Filtered Sizes as nested rectangles coloured by Category.

use crate::engine::Shared;
use crate::theme;
use crate::view::View;
use egui::{Color32, Pos2, Rect, Stroke, StrokeKind, Vec2};
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use ve_core::{Category, CategoryMap, EntryId, Filter, Index};

/// Stop laying out once this many rectangles exist; deeper detail is invisible anyway.
const MAX_TILES: usize = 60_000;
/// Folders smaller than this (in points, either side) are drawn as one block.
const MIN_FOLDER_SIDE: f32 = 6.0;

pub struct Tile {
    pub rect: Rect,
    pub id: EntryId,
    pub is_dir: bool,
    /// A file's Category colour; folders take the palette's folder colour when painted, so
    /// laid-out tiles survive a switch between light and dark.
    pub color: Color32,
}

pub fn category_color(category: Category) -> Color32 {
    match category {
        Category::Video => Color32::from_rgb(0xE8, 0x68, 0x4A),
        Category::Image => Color32::from_rgb(0x5B, 0x8F, 0xF9),
        Category::Audio => Color32::from_rgb(0x92, 0x70, 0xCA),
        Category::Archive => Color32::from_rgb(0xF6, 0xBD, 0x16),
        Category::Code => Color32::from_rgb(0x5A, 0xD8, 0xA6),
        Category::Document => Color32::from_rgb(0x6D, 0xC8, 0xEC),
        Category::Executable => Color32::from_rgb(0xFF, 0x99, 0xC3),
        Category::Other => Color32::from_rgb(0x9A, 0x9A, 0x9A),
    }
}

/// What a set of tiles is laid out for: the View id, the zoom root and the pixel rectangle.
type LayoutKey = (u64, EntryId, [i32; 4]);

struct Request {
    view: Arc<View>,
    root: EntryId,
    rect: Rect,
}

/// Lays out tiles on a background thread, so a new View or a zoom never stalls a frame. Only
/// the newest request is served; the GUI keeps painting the previous tiles until it arrives.
pub struct Layouter {
    requests: Sender<Request>,
    results: Receiver<Vec<Tile>>,
    requested: Option<LayoutKey>,
}

impl Layouter {
    pub fn start(ctx: egui::Context, shared: Arc<Shared>, categories: CategoryMap) -> Layouter {
        let (requests, inbox) = mpsc::channel::<Request>();
        let (outbox, results) = mpsc::channel();
        thread::Builder::new()
            .name("ve-treemap".into())
            .spawn(move || {
                while let Ok(first) = inbox.recv() {
                    let request = inbox.try_iter().last().unwrap_or(first);
                    let tiles = {
                        let index = shared.index.read();
                        let filter = &request.view.filter;
                        layout(&index, filter, &categories, request.root, request.rect)
                    };
                    if outbox.send(tiles).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn treemap thread");
        Layouter {
            requests,
            results,
            requested: None,
        }
    }

    /// Asks for tiles of `root` in `rect` under `view`, unless already asked.
    pub fn request(&mut self, view: &Arc<View>, root: EntryId, rect: Rect) {
        let edges = [rect.min.x, rect.min.y, rect.max.x, rect.max.y].map(|v| v.round() as i32);
        let key = (view.id, root, edges);
        if self.requested == Some(key) {
            return;
        }
        self.requested = Some(key);
        let _ = self.requests.send(Request {
            view: view.clone(),
            root,
            rect,
        });
    }

    /// The newest tiles finished since the last call.
    pub fn finished(&self) -> Option<Vec<Tile>> {
        self.results.try_iter().last()
    }
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
    let mut tiles = Vec::new();
    let mut folders = vec![(root, rect)];
    let mut queue = BinaryHeap::from([(area_key(rect), 0usize)]);
    while let Some((_, slot)) = queue.pop() {
        let (dir, area) = folders[slot];
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
                return tiles;
            }
            let is_dir = index.is_dir(id);
            let color = if is_dir {
                Color32::TRANSPARENT
            } else {
                category_color(categories.of_name(index.name(id)))
            };
            tiles.push(Tile {
                rect,
                id,
                is_dir,
                color,
            });
            if is_dir && rect.width() >= MIN_FOLDER_SIDE && rect.height() >= MIN_FOLDER_SIDE {
                let inner = rect.shrink(1.0);
                queue.push((area_key(inner), folders.len()));
                folders.push((id, inner));
            }
        }
    }
    tiles
}

fn area_key(rect: Rect) -> u64 {
    (rect.area().max(0.0) * 16.0) as u64
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
                Rect::from_min_size(
                    Pos2::new(free.min.x, free.min.y + offset),
                    Vec2::new(thickness, length),
                )
            } else {
                Rect::from_min_size(
                    Pos2::new(free.min.x + offset, free.min.y),
                    Vec2::new(length, thickness),
                )
            };
            out.push((rect, items[i].1));
            offset += length;
        }
        if horizontal {
            free.min.x += thickness;
        } else {
            free.min.y += thickness;
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

pub fn paint(painter: &egui::Painter, p: &theme::Palette, tiles: &[Tile], highlight: Option<EntryId>) {
    // Files are split by 1px seams in the surface colour; each folder by a 1px grey edge, so
    // nesting reads even where the tile budget left a folder as one block.
    let seam = Stroke::new(1.0, p.seam);
    let folder_edge = Stroke::new(1.0, p.folder_edge);
    for tile in tiles {
        let fill = if tile.is_dir { p.folder } else { tile.color };
        painter.rect_filled(tile.rect, 0.0, fill);
        if tile.rect.width() > 3.0 && tile.rect.height() > 3.0 {
            let edge = if tile.is_dir { folder_edge } else { seam };
            painter.rect_stroke(tile.rect, 0.0, edge, StrokeKind::Inside);
        }
    }
    if let Some(selected) = highlight
        && let Some(tile) = tiles.iter().find(|t| t.id == selected)
    {
        painter.rect_stroke(tile.rect, 0.0, Stroke::new(2.0, p.text), StrokeKind::Inside);
        painter.rect_stroke(
            tile.rect.shrink(2.0),
            0.0,
            Stroke::new(1.0, p.highlight_inner),
            StrokeKind::Inside,
        );
    }
}

/// The deepest tile under `pos`.
pub fn hit(tiles: &[Tile], pos: Pos2) -> Option<&Tile> {
    // Children follow their parents, but siblings of different folders interleave because of
    // the work queue; pick the smallest containing tile, which is the deepest.
    tiles
        .iter()
        .filter(|t| t.rect.contains(pos))
        .min_by(|a, b| (a.rect.area()).total_cmp(&b.rect.area()))
}
