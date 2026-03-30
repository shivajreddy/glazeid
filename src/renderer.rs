/// Software-rasterized workspace bar renderer.
///
/// Uses `tiny_skia` for 2-D drawing and `fontdue` for CPU font rasterization.
/// The output is a `Vec<u32>` ARGB pixel buffer suitable for `softbuffer`.
use fontdue::{Font, FontSettings};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, PixmapMut, Rect, Transform};

use crate::client::WorkspaceInfo;
use crate::config::Config;

// ---------------------------------------------------------------------------
// Embedded font
// ---------------------------------------------------------------------------

/// Bundled subset of JetBrains Mono — included at compile time so the binary
/// has no runtime font dependency.
///
/// We embed a minimal `.ttf` file kept in `resources/`.  During development
/// any monospace TTF can be dropped there.
const FONT_BYTES: &[u8] = include_bytes!("../resources/JetBrainsMono-Regular.ttf");

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// Stateless renderer.  Call `render` every time the state changes.
pub struct Renderer {
    font: Font,
}

impl Renderer {
    /// Create a new renderer, loading the embedded font.
    pub fn new() -> Self {
        let font =
            Font::from_bytes(FONT_BYTES, FontSettings::default()).expect("embedded font is valid");
        Self { font }
    }

    /// Render the bar into `buffer` (ARGB u32 pixels, row-major).
    ///
    /// `width` and `height` are the physical pixel dimensions of the bar
    /// window.  `scale` is the DPI scale factor (e.g. 1.0, 1.5, 2.0) used to
    /// convert logical pixel values from the config into physical pixels.
    pub fn render(
        &self,
        buffer: &mut [u32],
        width: u32,
        height: u32,
        scale: f32,
        workspaces: &[WorkspaceInfo],
        cfg: &Config,
    ) {
        if width == 0 || height == 0 {
            return;
        }

        // Build a pixmap over the caller's buffer.
        let mut pixmap = PixmapMut::from_bytes(
            // SAFETY: softbuffer gives us a `&mut [u32]`; we reinterpret as
            // `&mut [u8]` (4 bytes per pixel, same length × 4).
            bytemuck_u32_to_u8_mut(buffer),
            width,
            height,
        )
        .expect("buffer size matches width × height × 4");

        // Clear to background.
        pixmap.fill(cfg.background.to_skia());

        let font_px = cfg.font_size * scale;
        let pad_x = cfg.label_padding_x * scale;
        let pad_y = cfg.label_padding_y * scale;
        let radius = cfg.pill_radius * scale;

        // Measure all labels first so we can centre them vertically.
        let metrics_list: Vec<_> = workspaces
            .iter()
            .map(|ws| measure_text(&self.font, &ws.label, font_px))
            .collect();

        let mut cursor_x = pad_x;

        for (ws, (text_w, text_h)) in workspaces.iter().zip(metrics_list.iter()) {
            let text_w = *text_w as f32;
            let text_h = *text_h as f32;
            let pill_w = text_w + pad_x * 2.0;
            let pill_h = (height as f32).min(text_h + pad_y * 2.0);
            let pill_y = (height as f32 - pill_h) / 2.0;

            if ws.has_focus {
                // Draw filled rounded rectangle.
                draw_rounded_rect(
                    &mut pixmap,
                    cursor_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    radius,
                    cfg.active_bg.to_skia(),
                );
            }

            // Draw text centred inside the pill.
            let text_x = cursor_x + pad_x;
            let text_y = (height as f32 - text_h) / 2.0;
            let fg = if ws.has_focus {
                cfg.active_fg.to_skia()
            } else {
                cfg.foreground.to_skia()
            };
            draw_text(
                &mut pixmap,
                &self.font,
                &ws.label,
                font_px,
                text_x,
                text_y,
                fg,
            );

            cursor_x += pill_w + pad_x;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Measure the bounding box of `text` rendered at `font_px` physical pixels.
/// Returns `(width_px, height_px)` as integer pixels.
fn measure_text(font: &Font, text: &str, font_px: f32) -> (u32, u32) {
    let mut total_w = 0u32;
    let mut max_h = 0u32;
    for ch in text.chars() {
        let (metrics, _) = font.rasterize(ch, font_px);
        total_w += metrics.advance_width as u32;
        let h = (metrics.height) as u32;
        if h > max_h {
            max_h = h;
        }
    }
    (total_w, max_h.max(font_px as u32))
}

/// Blit a fontdue-rasterized glyph onto the pixmap at physical pixel
/// position `(x, y)` with the given foreground color.
fn draw_text(
    pixmap: &mut PixmapMut,
    font: &Font,
    text: &str,
    font_px: f32,
    x: f32,
    y: f32,
    color: Color,
) {
    let (cr, cg, cb, _) = (
        (color.red() * 255.0) as u8,
        (color.green() * 255.0) as u8,
        (color.blue() * 255.0) as u8,
        (color.alpha() * 255.0) as u8,
    );

    let mut cursor = x;
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    let data = pixmap.data_mut();

    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, font_px);

        for py in 0..metrics.height {
            for px in 0..metrics.width {
                let alpha = bitmap[py * metrics.width + px];
                if alpha == 0 {
                    continue;
                }
                let px_x = (cursor as i32) + metrics.xmin + px as i32;
                let px_y = (y as i32)
                    + (font_px as i32 - metrics.height as i32 - metrics.ymin)
                    + py as i32;

                if px_x < 0 || px_x >= w || px_y < 0 || px_y >= h {
                    continue;
                }

                let idx = (px_y as usize * w as usize + px_x as usize) * 4;
                if idx + 3 >= data.len() {
                    continue;
                }

                // Alpha blend over existing pixel.
                let a = alpha as u32;
                let ia = 255 - a;
                data[idx] = ((cr as u32 * a + data[idx] as u32 * ia) / 255) as u8;
                data[idx + 1] = ((cg as u32 * a + data[idx + 1] as u32 * ia) / 255) as u8;
                data[idx + 2] = ((cb as u32 * a + data[idx + 2] as u32 * ia) / 255) as u8;
                data[idx + 3] = 255;
            }
        }

        cursor += metrics.advance_width;
    }
}

/// Draw a filled rounded rectangle using `tiny_skia`.
///
/// Builds the path manually with cubic bezier corners because `tiny_skia`
/// 0.11 does not expose a `RoundRect` builder.  The Bezier approximation
/// constant `k ≈ 0.5523` gives a visually accurate circle arc quarter.
fn draw_rounded_rect(
    pixmap: &mut PixmapMut,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: Color,
) {
    // Clamp radius so it never exceeds half of the smaller dimension.
    let r = radius.min(w / 2.0).min(h / 2.0);

    if r <= 0.0 {
        // Degenerate: plain rectangle.
        let Some(rect) = Rect::from_xywh(x, y, w, h) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(color);
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        return;
    }

    // Cubic bezier approximation constant for a quarter-circle arc.
    const K: f32 = 0.552_284_8;
    let kr = k_r(r, K);

    let mut pb = PathBuilder::new();

    // Top edge: start after top-left corner arc.
    pb.move_to(x + r, y);
    // Top-right corner.
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + kr, y, x + w, y + r - kr, x + w, y + r);
    // Right edge.
    pb.line_to(x + w, y + h - r);
    // Bottom-right corner.
    pb.cubic_to(
        x + w,
        y + h - r + kr,
        x + w - r + kr,
        y + h,
        x + w - r,
        y + h,
    );
    // Bottom edge.
    pb.line_to(x + r, y + h);
    // Bottom-left corner.
    pb.cubic_to(x + r - kr, y + h, x, y + h - r + kr, x, y + h - r);
    // Left edge.
    pb.line_to(x, y + r);
    // Top-left corner.
    pb.cubic_to(x, y + r - kr, x + r - kr, y, x + r, y);
    pb.close();

    let Some(path) = pb.finish() else {
        return;
    };

    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

/// Compute `k × r` once.
#[inline(always)]
fn k_r(r: f32, k: f32) -> f32 {
    k * r
}

// ---------------------------------------------------------------------------
// Byte-reinterpretation helper (avoids pulling in `bytemuck`)
// ---------------------------------------------------------------------------

/// Reinterpret a `&mut [u32]` as `&mut [u8]` (same memory, 4× length).
///
/// # Safety
///
/// `u32` and `u8` have no alignment conflicts; the length is always a multiple
/// of 4 because it represents complete 32-bit pixels.
fn bytemuck_u32_to_u8_mut(slice: &mut [u32]) -> &mut [u8] {
    // SAFETY: u32 is 4 bytes, so len×4 bytes is valid as [u8]; alignment
    // of u8 (1) is less than u32 (4) so the cast is sound.
    unsafe { std::slice::from_raw_parts_mut(slice.as_mut_ptr() as *mut u8, slice.len() * 4) }
}
