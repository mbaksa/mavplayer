//! Timeline visualizations derived from cached BGRA samples, not full video analysis.
use crate::engine::thumbnails::Thumbnail;
use crate::settings::TimelineVisualization as Mode;
use image::{Rgba, RgbaImage};

const WIDTH: u32 = 480;
const HEIGHT: u32 = 32;

fn luma(p: &Rgba<u8>) -> u8 {
    ((722 * u32::from(p[0]) + 7152 * u32::from(p[1]) + 2126 * u32::from(p[2]) + 5000) / 10000) as u8
}

fn gray(v: u8) -> Rgba<u8> { Rgba([v, v, v, 255]) }

fn mean(pixels: impl Iterator<Item = Rgba<u8>>) -> Rgba<u8> {
    let mut sum = [0u64; 3];
    let mut n = 0u64;
    for p in pixels { for c in 0..3 { sum[c] += u64::from(p[c]); } n += 1; }
    Rgba([(sum[0] / n.max(1)) as u8, (sum[1] / n.max(1)) as u8, (sum[2] / n.max(1)) as u8, 255])
}

fn percentile(hist: &[u32; 256], n: u32, percent: u32) -> u8 {
    let rank = (n.saturating_sub(1) * percent) / 100;
    let mut sum = 0;
    for (i, count) in hist.iter().enumerate() { sum += count; if sum > rank { return i as u8; } }
    0
}

fn column(mode: Mode, image: &RgbaImage, previous: Option<&RgbaImage>) -> RgbaImage {
    let height = match mode { Mode::BarcodeCompressed | Mode::LumaRange | Mode::DominantColors => HEIGHT, _ => 1 };
    let mut out = RgbaImage::new(1, height);
    let n = image.width() * image.height();
    if n == 0 { return out; }
    match mode {
        Mode::BarcodeAverage => out.put_pixel(0, 0, mean(image.pixels().copied())),
        Mode::BarcodeCompressed => return image::imageops::resize(image, 1, HEIGHT, image::imageops::FilterType::Triangle),
        Mode::Luma => {
            let sum: u64 = image.pixels().map(|p| u64::from(luma(p))).sum();
            out.put_pixel(0, 0, gray((sum / u64::from(n)) as u8));
        }
        Mode::LumaRange => {
            let mut hist = [0u32; 256];
            for p in image.pixels() { hist[luma(p) as usize] += 1; }
            let lo = percentile(&hist, n, 5);
            let mid = percentile(&hist, n, 50);
            let hi = percentile(&hist, n, 95);
            for y in 0..HEIGHT {
                let level = ((HEIGHT - 1 - y) * 255 / (HEIGHT - 1)) as u8;
                let value = if level.abs_diff(mid) <= 4 { 255 } else if level >= lo && level <= hi { 145 } else { 20 };
                out.put_pixel(0, y, gray(value));
            }
        }
        Mode::Saturation => {
            let sum: u64 = image.pixels().map(|p| {
                let max = p[0].max(p[1]).max(p[2]);
                let min = p[0].min(p[1]).min(p[2]);
                if max == 0 { 0 } else { 255 * u64::from(max - min) / u64::from(max) }
            }).sum();
            out.put_pixel(0, 0, gray((sum / u64::from(n)) as u8));
        }
        Mode::DominantColors => {
            // Quantized RGB histogram; use actual mean of each populated bucket.
            let mut buckets = [(0u64, [0u64; 3]); 512];
            for p in image.pixels() {
                let i = ((p[0] as usize >> 5) << 6) | ((p[1] as usize >> 5) << 3) | (p[2] as usize >> 5);
                buckets[i].0 += 1;
                for (sum, value) in buckets[i].1.iter_mut().zip(p.0) { *sum += u64::from(value); }
            }
            buckets.sort_unstable_by_key(|b| std::cmp::Reverse(b.0));
            let top = &buckets[..4];
            let total: u64 = top.iter().map(|b| b.0).sum();
            for y in 0..HEIGHT {
                let target = u64::from(y) * total / u64::from(HEIGHT);
                let mut sum = 0;
                for (count, colors) in top {
                    sum += count;
                    if sum > target {
                        out.put_pixel(0, y, Rgba([(colors[0] / *count) as u8, (colors[1] / *count) as u8, (colors[2] / *count) as u8, 255]));
                        break;
                    }
                }
            }
        }
        Mode::VisualComplexity => {
            let small = image::imageops::resize(image, 64, 36, image::imageops::FilterType::Triangle);
            let mut sum = 0u64;
            for y in 0..35 { for x in 0..63 {
                let v = luma(small.get_pixel(x, y));
                sum += u64::from(v.abs_diff(luma(small.get_pixel(x + 1, y)))) + u64::from(v.abs_diff(luma(small.get_pixel(x, y + 1))));
            } }
            out.put_pixel(0, 0, gray((sum * 4 / (35 * 63 * 2)).min(255) as u8));
        }
        Mode::FrameDifference => {
            let value = previous.map_or(0, |prev| {
                let a = image::imageops::resize(prev, 32, 18, image::imageops::FilterType::Triangle);
                let b = image::imageops::resize(image, 32, 18, image::imageops::FilterType::Triangle);
                let sum: u64 = a.pixels().zip(b.pixels()).map(|(a, b)| (0..3).map(|c| u64::from(a[c].abs_diff(b[c]))).sum::<u64>()).sum();
                (sum / (32 * 18 * 3)) as u8
            });
            out.put_pixel(0, 0, gray(value));
        }
        Mode::None | Mode::Filmstrip | Mode::Waveform | Mode::FilmstripWaveform => {}
    }
    out
}

