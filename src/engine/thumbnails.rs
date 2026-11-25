//! Independent, cancellable thumbnail extraction for local video files.
//! Timestamps use the same media-relative clock as the playback engine.

use super::av::{self, Frame, Packet, ff};
use super::{EngineConfig, MediaSource, ScaleQuality};
use image::RgbaImage;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Debug)]
pub struct Thumbnail {
    pub time: f64,
    pub image: RgbaImage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThumbnailSpec {
    pub stream: usize,
    pub count: u32,
    pub width: u32,
    pub height: u32,
}

pub const MAX_THUMBNAILS: u32 = 480;

/// Work units are requested sample positions, not necessarily distinct frames.
/// The final percentage is reserved for cache publication.
pub struct GenerationProgress {
    total: AtomicU32,
    done: AtomicU32,
    extracted: AtomicU32,
}

impl GenerationProgress {
    pub fn new(count: u32) -> Self {
        Self { total: AtomicU32::new(count), done: AtomicU32::new(0), extracted: AtomicU32::new(0) }
    }

    pub(crate) fn set_total(&self, count: u32) { self.total.store(count, Ordering::Relaxed); }

    pub fn snapshot(&self) -> (u32, u32, u32) {
        let total = self.total.load(Ordering::Relaxed).max(1);
        let done = self.done.load(Ordering::Relaxed).min(total);
        (total, (done * 100 / total).min(99), self.extracted.load(Ordering::Relaxed))
    }
}

pub fn sample_count(duration: f64) -> u32 {
    if duration <= 60.0 { 20 }
    else if duration <= 600.0 { 60 }
    else if duration <= 3600.0 { 120 }
    else { 240 }
}

impl ThumbnailSpec {
    pub fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!((1..=MAX_THUMBNAILS).contains(&self.count), "invalid thumbnail count");
        anyhow::ensure!((1..=512).contains(&self.width) && (1..=512).contains(&self.height), "invalid thumbnail size");
        anyhow::ensure!(u64::from(self.count) * u64::from(self.width) * u64::from(self.height) <= 16_000_000, "thumbnail set too large");
        Ok(())
    }
}

#[allow(dead_code)] // Also useful for consumers of the raw thumbnail set.
pub fn nearest(thumbnails: &[Thumbnail], time: f64) -> Option<&Thumbnail> {
    thumbnails.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
}

fn cancelled(stop: &AtomicBool) -> anyhow::Result<()> {
    anyhow::ensure!(!stop.load(Ordering::Relaxed), "thumbnail generation cancelled");
    Ok(())
}

