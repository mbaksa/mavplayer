//! Build two timeline textures from the existing BGRA thumbnail samples.
//! No additional media reads, decoding or disk cache are needed.

use crate::engine::thumbnails::Thumbnail;
use image::{Rgba, RgbaImage};

const WIDTH: u32 = 480;
const HEIGHT: u32 = 32;

pub(super) fn build(thumbnails: &[Thumbnail], duration: f64) -> [RgbaImage; 2] {
    let mut average = RgbaImage::new(WIDTH, 1);
    let mut compressed = RgbaImage::new(WIDTH, HEIGHT);
    // Calculate each sample once, even if it supplies several columns.
    let colors: Vec<_> = thumbnails.iter().map(|t| {
        let mut sum = [0u64; 3];
        for pixel in t.image.pixels() {
            for c in 0..3 { sum[c] += u64::from(pixel[c]); }
        }
        let n = u64::from(t.image.width()) * u64::from(t.image.height());
        let avg = Rgba([(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, 255]);
        let column = image::imageops::resize(&t.image, 1, HEIGHT, image::imageops::FilterType::Triangle);
        (avg, column)
    }).collect();
    for x in 0..WIDTH {
        let time = duration * (f64::from(x) + 0.5) / f64::from(WIDTH);
        if let Some((index, _)) = thumbnails.iter().enumerate().min_by(|(_, a), (_, b)| {
            (a.time - time).abs().total_cmp(&(b.time - time).abs())
        }) {
            let (avg, column) = &colors[index];
            average.put_pixel(x, 0, *avg);
            for y in 0..HEIGHT { compressed.put_pixel(x, y, *column.get_pixel(0, y)); }
        }
    }
    [average, compressed]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barcode_uses_media_time_and_preserves_vertical_colors() {
        let mut image = RgbaImage::from_pixel(2, 32, Rgba([20, 40, 60, 255]));
        for y in 16..32 { for x in 0..2 { image.put_pixel(x, y, Rgba([100, 120, 140, 255])); } }
        let samples = [Thumbnail { time: 1.0, image }, Thumbnail { time: 9.0, image: RgbaImage::from_pixel(2, 32, Rgba([200, 10, 5, 255])) }];
        let [avg, compressed] = build(&samples, 10.0);
        assert_eq!(*avg.get_pixel(0, 0), Rgba([60, 80, 100, 255]));
        assert_eq!(*avg.get_pixel(WIDTH - 1, 0), Rgba([200, 10, 5, 255]));
        assert_eq!(*compressed.get_pixel(0, 0), Rgba([20, 40, 60, 255]));
        assert_eq!(*compressed.get_pixel(0, HEIGHT - 1), Rgba([100, 120, 140, 255]));
    }
}
