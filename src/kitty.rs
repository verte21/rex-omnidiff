use std::io::{Cursor, Write};
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

pub const KITTY_IMAGE_ID: u32 = 4242;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyImageMode {
    SideBySide,
    DiffHeatmap,
    SplitWipe,
    OnionBlend,
}

impl KittyImageMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::SideBySide => "Side-by-Side",
            Self::DiffHeatmap => "Pixel Heatmap",
            Self::SplitWipe => "Split Wipe",
            Self::OnionBlend => "Onion Blend",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::SideBySide => Self::DiffHeatmap,
            Self::DiffHeatmap => Self::SplitWipe,
            Self::SplitWipe => Self::OnionBlend,
            Self::OnionBlend => Self::SideBySide,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundingBox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub changed_pixels: u64,
}

#[derive(Debug, Clone)]
pub struct ImageDiffStats {
    pub before_dims: Option<(u32, u32)>,
    pub after_dims: Option<(u32, u32)>,
    pub changed_pixels: u64,
    pub total_pixels: u64,
    pub regions: Vec<BoundingBox>,
}

fn load_rgba(path: Option<&str>) -> Option<RgbaImage> {
    let p = path.filter(|s| !s.is_empty())?;
    let path_ref = Path::new(p);
    if !path_ref.is_file() {
        return None;
    }
    image::open(path_ref).ok().map(DynamicImage::into_rgba8)
}

pub fn compute_image_diff_stats(
    before: Option<&RgbaImage>,
    after: Option<&RgbaImage>,
) -> ImageDiffStats {
    let before_dims = before.map(|img| (img.width(), img.height()));
    let after_dims = after.map(|img| (img.width(), img.height()));

    let w = before_dims
        .map(|d| d.0)
        .unwrap_or(0)
        .max(after_dims.map(|d| d.0).unwrap_or(0));
    let h = before_dims
        .map(|d| d.1)
        .unwrap_or(0)
        .max(after_dims.map(|d| d.1).unwrap_or(0));

    if w == 0 || h == 0 {
        return ImageDiffStats {
            before_dims,
            after_dims,
            changed_pixels: 0,
            total_pixels: 0,
            regions: Vec::new(),
        };
    }

    let total_pixels = (w as u64) * (h as u64);
    let mut changed_pixels = 0u64;

    // Grid-based clustering (8x8 tiles) to identify bounding boxes of changed regions
    let cols = 8usize;
    let rows = 8usize;
    let mut cell_min_x = vec![u32::MAX; cols * rows];
    let mut cell_min_y = vec![u32::MAX; cols * rows];
    let mut cell_max_x = vec![0u32; cols * rows];
    let mut cell_max_y = vec![0u32; cols * rows];
    let mut cell_counts = vec![0u64; cols * rows];

    for y in 0..h {
        for x in 0..w {
            let pb = before.and_then(|img| {
                if x < img.width() && y < img.height() {
                    Some(*img.get_pixel(x, y))
                } else {
                    None
                }
            });
            let pa = after.and_then(|img| {
                if x < img.width() && y < img.height() {
                    Some(*img.get_pixel(x, y))
                } else {
                    None
                }
            });
            let diff = match (pb, pa) {
                (Some(b), Some(a)) => pixel_delta(b, a) > 12,
                (None, None) => false,
                _ => true,
            };
            if diff {
                changed_pixels += 1;
                let cx = ((x as usize) * cols / (w as usize).max(1)).min(cols - 1);
                let cy = ((y as usize) * rows / (h as usize).max(1)).min(rows - 1);
                let idx = cy * cols + cx;
                cell_min_x[idx] = cell_min_x[idx].min(x);
                cell_min_y[idx] = cell_min_y[idx].min(y);
                cell_max_x[idx] = cell_max_x[idx].max(x);
                cell_max_y[idx] = cell_max_y[idx].max(y);
                cell_counts[idx] += 1;
            }
        }
    }

    let mut regions: Vec<BoundingBox> = Vec::new();
    for idx in 0..(cols * rows) {
        if cell_counts[idx] > 0 {
            let min_x = cell_min_x[idx];
            let min_y = cell_min_y[idx];
            let max_x = cell_max_x[idx];
            let max_y = cell_max_y[idx];
            regions.push(BoundingBox {
                x: min_x,
                y: min_y,
                width: (max_x - min_x + 1).max(1),
                height: (max_y - min_y + 1).max(1),
                changed_pixels: cell_counts[idx],
            });
        }
    }
    regions.sort_by(|a, b| b.changed_pixels.cmp(&a.changed_pixels));
    regions.truncate(8);

    ImageDiffStats {
        before_dims,
        after_dims,
        changed_pixels,
        total_pixels,
        regions,
    }
}

