//! EXPERIMENT: Vello CPU vector outline under native GPUI subtitle text.
//! This renders ONLY the black stroke. The native GPUI white glyphs stay crisp.
//! Text alignment uses the shared subtitle_style font in both engines.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::subtitle_style::{self, FONT_FAMILY, FONT_WEIGHT};
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use gpui::RenderImage;
use vello_cpu::{
    Pixmap, RenderContext, Resources,
    color::palette::css::BLACK,
    kurbo::{BezPath, Join, Stroke},
};

pub fn cache_key(text: &str, font: f32, width: f32, scale: f32) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    font.to_bits().hash(&mut hasher);
    width.to_bits().hash(&mut hasher);
    scale.to_bits().hash(&mut hasher);
    // Reserve the high bit to distinguish these entries from engine subtitle IDs.
    hasher.finish() | (1u64 << 63)
}

fn path_from_commands(commands: &[cosmic_text::Command], x: f64, y: f64) -> BezPath {
    let mut path = BezPath::new();
    for command in commands {
        match *command {
            cosmic_text::Command::MoveTo(p) => path.move_to((x + f64::from(p.x), y - f64::from(p.y))),
            cosmic_text::Command::LineTo(p) => path.line_to((x + f64::from(p.x), y - f64::from(p.y))),
            cosmic_text::Command::QuadTo(a, b) => {
                path.quad_to((x + f64::from(a.x), y - f64::from(a.y)), (x + f64::from(b.x), y - f64::from(b.y)))
            }
            cosmic_text::Command::CurveTo(a, b, c) => path.curve_to(
                (x + f64::from(a.x), y - f64::from(a.y)),
                (x + f64::from(b.x), y - f64::from(b.y)),
                (x + f64::from(c.x), y - f64::from(c.y)),
            ),
            cosmic_text::Command::Close => path.close_path(),
        }
    }
    path
}

pub fn stroke_image(text: &str, font: f32, logical_width: f32, dpi: f32) -> Option<Arc<RenderImage>> {
    let dpi = dpi.clamp(0.5, 4.0);
    let line_height = subtitle_style::line_height(font);
    // Extra transparent room for descenders and the lower half of the stroke.
    // Must match `outline_pad` in render.rs (in logical pixels).
    let outline_pad = subtitle_style::outline_padding(font);
    let pad_px = (outline_pad * dpi).ceil() as u16;
    let w = (logical_width * dpi).ceil().clamp(1.0, u16::MAX as f32) as u16;
    let h = ((line_height * dpi).ceil() + 2.0 * f32::from(pad_px)).clamp(1.0, u16::MAX as f32) as u16;
    if text.is_empty() || w == 0 || h == 0 {
        return None;
    }

    let mut fonts = FontSystem::new();
    let mut swash = SwashCache::new();
    let mut buffer = Buffer::new(&mut fonts, Metrics::new(font, line_height));

    let physical = {
        let mut buf = buffer.borrow_with(&mut fonts);
        buf.set_size(Some(logical_width), Some(line_height));
        buf.set_text(
            text,
            &Attrs::new().family(Family::Name(FONT_FAMILY)).weight(FONT_WEIGHT.into()),
            Shaping::Advanced,
        );
        // buf.set_text(text, &Attrs::new(), Shaping::Advanced);:with_join
        buf.shape_until_scroll(true);

        let physical = buf
            .layout_runs()
            .flat_map(|run| {
                // Center text to match GPUI's .text_center().
                let x = ((logical_width - run.line_w) * 0.5).max(0.0);
                run.glyphs.iter().map(|glyph| glyph.physical((x * dpi, run.line_y * dpi), dpi)).collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        physical
    };
    let mut ctx = RenderContext::new(w, h);
    ctx.set_paint(BLACK);
    ctx.set_stroke(
        Stroke::new((font * dpi / subtitle_style::OUTLINE_STROKE_DIVISOR).max(1.0) as f64).with_join(Join::Round),
    );
    for glyph in physical {
        if let Some(commands) = swash.get_outline_commands(&mut fonts, glyph.cache_key) {
            let path = path_from_commands(commands, glyph.x.into(), f64::from(glyph.y) + f64::from(pad_px));
            ctx.stroke_path(&path);
        }
    }
    ctx.flush();
    let mut pixmap = Pixmap::new(w, h);
    let mut resources = Resources::new();
    ctx.render(&mut pixmap, &mut resources);

    // For black-only pixels premultiplied and straight RGBA are identical.
    let mut data = Vec::with_capacity(w as usize * h as usize * 4);
    for p in pixmap.data() {
        data.extend_from_slice(&[0, 0, 0, p.a]);
    }
    let rgba = image::RgbaImage::from_raw(w.into(), h.into(), data)?;
    Some(Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(rgba)])))
}
