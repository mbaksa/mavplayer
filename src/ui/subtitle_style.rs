//! Shared subtitle typography and rasterization settings.
//! Future Preferences fields can replace these defaults at the call sites.

pub const FONT_FAMILY: &str = "Open Sans";
pub const FONT_WEIGHT: gpui::FontWeight = gpui::FontWeight::NORMAL;

/// The base size is relative to video height, then adjusted by Settings::sub_scale.
pub const BASE_FONT_HEIGHT_RATIO: f32 = 0.052;
pub const MIN_FONT_SIZE: f32 = 12.0;
pub const MAX_FONT_SIZE: f32 = 96.0;
pub const LINE_HEIGHT_FACTOR: f32 = 1.22;
pub const BOX_HORIZONTAL_PADDING_FACTOR: f32 = 0.35;
pub const BOX_VERTICAL_PADDING_FACTOR: f32 = 0.10;
/// Visible separation between successive subtitle rows, in font-size units.
pub const LINE_GAP_FACTOR: f32 = 0.09;
pub const OUTLINE_PADDING_FACTOR: f32 = 0.12;
pub const OUTLINE_MIN_PADDING: f32 = 3.0;
pub const OUTLINE_STROKE_DIVISOR: f32 = 6.0;

pub fn font_size(video_height: f32, scale: f32) -> f32 {
    (video_height.max(100.0) * BASE_FONT_HEIGHT_RATIO * scale).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
}

pub fn line_height(font_size: f32) -> f32 {
    font_size * LINE_HEIGHT_FACTOR
}

pub fn outline_padding(font_size: f32) -> f32 {
    (font_size * OUTLINE_PADDING_FACTOR).max(OUTLINE_MIN_PADDING)
}