#[inline]
fn pixel_delta(a: Rgba<u8>, b: Rgba<u8>) -> u16 {
    let dr = (a[0] as i16 - b[0] as i16).unsigned_abs();
    let dg = (a[1] as i16 - b[1] as i16).unsigned_abs();
    let db = (a[2] as i16 - b[2] as i16).unsigned_abs();
    let da = (a[3] as i16 - b[3] as i16).unsigned_abs();
    dr.max(dg).max(db).max(da)
}

fn draw_rect_outline(
    canvas: &mut RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: Rgba<u8>,
    thickness: u32,
) {
    let cw = canvas.width();
    let ch = canvas.height();
    if cw == 0 || ch == 0 || w == 0 || h == 0 {
        return;
    }
    let x1 = (x + w).min(cw);
    let y1 = (y + h).min(ch);
    for t in 0..thickness {
        let top = (y + t).min(ch - 1);
        let bot = y1.saturating_sub(1 + t).min(ch - 1);
        for px in x.min(cw - 1)..x1 {
            canvas.put_pixel(px, top, color);
            canvas.put_pixel(px, bot, color);
        }
        let left = (x + t).min(cw - 1);
        let right = x1.saturating_sub(1 + t).min(cw - 1);
        for py in y.min(ch - 1)..y1 {
            canvas.put_pixel(left, py, color);
            canvas.put_pixel(right, py, color);
        }
    }
}

fn fit_dimensions(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (max_w.max(1), max_h.max(1));
    }
    let scale_x = (max_w as f64) / (src_w as f64);
    let scale_y = (max_h as f64) / (src_h as f64);
    let scale = scale_x.min(scale_y);
    let out_w = ((src_w as f64) * scale).round().max(1.0) as u32;
    let out_h = ((src_h as f64) * scale).round().max(1.0) as u32;
    (out_w.min(max_w), out_h.min(max_h))
}

