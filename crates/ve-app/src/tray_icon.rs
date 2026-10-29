//! The app icon as pixels for the tray: Windows and macOS always want a bitmap, and a Linux
//! tray host falls back to one when the `view-everything` icon is not installed in a theme
//! (a build run from the source tree).

use resvg::{tiny_skia, usvg};

static SVG: &[u8] = include_bytes!("../../../packaging/icons/view-everything.svg");

fn render(size: u32) -> Option<tiny_skia::Pixmap> {
    let tree = usvg::Tree::from_data(SVG, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Some(pixmap)
}

/// `size` x `size` RGBA pixels (alpha not premultiplied).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn rgba(size: u32) -> Option<Vec<u8>> {
    let pixmap = render(size)?;
    let mut out = Vec::with_capacity(pixmap.data().len());
    for p in pixmap.pixels() {
        let c = p.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Some(out)
}

/// The same as a PNG file.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn png(size: u32) -> Option<Vec<u8>> {
    render(size)?.encode_png().ok()
}

/// `size` x `size` ARGB32 pixels in network byte order, alpha not premultiplied: the
/// StatusNotifierItem `IconPixmap` format.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn argb(size: u32) -> Option<Vec<u8>> {
    let pixmap = render(size)?;
    let mut out = Vec::with_capacity(pixmap.data().len());
    for p in pixmap.pixels() {
        let c = p.demultiply();
        out.extend_from_slice(&[c.alpha(), c.red(), c.green(), c.blue()]);
    }
    Some(out)
}
