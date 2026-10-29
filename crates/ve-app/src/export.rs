//! Export actions: a native save dialog, then the file written off the UI thread, then a toast.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Window};
use std::io;
use std::path::{Path, PathBuf};

/// Pixels per drawn pixel in an exported Treemap, so it stays sharp on high-density screens.
const PNG_SCALE: f32 = 2.0;

/// Asks where to save `name`, runs `write` there on a background thread and reports the result.
/// Where the platform has no save dialog the file goes to the Downloads folder (else home).
pub fn save(window: &mut Window, cx: &mut App, name: &str, write: impl FnOnce(&Path) -> io::Result<()> + Send + 'static) {
    let dir = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));
    let asked = cx.prompt_for_new_path(&dir, Some(name));
    let fallback = dir.join(name);
    window
        .spawn(cx, async move |cx| {
            let path = match asked.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(_)) => fallback,
            };
            let shown = path.display().to_string();
            let result = cx.background_executor().spawn(async move { write(&path) }).await;
            cx.update(|window, cx| {
                let note = match result {
                    Ok(()) => Notification::info(format!("Exported to {shown}")),
                    Err(e) => Notification::error(format!("Could not export to {shown}: {e}")),
                };
                window.push_notification(note, cx);
            })
            .ok();
        })
        .detach();
}

/// A filled rectangle in drawn pixels with its colour.
pub struct Block {
    pub rect: [f32; 4],
    pub rgb: [u8; 3],
}

/// Paints `blocks` over `background` on a `width` x `height` canvas (drawn pixels, scaled by
/// `PNG_SCALE`) and encodes it as a PNG.
pub fn encode_png(width: f32, height: f32, background: [u8; 3], blocks: &[Block]) -> io::Result<Vec<u8>> {
    let (w, h) = (
        (width * PNG_SCALE).round().max(1.) as usize,
        (height * PNG_SCALE).round().max(1.) as usize,
    );
    let mut pixels: Vec<u8> = background.iter().copied().cycle().take(w * h * 3).collect();
    for block in blocks {
        let [x0, y0, x1, y1] = block.rect.map(|v| v * PNG_SCALE);
        let (x0, x1) = ((x0.round().max(0.) as usize).min(w), (x1.round().max(0.) as usize).min(w));
        let (y0, y1) = ((y0.round().max(0.) as usize).min(h), (y1.round().max(0.) as usize).min(h));
        for y in y0..y1 {
            for x in x0..x1 {
                pixels[(y * w + x) * 3..][..3].copy_from_slice(&block.rgb);
            }
        }
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    writer.write_image_data(&pixels).map_err(io::Error::other)?;
    writer.finish().map_err(io::Error::other)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_has_the_scaled_size_and_the_block_colour() {
        let bytes = encode_png(
            10.,
            5.,
            [1, 2, 3],
            &[Block {
                rect: [0., 0., 5., 5.],
                rgb: [200, 100, 50],
            }],
        )
        .unwrap();
        let mut reader = png::Decoder::new(io::Cursor::new(bytes)).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (20, 10));
        assert_eq!(&buf[..3], &[200, 100, 50]);
        assert_eq!(&buf[(19) * 3..][..3], &[1, 2, 3]);
    }
}
