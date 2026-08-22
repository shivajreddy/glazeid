/// Software-rasterized workspace bar renderer.
///
/// Uses `tiny_skia` for 2-D drawing and `fontdue` for CPU font rasterization.
/// The output is a premultiplied-BGRA `&mut [u32]` pixel buffer
/// (u32 = 0xAARRGGBB), which is exactly what `UpdateLayeredWindow` expects
/// from a 32-bpp DIB section with `AC_SRC_ALPHA`.
use fontdue::{Font, FontSettings};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, PixmapMut, Rect, Transform};

use crate::client::WorkspaceInfo;
use crate::config::Config;

// ---------------------------------------------------------------------------
// Embedded font
// ---------------------------------------------------------------------------

/// Bundled JetBrains Mono — included at compile time so the binary has no
/// runtime font dependency.
const FONT_BYTES: &[u8] = include_bytes!("../resources/JetBrainsMono-Regular.ttf");

// ---------------------------------------------------------------------------
// Content size
// ---------------------------------------------------------------------------

/// Physical pixel dimensions required to render a set of workspace labels.
///
/// Returned by [`Renderer::measure`] so the caller can size the window before
/// painting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentSize {
    pub width: u32,
    pub height: u32,
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// Stateless renderer.
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

    /// Measure the exact pixel dimensions needed to render `workspaces` with
    /// the given config and DPI `scale`.
    ///
    /// Returns a minimum of 1×1 so the window is never zero-sized.
    pub fn measure(&self, workspaces: &[WorkspaceInfo], cfg: &Config, scale: f32) -> ContentSize {
        let font_px = cfg.font_size * scale;
        let pad_x = cfg.label_padding_x * scale;
        let pad_y = cfg.label_padding_y * scale;

        // Cap-height: the tallest glyph in the label set (at minimum font_px).
        let cap_h = workspaces
            .iter()
            .flat_map(|ws| ws.label.chars())
            .map(|ch| {
                let (m, _) = self.font.rasterize(ch, font_px);
                m.height as u32
            })
            .max()
            .unwrap_or(font_px as u32)
            .max(font_px as u32);

        // Height = cap-height + top + bottom padding, minimum 1.
        let height = (cap_h + (pad_y * 2.0) as u32).max(1);

        // Width = sum of pill widths (text_w + 2×pad_x) + leading pad_x gap.
        // Layout: [pad_x][label1][pad_x][pad_x][label2][pad_x]...
        // i.e. each pill = pad_x + text_w + pad_x, pills are adjacent.
        let total_w: u32 = if workspaces.is_empty() {
            1
        } else {
            let labels_w: u32 = workspaces
                .iter()
                .map(|ws| {
                    let text_w: u32 = ws
                        .label
                        .chars()
                        .map(|ch| {
                            let (m, _) = self.font.rasterize(ch, font_px);
                            m.advance_width as u32
                        })
                        .sum();
                    text_w + (pad_x * 2.0) as u32
                })
                .sum();
            // Add a half-pad gap on each outer side.
            labels_w + (pad_x as u32)
        };

        ContentSize {
            width: total_w.max(1),
            height,
        }
    }

    /// Compute the horizontal pixel range `(x_start, x_end)` of every
    /// workspace pill, in the same physical-pixel space [`Renderer::render`]
    /// draws in. Used for click hit-testing.
    pub fn pill_ranges(
        &self,
        workspaces: &[WorkspaceInfo],
        cfg: &Config,
        scale: f32,
    ) -> Vec<(f32, f32)> {
        let font_px = cfg.font_size * scale;
        let pad_x = cfg.label_padding_x * scale;

        let mut cursor_x = pad_x / 2.0;
        workspaces
            .iter()
            .map(|ws| {
                let text_w: f32 = ws
                    .label
                    .chars()
                    .map(|ch| {
                        let (m, _) = self.font.rasterize(ch, font_px);
                        m.advance_width
                    })
                    .sum();
                let pill_w = text_w + pad_x * 2.0;
                let range = (cursor_x, cursor_x + pill_w);
                cursor_x += pill_w;
                range
            })
            .collect()
    }

    /// Render the bar into `buffer` (premultiplied BGRA u32 pixels, row-major).
    ///
    /// `width` and `height` must match the buffer dimensions exactly (they
    /// should come from a prior [`Renderer::measure`] call scaled to physical
    /// pixels).  `scale` converts logical config values to physical pixels.
    /// `dark` selects the light/dark variant of themed colors.
    pub fn render(
        &self,
        buffer: &mut [u32],
        width: u32,
        height: u32,
        scale: f32,
        workspaces: &[WorkspaceInfo],
        cfg: &Config,
        dark: bool,
    ) {
        if width == 0 || height == 0 {
            return;
        }

        let mut pixmap = PixmapMut::from_bytes(bytemuck_u32_to_u8_mut(buffer), width, height)
            .expect("buffer size matches width × height × 4");

        // Clear to background. `fill` overwrites every pixel, so a reused
        // buffer (the DIB section persists across frames) starts clean.
        pixmap.fill(cfg.background.resolve(dark).to_skia());

        let font_px = cfg.font_size * scale;
        let pad_x = cfg.label_padding_x * scale;
        let pad_y = cfg.label_padding_y * scale;
        let radius = cfg.pill_radius * scale;

        // Leading half-gap before the first pill.
        let mut cursor_x = pad_x / 2.0;

        for ws in workspaces {
            let text_w: f32 = ws
                .label
                .chars()
                .map(|ch| {
                    let (m, _) = self.font.rasterize(ch, font_px);
                    m.advance_width
                })
                .sum();

            let pill_w = text_w + pad_x * 2.0;
            let pill_h = height as f32 - pad_y * 0.5; // slight vertical inset
            let pill_y = (height as f32 - pill_h) / 2.0;

            if ws.has_focus {
                draw_rounded_rect(
                    &mut pixmap,
                    cursor_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    radius,
                    cfg.active_bg.resolve(dark).to_skia(),
                );
            } else {
                // Nearly invisible fill (alpha 1/255): layered windows pass
                // clicks through wherever alpha is exactly 0, so this makes
                // the whole pill area clickable — not just the glyph pixels —
                // without any visible difference.
                draw_rounded_rect(
                    &mut pixmap,
                    cursor_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    radius,
                    Color::from_rgba8(0, 0, 0, 1),
                );
            }

            // Text: vertically centred by baseline within the pill.
            let text_x = cursor_x + pad_x;
            // Compute cap-height for this label to centre it.
            let cap_h = ws
                .label
                .chars()
                .map(|ch| {
                    let (m, _) = self.font.rasterize(ch, font_px);
                    m.height as f32
                })
                .fold(0.0f32, f32::max)
                .max(font_px);
            let text_y = (height as f32 - cap_h) / 2.0;

            let fg = if ws.has_focus {
                cfg.active_fg.resolve(dark).to_skia()
            } else {
                cfg.foreground.resolve(dark).to_skia()
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

            cursor_x += pill_w;
        }

        // Drop the pixmap borrow before we touch `buffer` again as u32.
        drop(pixmap);

        // tiny_skia writes premultiplied RGBA bytes [R, G, B, A]; on
        // little-endian that is u32 = 0xAABBGGRR (A high byte, R low).
        // UpdateLayeredWindow wants premultiplied BGRA bytes [B, G, R, A]
        // = u32 0xAARRGGBB → keep A, swap R↔B.
        for pixel in buffer.iter_mut() {
            let r = (*pixel) & 0xFF;
            let g = (*pixel >> 8) & 0xFF;
            let b = (*pixel >> 16) & 0xFF;
            let a = (*pixel >> 24) & 0xFF;
            *pixel = (a << 24) | (r << 16) | (g << 8) | b;
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing helpers
// ---------------------------------------------------------------------------

/// Blit fontdue-rasterized glyphs onto the pixmap.
///
/// Composites in premultiplied space: the destination pixmap is
/// premultiplied, the glyph coverage is the source alpha.
fn draw_text(
    pixmap: &mut PixmapMut,
    font: &Font,
    text: &str,
    font_px: f32,
    x: f32,
    y: f32,
    color: Color,
) {
    let cr = (color.red() * 255.0) as u8;
    let cg = (color.green() * 255.0) as u8;
    let cb = (color.blue() * 255.0) as u8;

    let mut cursor = x;
    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    let data = pixmap.data_mut();

    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, font_px);

        for py in 0..metrics.height {
            for px in 0..metrics.width {
                let alpha = bitmap[py * metrics.width + px];
                if alpha == 0 {
                    continue;
                }
                let dst_x = cursor as i32 + metrics.xmin + px as i32;
                let dst_y =
                    y as i32 + (font_px as i32 - metrics.height as i32 - metrics.ymin) + py as i32;

                if dst_x < 0 || dst_x >= pw || dst_y < 0 || dst_y >= ph {
                    continue;
                }

                let idx = (dst_y as usize * pw as usize + dst_x as usize) * 4;
                if idx + 3 >= data.len() {
                    continue;
                }

                // Premultiplied "over": src channels are c·a/255, dst is
                // already premultiplied; alpha composites the same way.
                let a = alpha as u32;
                let ia = 255 - a;
                data[idx] = ((cr as u32 * a + data[idx] as u32 * ia) / 255) as u8;
                data[idx + 1] = ((cg as u32 * a + data[idx + 1] as u32 * ia) / 255) as u8;
                data[idx + 2] = ((cb as u32 * a + data[idx + 2] as u32 * ia) / 255) as u8;
                data[idx + 3] = ((a * 255 + data[idx + 3] as u32 * ia) / 255) as u8;
            }
        }

        cursor += metrics.advance_width;
    }
}

/// Draw a filled rounded rectangle.
///
/// Builds the path with cubic bezier corners; `tiny_skia` 0.11 does not
/// expose a `RoundRect` builder.
fn draw_rounded_rect(
    pixmap: &mut PixmapMut,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: Color,
) {
    let r = radius.min(w / 2.0).min(h / 2.0);

    if r <= 0.0 {
        let Some(rect) = Rect::from_xywh(x, y, w, h) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(color);
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        return;
    }

    // k ≈ 0.5523: cubic bezier approximation of a quarter-circle.
    let kr = 0.552_284_8_f32 * r;

    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + kr, y, x + w, y + r - kr, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(
        x + w,
        y + h - r + kr,
        x + w - r + kr,
        y + h,
        x + w - r,
        y + h,
    );
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - kr, y + h, x, y + h - r + kr, x, y + h - r);
    pb.line_to(x, y + r);
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

// ---------------------------------------------------------------------------
// Byte-reinterpretation helper
// ---------------------------------------------------------------------------

/// Reinterpret a `&mut [u32]` as `&mut [u8]` (same memory, 4× length).
///
/// # Safety
///
/// `u32` is 4 bytes; `u8` alignment is 1 ≤ 4, so the pointer cast is sound.
/// The slice length is always a multiple of 4 (one u32 per pixel).
fn bytemuck_u32_to_u8_mut(slice: &mut [u32]) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(slice.as_mut_ptr() as *mut u8, slice.len() * 4) }
}