fn composite_over_checkerboard(img: &RgbaImage, ox: u32, oy: u32, dst: &mut RgbaImage) {
    for y in 0..img.height() {
        let dy = oy + y;
        if dy >= dst.height() {
            break;
        }
        for x in 0..img.width() {
            let dx = ox + x;
            if dx >= dst.width() {
                break;
            }
            let p = *img.get_pixel(x, y);
            let checker = if ((x / 8) + (y / 8)) % 2 == 0 {
                34u8
            } else {
                24u8
            };
            let alpha = (p[3] as f32) / 255.0;
            let r = ((p[0] as f32) * alpha + (checker as f32) * (1.0 - alpha)).round() as u8;
            let g = ((p[1] as f32) * alpha + (checker as f32) * (1.0 - alpha)).round() as u8;
            let b = ((p[2] as f32) * alpha + (checker as f32) * (1.0 - alpha)).round() as u8;
            dst.put_pixel(dx, dy, Rgba([r, g, b, 255]));
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_composite_diff_image(
    before: Option<&RgbaImage>,
    after: Option<&RgbaImage>,
    stats: &ImageDiffStats,
    mode: KittyImageMode,
    slider_pct: u8,
    show_boxes: bool,
    canvas_w: u32,
    canvas_h: u32,
) -> RgbaImage {
    let cw = canvas_w.clamp(120, 960);
    let ch = canvas_h.clamp(80, 520);
    let mut canvas = RgbaImage::from_pixel(cw, ch, Rgba([20, 22, 30, 255]));

    let ref_w = stats
        .after_dims
        .or(stats.before_dims)
        .map(|d| d.0)
        .unwrap_or(256)
        .max(1);
    let ref_h = stats
        .after_dims
        .or(stats.before_dims)
        .map(|d| d.1)
        .unwrap_or(256)
        .max(1);

    match mode {
        KittyImageMode::SideBySide => {
            let gap = 12u32;
            let half_w = cw.saturating_sub(gap) / 2;
            let inner_h = ch.saturating_sub(8);

            if let Some(b_img) = before {
                let (tw, th) = fit_dimensions(b_img.width(), b_img.height(), half_w.saturating_sub(8), inner_h);
                let resized = image::imageops::resize(b_img, tw, th, FilterType::Triangle);
                let ox = (half_w.saturating_sub(tw)) / 2;
                let oy = (ch.saturating_sub(th)) / 2;
                composite_over_checkerboard(&resized, ox, oy, &mut canvas);
                draw_rect_outline(&mut canvas, ox, oy, tw, th, Rgba([247, 118, 142, 220]), 1);
            }

            if let Some(a_img) = after {
                let (tw, th) = fit_dimensions(a_img.width(), a_img.height(), half_w.saturating_sub(8), inner_h);
                let resized = image::imageops::resize(a_img, tw, th, FilterType::Triangle);
                let ox = half_w + gap + (half_w.saturating_sub(tw)) / 2;
                let oy = (ch.saturating_sub(th)) / 2;
                composite_over_checkerboard(&resized, ox, oy, &mut canvas);
                draw_rect_outline(&mut canvas, ox, oy, tw, th, Rgba([158, 206, 106, 220]), 1);

                if show_boxes {
                    let sx = (tw as f64) / (a_img.width().max(1) as f64);
                    let sy = (th as f64) / (a_img.height().max(1) as f64);
                    for reg in &stats.regions {
                        let rx = ox + ((reg.x as f64) * sx).floor() as u32;
                        let ry = oy + ((reg.y as f64) * sy).floor() as u32;
                        let rw = (((reg.width as f64) * sx).ceil() as u32).max(3);
                        let rh = (((reg.height as f64) * sy).ceil() as u32).max(3);
                        draw_rect_outline(
                            &mut canvas,
                            rx,
                            ry,
                            rw,
                            rh,
                            Rgba([125, 207, 255, 255]),
                            2,
                        );
                    }
                }
            }
        }
        KittyImageMode::DiffHeatmap => {
            let (tw, th) = fit_dimensions(ref_w, ref_h, cw.saturating_sub(12), ch.saturating_sub(8));
            let b_scaled = before.map(|img| image::imageops::resize(img, tw, th, FilterType::Triangle));
            let a_scaled = after.map(|img| image::imageops::resize(img, tw, th, FilterType::Triangle));
            let ox = (cw.saturating_sub(tw)) / 2;
            let oy = (ch.saturating_sub(th)) / 2;

            for y in 0..th {
                for x in 0..tw {
                    let pb = b_scaled.as_ref().map(|i| *i.get_pixel(x, y));
                    let pa = a_scaled.as_ref().map(|i| *i.get_pixel(x, y));
                    let delta = match (pb, pa) {
                        (Some(b), Some(a)) => pixel_delta(b, a),
                        (None, None) => 0,
                        _ => 255,
                    };
                    let out_px = if delta > 12 {
                        let intensity = ((delta as f32 / 255.0).sqrt() * 200.0 + 55.0).min(255.0) as u8;
                        Rgba([255, 75, 130, 255]).map_with_alpha(|c| c, |_| intensity)
                    } else if let Some(base) = pa.or(pb) {
                        let lum = ((base[0] as u16 * 30 + base[1] as u16 * 59 + base[2] as u16 * 11) / 100) as u8;
                        let dim = (lum / 3).saturating_add(16);
                        Rgba([dim, dim, dim.saturating_add(6), 255])
                    } else {
                        Rgba([24, 26, 36, 255])
                    };
                    canvas.put_pixel(ox + x, oy + y, Rgba([out_px[0], out_px[1], out_px[2], 255]));
                }
            }
            draw_rect_outline(&mut canvas, ox, oy, tw, th, Rgba([130, 170, 255, 220]), 1);
            if show_boxes {
                let sx = (tw as f64) / (ref_w as f64);
                let sy = (th as f64) / (ref_h as f64);
                for reg in &stats.regions {
                    let rx = ox + ((reg.x as f64) * sx).floor() as u32;
                    let ry = oy + ((reg.y as f64) * sy).floor() as u32;
                    let rw = (((reg.width as f64) * sx).ceil() as u32).max(3);
                    let rh = (((reg.height as f64) * sy).ceil() as u32).max(3);
                    draw_rect_outline(
                        &mut canvas,
                        rx,
                        ry,
                        rw,
                        rh,
                        Rgba([134, 225, 252, 255]),
                        2,
                    );
                }
            }
        }
        KittyImageMode::SplitWipe => {
            let (tw, th) = fit_dimensions(ref_w, ref_h, cw.saturating_sub(12), ch.saturating_sub(8));
            let b_scaled = before.map(|img| image::imageops::resize(img, tw, th, FilterType::Triangle));
            let a_scaled = after.map(|img| image::imageops::resize(img, tw, th, FilterType::Triangle));
            let ox = (cw.saturating_sub(tw)) / 2;
            let oy = (ch.saturating_sub(th)) / 2;
            let split_x = (tw * (slider_pct.clamp(0, 100) as u32) / 100).min(tw.saturating_sub(1));

            for y in 0..th {
                for x in 0..tw {
                    let src = if x <= split_x {
                        b_scaled.as_ref().map(|i| *i.get_pixel(x, y))
                    } else {
                        a_scaled.as_ref().map(|i| *i.get_pixel(x, y))
                    };
                    if let Some(p) = src {
                        let alpha = (p[3] as f32) / 255.0;
                        let bg = 28.0f32;
                        let r = ((p[0] as f32) * alpha + bg * (1.0 - alpha)).round() as u8;
                        let g = ((p[1] as f32) * alpha + bg * (1.0 - alpha)).round() as u8;
                        let b = ((p[2] as f32) * alpha + bg * (1.0 - alpha)).round() as u8;
                        canvas.put_pixel(ox + x, oy + y, Rgba([r, g, b, 255]));
                    }
                }
                // Vertical accent wipe bar
                let bar_x = (ox + split_x).min(cw - 1);
                canvas.put_pixel(bar_x, oy + y, Rgba([134, 225, 252, 255]));
                if bar_x + 1 < cw {
                    canvas.put_pixel(bar_x + 1, oy + y, Rgba([130, 170, 255, 255]));
                }
            }
            draw_rect_outline(&mut canvas, ox, oy, tw, th, Rgba([130, 170, 255, 220]), 1);
        }
        KittyImageMode::OnionBlend => {
            let (tw, th) = fit_dimensions(ref_w, ref_h, cw.saturating_sub(12), ch.saturating_sub(8));
            let b_scaled = before.map(|img| image::imageops::resize(img, tw, th, FilterType::Triangle));
            let a_scaled = after.map(|img| image::imageops::resize(img, tw, th, FilterType::Triangle));
            let ox = (cw.saturating_sub(tw)) / 2;
            let oy = (ch.saturating_sub(th)) / 2;
            let w_after = (slider_pct.clamp(0, 100) as f32) / 100.0;
            let w_before = 1.0 - w_after;

            for y in 0..th {
                for x in 0..tw {
                    let pb = b_scaled
                        .as_ref()
                        .map(|i| *i.get_pixel(x, y))
                        .unwrap_or(Rgba([24, 26, 36, 255]));
                    let pa = a_scaled
                        .as_ref()
                        .map(|i| *i.get_pixel(x, y))
                        .unwrap_or(Rgba([24, 26, 36, 255]));
                    let r = ((pb[0] as f32) * w_before + (pa[0] as f32) * w_after).round() as u8;
                    let g = ((pb[1] as f32) * w_before + (pa[1] as f32) * w_after).round() as u8;
                    let b = ((pb[2] as f32) * w_before + (pa[2] as f32) * w_after).round() as u8;
                    canvas.put_pixel(ox + x, oy + y, Rgba([r, g, b, 255]));
                }
            }
            draw_rect_outline(&mut canvas, ox, oy, tw, th, Rgba([192, 153, 255, 220]), 1);
        }
    }

    canvas
}

use image::Pixel;

pub fn encode_kitty_png_escape(
    img: &RgbaImage,
    col_1idx: u16,
    row_1idx: u16,
    cell_cols: u16,
    cell_rows: u16,
) -> Result<String> {
    let mut png_bytes: Vec<u8> = Vec::new();
    DynamicImage::ImageRgba8(img.clone())
        .write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)
        .context("failed to encode composite diff image as PNG")?;

    let b64 = BASE64.encode(&png_bytes);
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    let mut out = String::with_capacity(b64.len() + 256);
    // Delete previous placement with id=KITTY_IMAGE_ID quietly (`q=2`), then move cursor to target cell
    out.push_str(&format!(
        "\x1b_Ga=d,d=I,i={KITTY_IMAGE_ID},q=2\x1b\\\x1b[{row_1idx};{col_1idx}H"
    ));

    for (idx, chunk) in chunks.iter().enumerate() {
        let more = if idx + 1 < chunks.len() { 1 } else { 0 };
        if idx == 0 {
            out.push_str(&format!(
                "\x1b_Ga=T,f=100,i={KITTY_IMAGE_ID},c={cell_cols},r={cell_rows},C=1,q=2,m={more};{chunk}\x1b\\"
            ));
        } else {
            out.push_str(&format!("\x1b_Gm={more},q=2;{chunk}\x1b\\"));
        }
    }

    Ok(out)
}

pub fn clear_kitty_images<W: Write>(writer: &mut W) -> Result<()> {
    write!(writer, "\x1b_Ga=d,d=A,q=2\x1b\\")?;
    writer.flush()?;
    Ok(())
}

#[derive(Debug, Default)]
pub struct KittyPreviewCache {
    last_key: String,
    cached_escape: String,
    cached_stats: Option<ImageDiffStats>,
    cached_sig: String,
    before_img: Option<RgbaImage>,
    after_img: Option<RgbaImage>,
}

impl KittyPreviewCache {
    pub fn ensure_loaded(
        &mut self,
        file_sig: &str,
        before_abs: Option<&str>,
        after_abs: Option<&str>,
    ) -> &ImageDiffStats {
        if self.cached_sig != file_sig || self.cached_stats.is_none() {
            self.before_img = load_rgba(before_abs);
            self.after_img = load_rgba(after_abs);
            let stats =
                compute_image_diff_stats(self.before_img.as_ref(), self.after_img.as_ref());
            self.cached_sig = file_sig.to_string();
            self.cached_stats = Some(stats);
            self.last_key.clear();
            self.cached_escape.clear();
        }
        self.cached_stats.as_ref().unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare_escape(
        &mut self,
        file_sig: &str,
        before_abs: Option<&str>,
        after_abs: Option<&str>,
        mode: KittyImageMode,
        slider_pct: u8,
        show_boxes: bool,
        col_1idx: u16,
        row_1idx: u16,
        cell_cols: u16,
        cell_rows: u16,
    ) -> Option<&str> {
        self.ensure_loaded(file_sig, before_abs, after_abs);
        if self.before_img.is_none() && self.after_img.is_none() {
            return None;
        }
        let key = format!(
            "{file_sig}:{mode:?}:{slider_pct}:{show_boxes}:{col_1idx}:{row_1idx}:{cell_cols}:{cell_rows}"
        );
        if self.last_key != key || self.cached_escape.is_empty() {
            let stats = self.cached_stats.as_ref()?;
            let px_w = (cell_cols as u32) * 10;
            let px_h = (cell_rows as u32) * 20;
            let comp = render_composite_diff_image(
                self.before_img.as_ref(),
                self.after_img.as_ref(),
                stats,
                mode,
                slider_pct,
                show_boxes,
                px_w,
                px_h,
            );
            self.cached_escape =
                encode_kitty_png_escape(&comp, col_1idx, row_1idx, cell_cols, cell_rows).ok()?;
            self.last_key = key;
        }
        Some(&self.cached_escape)
    }
}