/// Call on a dedicated worker, never the UI or playback thread.
/// `stream` is the selected embedded video stream index, not its ordinal.
/// Samples are approximate; repeated keyframes are deduplicated.
pub fn generate(
    path: &Path,
    cfg: &EngineConfig,
    spec: ThumbnailSpec,
    stop: &AtomicBool,
    progress: &GenerationProgress,
) -> anyhow::Result<Vec<Thumbnail>> {
    spec.validate()?;
    cancelled(stop)?;
    let source = MediaSource::File(path.to_path_buf());
    let ic = super::demux::open_input(&source, cfg, None)?;
    let info = super::demux::build_media_info(&ic, &source);
    anyhow::ensure!(info.seekable && info.duration.is_finite() && info.duration > 0.0, "media is not seekable");
    anyhow::ensure!(spec.stream < ic.nb_streams(), "invalid video stream");
    let stream = ic.stream(spec.stream);
    unsafe {
        anyhow::ensure!((*(*stream).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_VIDEO, "not a video stream");
        anyhow::ensure!((*stream).disposition & ff::AV_DISPOSITION_ATTACHED_PIC == 0, "cover art is not video");
        for i in 0..ic.nb_streams() {
            (*ic.stream(i)).discard = if i == spec.stream {
                ff::AVDiscard::AVDISCARD_DEFAULT
            } else {
                ff::AVDiscard::AVDISCARD_ALL
            };
        }
    }
    let si = super::video::StreamInfo::from_stream(&ic, spec.stream);
    let tb = av::q2d(si.time_base);
    anyhow::ensure!(tb.is_finite() && tb > 0.0, "invalid video time base");
    let codec = super::decoder::open_codec(stream, 1, None, None)?;
    let mut converter = super::present::Converter::new();
    let mut packet = Packet::new();
    let mut frame = Frame::new();
    let mut out: Vec<Thumbnail> = Vec::new();
    unsafe {
        // HEVC random access can emit decoder diagnostics even when a usable
        // preview is produced. Keep them at info level in this decoder only.
        (*codec.ctx.0).log_level_offset = 16;
        let mut targets: Vec<(f64, Option<(f64, f64)>)> = (0..spec.count).map(|i| {
            (info.duration * (f64::from(i) + 0.5) / f64::from(spec.count), None)
        }).collect();
        for chapter in &info.chapters {
            let start = chapter.start.max(0.0);
            let end = chapter.end.min(info.duration);
            if start.is_finite() && end.is_finite() && end > start
                && targets.len() < MAX_THUMBNAILS as usize
                && (targets.len() as u64 + 1) * u64::from(spec.width) * u64::from(spec.height) <= 16_000_000
            {
                targets.push((start + (end - start) * 0.25, Some((start, end))));
            }
        }
        let total = targets.len() as u32;
        progress.total.store(total, Ordering::Relaxed);
        for (index, (target, chapter)) in targets.into_iter().enumerate() {
            progress.done.store(index as u32, Ordering::Relaxed);
            cancelled(stop)?;
            if let Some((start, end)) = chapter
                && out.iter().any(|t| t.time >= start && t.time < end)
            {
                continue;
            }
            let precise = info.duration <= 60.0 || chapter.is_some();
            (*codec.ctx.0).skip_frame = if precise { ff::AVDiscard::AVDISCARD_DEFAULT } else { ff::AVDiscard::AVDISCARD_NONKEY };
            let ts = ((target + info.start_time) / tb).round() as i64;
            if ff::av_seek_frame(ic.0, spec.stream as i32, ts, ff::AVSEEK_FLAG_BACKWARD) < 0 {
                continue;
            }
            ff::avcodec_flush_buffers(codec.ctx.0);
            packet.unref();
            frame.unref();
            // Bound the work on malformed files or unusual GOP structures.
            let mut draining = false;
            for _ in 0..4096 {
                cancelled(stop)?;
                let r = ff::avcodec_receive_frame(codec.ctx.0, frame.0);
                if r >= 0 {
                    let pts = frame.raw().best_effort_timestamp;
                    let time = av::ts_to_secs(pts, si.time_base) - info.start_time;
                    if time.is_finite() && time >= -0.1 && time <= info.duration + 0.1 {
                        let time = time.clamp(0.0, info.duration);
                        let acceptable = if let Some((start, end)) = chapter {
                            time >= start && time < end
                        } else {
                            !precise || time >= target
                        };
                        if !acceptable {
                            frame.unref();
                            if chapter.is_some_and(|(_, end)| time >= end) { break; }
                            continue;
                        }
                        if !out.iter().any(|t| (t.time - time).abs() < 0.000_001)
                            && let Some(image) = convert(&mut converter, &frame, &si, spec)
                        {
                            out.push(Thumbnail { time, image });
                            progress.extracted.store(out.len() as u32, Ordering::Relaxed);
                        }
                        break;
                    }
                    frame.unref();
                    continue;
                }
                if r != av::EAGAIN || draining {
                    break;
                }
                packet.unref();
                let r = ff::av_read_frame(ic.0, packet.0);
                if r < 0 {
                    draining = true;
                    if ff::avcodec_send_packet(codec.ctx.0, std::ptr::null()) < 0 {
                        break;
                    }
                } else if packet.stream_index() == spec.stream as i32 {
                    // receive_frame returned EAGAIN, so the decoder can accept input.
                    if ff::avcodec_send_packet(codec.ctx.0, packet.0) < 0 {
                        break;
                    }
                }
            }
        }
        progress.done.store(total, Ordering::Relaxed);
    }
    cancelled(stop)?;
    out.sort_by(|a, b| a.time.total_cmp(&b.time));
    anyhow::ensure!(!out.is_empty(), "no thumbnails decoded");
    Ok(out)
}

fn convert(
    converter: &mut super::present::Converter,
    frame: &Frame,
    si: &super::video::StreamInfo,
    spec: ThumbnailSpec,
) -> Option<RgbaImage> {
    if frame.width() <= 0 || frame.height() <= 0 {
        return None;
    }
    let sar = frame.raw().sample_aspect_ratio;
    let sar = if sar.num > 0 && sar.den > 0 { av::q2d(sar) } else { si.sar };
    let aspect = frame.width() as f64 * sar / frame.height() as f64;
    if !aspect.is_finite() || aspect <= 0.0 {
        return None;
    }
    let (mw, mh) = if matches!(si.rotation, 90 | 270) { (spec.height, spec.width) } else { (spec.width, spec.height) };
    let h = (f64::from(mw) / aspect).min(f64::from(mh));
    let w = (h * aspect).round().clamp(1.0, f64::from(mw)) as u32;
    let h = h.round().clamp(1.0, f64::from(mh)) as u32;
    let mut pixels = converter.convert(frame, w, h, 0, ScaleQuality::Linear)?;
    for p in pixels.as_chunks_mut::<4>().0 {
        p.swap(0, 2); // BGRA -> RGBA
        p[3] = 255;
    }
    let image = RgbaImage::from_raw(w, h, pixels)?;
    Some(match si.rotation {
        90 => image::imageops::rotate90(&image),
        180 => image::imageops::rotate180(&image),
        270 => image::imageops::rotate270(&image),
        _ => image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_count_duration_boundaries() {
        for (duration, count) in [(10.0, 20), (60.0, 20), (60.01, 60), (600.0, 60), (600.01, 120), (3600.0, 120), (3600.01, 240)] {
            assert_eq!(sample_count(duration), count);
        }
    }

    #[test]
    fn generation_progress_counts_samples_and_unique_frames() {
        let progress = GenerationProgress::new(20);
        assert_eq!(progress.snapshot(), (20, 0, 0));
        progress.total.store(25, Ordering::Relaxed);
        progress.done.store(10, Ordering::Relaxed);
        progress.extracted.store(7, Ordering::Relaxed);
        assert_eq!(progress.snapshot(), (25, 40, 7));
        progress.done.store(25, Ordering::Relaxed);
        assert_eq!(progress.snapshot(), (25, 99, 7));
    }

    #[test]
    fn nearest_uses_actual_timestamp() {
        let thumbs = [Thumbnail { time: 2.0, image: RgbaImage::new(1, 1) }, Thumbnail { time: 9.0, image: RgbaImage::new(1, 1) }];
        assert_eq!(nearest(&thumbs, 6.0).unwrap().time, 9.0);
        assert!(nearest(&[], 0.0).is_none());
    }
}

/// Persistent JPEG cache shared by filmstrip and hover preview.
pub mod cache;