pub(super) fn build(mode: Mode, thumbnails: &[Thumbnail], duration: f64) -> RgbaImage {
    let columns: Vec<_> = thumbnails.iter().enumerate().map(|(i, t)| column(mode, &t.image, i.checked_sub(1).map(|p| &thumbnails[p].image))).collect();
    let height = columns.first().map_or(1, RgbaImage::height);
    let mut out = RgbaImage::new(WIDTH, height);
    for x in 0..WIDTH {
        let time = duration * (f64::from(x) + 0.5) / f64::from(WIDTH);
        if let Some((index, _)) = thumbnails.iter().enumerate().min_by(|(_, a), (_, b)| (a.time - time).abs().total_cmp(&(b.time - time).abs())) {
            for y in 0..height { out.put_pixel(x, y, *columns[index].get_pixel(0, y)); }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn barcode_uses_media_time_and_preserves_vertical_colors() {
        let mut image = RgbaImage::from_pixel(2, 32, Rgba([20, 40, 60, 255]));
        for y in 16..32 { for x in 0..2 { image.put_pixel(x, y, Rgba([100, 120, 140, 255])); } }
        let samples = [Thumbnail { time: 1.0, image }, Thumbnail { time: 9.0, image: RgbaImage::from_pixel(2, 32, Rgba([200, 10, 5, 255])) }];
        let avg = build(Mode::BarcodeAverage, &samples, 10.0);
        let compressed = build(Mode::BarcodeCompressed, &samples, 10.0);
        assert_eq!(*avg.get_pixel(0, 0), Rgba([60, 80, 100, 255]));
        assert_eq!(*avg.get_pixel(WIDTH - 1, 0), Rgba([200, 10, 5, 255]));
        assert_eq!(*compressed.get_pixel(0, 0), Rgba([20, 40, 60, 255]));
        assert_eq!(*compressed.get_pixel(0, HEIGHT - 1), Rgba([100, 120, 140, 255]));
    }
    #[test]
    fn barcode_metrics_distinguish_dark_light_and_saturated_frames() {
        let samples = [Thumbnail { time: 0.0, image: RgbaImage::from_pixel(2, 2, gray(0)) }, Thumbnail { time: 1.0, image: RgbaImage::from_pixel(2, 2, gray(255)) }];
        let l = build(Mode::Luma, &samples, 1.0);
        assert_eq!(*l.get_pixel(0, 0), gray(0));
        assert_eq!(*l.get_pixel(WIDTH - 1, 0), gray(255));
        let diff = build(Mode::FrameDifference, &samples, 1.0);
        assert_eq!(*diff.get_pixel(WIDTH - 1, 0), gray(255));
        let saturated = RgbaImage::from_pixel(2, 2, Rgba([0, 0, 255, 255]));
        assert_eq!(*column(Mode::Saturation, &saturated, None).get_pixel(0, 0), gray(255));
        assert_eq!(*column(Mode::DominantColors, &saturated, None).get_pixel(0, 0), Rgba([0, 0, 255, 255]));
    }
}
