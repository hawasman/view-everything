//! The Treemap panel: Filtered Sizes as nested rectangles coloured by Category.

use egui::{Color32, Pos2, Rect, Stroke, StrokeKind, Vec2};
use std::collections::BinaryHeap;
use ve_core::{Category, CategoryMap, EntryId, Filter, Index};

/// Stop laying out once this many rectangles exist; deeper detail is invisible anyway.
const MAX_TILES: usize = 60_000;
/// Folders smaller than this (in points, either side) are drawn as one block.
const MIN_FOLDER_SIDE: f32 = 6.0;

pub struct Tile {
    pub rect: Rect,
    pub id: EntryId,
    pub is_dir: bool,
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

const FOLDER_BLOCK: Color32 = Color32::from_rgb(0x70, 0x70, 0x78);

/// Tiles for `root` laid out in `rect`. Folders are subdivided largest first, so when the tile
/// budget runs out the detail missing is in the smallest folders.
pub fn layout(
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
                FOLDER_BLOCK
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

pub fn paint(painter: &egui::Painter, tiles: &[Tile], highlight: Option<EntryId>) {
    let outline = Stroke::new(1.0, Color32::from_black_alpha(140));
    for tile in tiles {
        if tile.is_dir {
            painter.rect_filled(tile.rect, 0.0, tile.color.gamma_multiply(0.35));
            painter.rect_stroke(
                tile.rect,
                0.0,
                Stroke::new(1.0, Color32::from_black_alpha(200)),
                StrokeKind::Inside,
            );
        } else {
            painter.rect_filled(tile.rect, 0.0, tile.color);
            if tile.rect.width() > 3.0 && tile.rect.height() > 3.0 {
                painter.rect_stroke(tile.rect, 0.0, outline, StrokeKind::Inside);
            }
        }
    }
    if let Some(selected) = highlight
        && let Some(tile) = tiles.iter().find(|t| t.id == selected)
    {
        painter.rect_stroke(
            tile.rect,
            0.0,
            Stroke::new(2.5, Color32::WHITE),
            StrokeKind::Inside,
        );
        painter.rect_stroke(
            tile.rect.shrink(2.5),
            0.0,
            Stroke::new(1.0, Color32::BLACK),
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
