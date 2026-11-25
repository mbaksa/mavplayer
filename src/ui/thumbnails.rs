//! Shared timeline previews, backed by a cancellable local-file worker.

use crate::engine::thumbnails::{GenerationProgress, Thumbnail, ThumbnailSpec, cache::{Policy, Stats, load_or_generate, maintain}};
use crate::engine::{MediaSource, PlaybackState, Player, Status, TrackId};
use crate::settings::{Settings, TimelineVisualization};
use gpui::{AnyElement, Bounds, ContentMask, Corners, Hsla, ImageId, RenderImage, canvas, div, point, prelude::*, px, relative, size};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

#[derive(Clone, PartialEq, Eq)]
struct Request {
    path: PathBuf,
    stream: usize,
    directory: PathBuf,
    count: u32,
}

impl Request {
    fn from_state(st: &PlaybackState, settings: &Settings) -> Option<Self> {
        if !settings.timeline_thumbnails || (settings.playlist_thumbnails == crate::settings::PlaylistThumbnails::None && !settings.chapter_thumbnails && ![false, true].into_iter().any(|fullscreen| {
            settings.timeline_visualization(fullscreen) != TimelineVisualization::None || settings.timeline_hover_enabled(fullscreen)
        })) {
            return None;
        }
        Self::for_media(st, settings)
    }

    fn for_media(st: &PlaybackState, settings: &Settings) -> Option<Self> {
        if !settings.timeline_thumbnails || !matches!(st.status, Status::Playing | Status::Paused | Status::Ended) {
            return None;
        }
        let (Some(MediaSource::File(path)), Some(TrackId::Stream(stream)), Some(media)) =
            (&st.source, st.selected.video, &st.media)
        else {
            return None;
        };
        if !media.seekable || !media.duration.is_finite() || media.duration <= 0.0
            || !media.tracks.iter().any(|t| t.id == TrackId::Stream(stream) && !t.is_cover_art)
        {
            return None;
        }
        Some(Self { path: path.clone(), stream, directory: settings.thumbnail_cache_dir(), count: crate::engine::thumbnails::sample_count(media.duration) })
    }
}

struct Worker {
    cancel: Arc<AtomicBool>,
    generating: Arc<AtomicBool>,
    progress: Arc<GenerationProgress>,
    result: Receiver<anyhow::Result<WorkerImages>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct WorkerImages {
    thumbnails: Vec<Thumbnail>,
    barcodes: [image::RgbaImage; 2],
    generated: bool,
}

#[derive(Clone)]
pub(super) struct Preview {
    pub(super) time: f64,
    pub(super) image: Arc<RenderImage>,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl Preview {
    pub(super) fn from_bgra(t: Thumbnail) -> Self {
        let (width, height) = t.image.dimensions();
        Self { time: t.time, image: Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(t.image)])), width, height }
    }

    pub(super) fn element(&self, width: f32, height: f32) -> AnyElement {
        let image = self.image.clone();
        let aspect = self.width as f32 / self.height as f32;
        canvas(|_, _, _| (), move |bounds, _, window, _| {
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let tw = w.min(h * aspect);
            let th = h.min(w / aspect);
            let target = Bounds::new(point(bounds.origin.x + px((w - tw) * 0.5), bounds.origin.y + px((h - th) * 0.5)), size(px(tw), px(th)));
            let _ = window.paint_image(target, Corners::default(), image.clone(), 0, false);
        }).w(px(width)).h(px(height)).flex_none().into_any_element()
    }
}

#[derive(Default)]
pub(super) struct TimelineThumbnails {
    request: Option<Request>,
    worker: Option<Worker>,
    attempted: bool,
    previews: Vec<Preview>,
    barcodes: Option<[Arc<RenderImage>; 2]>,
    policy: Option<Policy>,
    maintenance: Option<Receiver<anyhow::Result<Stats>>>,
    maintenance_pending: bool,
    clear_pending: Option<Policy>,
    maintenance_error: bool,
    stats: Option<Stats>,
    completed_path: Option<(PathBuf, bool)>,
}

impl TimelineThumbnails {
    pub(super) fn reset(&mut self) -> Vec<ImageId> {
        if let Some(worker) = &self.worker {
            worker.cancel.store(true, Ordering::Relaxed);
        }
        self.worker = None;
        self.request = None;
        self.attempted = false;
        self.take_images()
    }

    fn take_images(&mut self) -> Vec<ImageId> {
        let mut retired: Vec<_> = self.previews.drain(..).map(|p| p.image.id).collect();
        if let Some(barcodes) = self.barcodes.take() {
            retired.extend(barcodes.into_iter().map(|image| image.id));
        }
        retired
    }

    pub(super) fn progress(&self) -> Option<Arc<GenerationProgress>> {
        self.worker.as_ref().filter(|_| self.generating()).map(|w| w.progress.clone())
    }

    pub(super) fn generating(&self) -> bool {
        self.worker.as_ref().is_some_and(|w| w.generating.load(Ordering::Relaxed) && !w.cancel.load(Ordering::Relaxed))
    }

    pub(super) fn can_regenerate(&self, st: &PlaybackState, settings: &Settings) -> bool {
        Request::for_media(st, settings).is_some()
    }

    pub(super) fn busy_for(&self, path: &std::path::Path, stream: Option<usize>) -> bool {
        self.worker.is_some() && self.request.as_ref().is_some_and(|r| r.path.as_path() == path && stream.is_none_or(|s| s == r.stream))
    }

    /// Called from the existing UI tick and engine events. Never blocks on extraction.
    pub(super) fn update(&mut self, player: &Player, settings: &Settings) -> (bool, Vec<ImageId>) {
        let mut changed = self.update_cache(settings);
        let request = player.with_state(|st| Request::from_state(st, settings));
        let request_changed = self.request != request;
        changed |= request_changed;
        let mut retired = if request_changed {
            let retired = self.reset();
            self.request = request;
            retired
        } else {
            Vec::new()
        };
        if let Some(worker) = &self.worker {
            let result = match worker.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return (changed || self.generating(), retired),
                Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("thumbnail worker disconnected")),
            };
            if !worker.cancel.load(Ordering::Relaxed) {
                match result {
                    Ok(images) => {
                        retired.extend(self.take_images());
                        self.barcodes = Some(images.barcodes.map(|image| Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(image)]))));
                        self.previews = images.thumbnails.into_iter().map(|t| {
                            let (width, height) = t.image.dimensions();
                            Preview {
                                time: t.time,
                                image: Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(t.image)])),
                                width,
                                height,
                            }
                        }).collect();
                        self.maintenance_pending = true;
                        self.completed_path = self.request.as_ref().map(|r| (r.path.clone(), images.generated));
                    }
                    Err(err) => log::debug!("timeline thumbnails: {err}"),
                }
            }
            self.worker = None;
            changed = true;
        }
        if let Some(request) = &self.request
            && !self.attempted
        {
            self.attempted = true;
            let request = request.clone();
            let config = settings.engine_config();
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = cancel.clone();
            let generating = Arc::new(AtomicBool::new(false));
            let generation_flag = generating.clone();
            let progress = Arc::new(GenerationProgress::new(request.count));
            let generation_progress = progress.clone();
            let duration = player.with_state(|st| st.media.as_ref().map_or(0.0, |m| m.duration));
            let (tx, result) = std::sync::mpsc::channel();
            match std::thread::Builder::new().name("timeline-thumbnails".into()).spawn(move || {
                let spec = ThumbnailSpec { stream: request.stream, count: request.count, width: 240, height: 135 };
                let thumbnails = load_or_generate(&request.path, &config, spec, &request.directory, &stop, false, &generation_flag, &generation_progress).map(|mut thumbnails| {
                    // Extracted and cached images are RGBA; GPUI's RenderImage expects BGRA.
                    for thumbnail in &mut thumbnails {
                        let pixels: &mut [u8] = thumbnail.image.as_mut();
                        for pixel in pixels.as_chunks_mut::<4>().0 {
                            pixel.swap(0, 2);
                        }
                    }
                    let barcodes = super::barcode::build(&thumbnails, duration);
                    WorkerImages { thumbnails, barcodes, generated: generation_flag.load(Ordering::Relaxed) }
                });
                let _ = tx.send(thumbnails);
            }) {
                Ok(_) => self.worker = Some(Worker { cancel, generating, progress, result }),
                Err(err) => { log::debug!("could not start thumbnail worker: {err}"); },
            }
        }
        (changed || self.generating(), retired)
    }

    fn update_cache(&mut self, settings: &Settings) -> bool {
        let policy = Policy { directory: settings.thumbnail_cache_dir(), days: settings.thumbnail_cache_days, mb: settings.thumbnail_cache_mb };
        let mut changed = false;
        if self.policy.as_ref() != Some(&policy) {
            self.policy = Some(policy.clone());
            self.stats = None;
            self.maintenance_error = false;
            self.maintenance_pending = true;
            changed = true;
        }
        if let Some(receiver) = &self.maintenance {
            match receiver.try_recv() {
                Ok(result) => {
                    self.maintenance = None;
                    match result {
                        Ok(stats) if !self.maintenance_pending => { self.stats = Some(stats); self.maintenance_error = false; }
                        Ok(_) => {}
                        Err(err) => { self.maintenance_error = true; log::debug!("thumbnail cache maintenance: {err}"); }
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => return changed,
                Err(TryRecvError::Disconnected) => { self.maintenance = None; self.maintenance_error = true; changed = true; }
            }
        }
        if self.maintenance_pending {
            self.maintenance_pending = false;
            let clear = self.clear_pending.take();
            let clear_requested = clear.is_some();
            let job_policy = clear.unwrap_or_else(|| policy.clone());
            if job_policy != policy {
                self.maintenance_pending = true;
            }
            let (tx, rx) = std::sync::mpsc::channel();
            match std::thread::Builder::new().name("thumbnail-cache".into()).spawn(move || {
                let _ = tx.send(maintain(&job_policy, clear_requested));
            }) {
                Ok(_) => self.maintenance = Some(rx),
                Err(err) => { self.maintenance_error = true; log::debug!("could not start cache maintenance: {err}"); }
            }
        }
        changed
    }

    pub(super) fn take_completed_path(&mut self) -> Option<(PathBuf, bool)> { self.completed_path.take() }

    pub(super) fn refresh_cache_stats(&mut self) { self.maintenance_pending = true; }

    pub(super) fn clear_cache(&mut self, settings: &Settings) {
        self.clear_pending = Some(Policy { directory: settings.thumbnail_cache_dir(), days: settings.thumbnail_cache_days, mb: settings.thumbnail_cache_mb });
        // Keep already loaded images; prevent an in-flight extraction from
        // writing the entry back after the user clears the cache.
        if let Some(worker) = &self.worker {
            worker.cancel.store(true, Ordering::Relaxed);
        }
        self.maintenance_error = false;
        self.maintenance_pending = true;
        self.stats = None;
    }

    pub(super) fn cache_label(&self) -> String {
        if self.maintenance_error {
            "Cache unavailable".into()
        } else if let Some(stats) = self.stats {
            format!("{:.1} MB · {} cached entries", stats.bytes as f64 / (1024.0 * 1024.0), stats.count)
        } else {
            "Calculating…".into()
        }
    }

    pub(super) fn has_previews(&self, st: &PlaybackState, settings: &Settings, fullscreen: bool) -> bool {
        settings.timeline_thumbnails && settings.timeline_visualization(fullscreen) != TimelineVisualization::None
            && !self.previews.is_empty() && self.request == Request::from_state(st, settings)
    }

    pub(super) fn visualization(&self, mode: TimelineVisualization, duration: f64, height: f32, opacity: f32) -> AnyElement {
        let element = match mode {
            TimelineVisualization::Filmstrip => self.filmstrip(duration, height),
            TimelineVisualization::BarcodeAverage | TimelineVisualization::BarcodeCompressed => {
                let index = usize::from(mode == TimelineVisualization::BarcodeCompressed);
                let image = self.barcodes.as_ref().map(|images| images[index].clone());
                canvas(|_, _, _| (), move |bounds, _, window, _| {
                    if let Some(image) = &image {
                        let _ = window.paint_image(bounds, Corners::default(), image.clone(), 0, false);
                    }
                }).absolute().top_0().left_0().w_full().h(px(height)).into_any_element()
            }
            TimelineVisualization::None => div().into_any_element(),
        };
        div().absolute().top_0().left_0().w_full().h(px(height)).opacity(opacity).child(element).into_any_element()
    }

    fn filmstrip(&self, duration: f64, filmstrip_height: f32) -> AnyElement {
        let previews = self.previews.clone();
        canvas(
            |_, _, _| (),
            move |clip, _, window, _| {
                let bounds = clip;
                let width = f32::from(bounds.size.width);
                let height = f32::from(bounds.size.height);
                if width <= 0.0 || height <= 0.0 || previews.is_empty() {
                    return;
                }
                window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                    let aspect = previews[0].width as f32 / previews[0].height as f32;
                    let count = ((width / (height * aspect)).round() as usize).clamp(1, 1024);
                    let cell = width / count as f32;
                    for i in 0..count {
                        let time = (i as f64 + 0.5) / count as f64 * duration;
                        let Some(preview) = previews.iter().min_by(|a, b| {
                            (a.time - time).abs().total_cmp(&(b.time - time).abs())
                        }) else { continue };
                        let scale = ((cell - 1.0).max(0.0) / preview.width as f32).min(height / preview.height as f32);
                        let w = preview.width as f32 * scale;
                        let h = preview.height as f32 * scale;
                        let target = Bounds::new(
                            point(bounds.origin.x + px(i as f32 * cell + (cell - w) * 0.5), bounds.origin.y + px((height - h) * 0.5)),
                            size(px(w), px(h)),
                        );
                        if target.intersect(&clip).size.width > px(0.) {
                            let _ = window.paint_image(target, Corners::default(), preview.image.clone(), 0, false);
                        }
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .w_full()
        .h(px(filmstrip_height))
        .into_any_element()
    }

    pub(super) fn for_playlist(&self, path: &std::path::Path, stream: usize, count: u32, time: f64) -> Option<&Preview> {
        let request = self.request.as_ref()?;
        if request.path.as_path() != path || request.stream != stream || request.count != count { return None; }
        self.previews.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
    }

    pub(super) fn for_chapter(&self, st: &PlaybackState, settings: &Settings, start: f64, end: f64) -> Option<&Preview> {
        if !settings.timeline_thumbnails || !settings.chapter_thumbnails || self.request != Request::from_state(st, settings) { return None; }
        let target = start + (end - start) * 0.25;
        self.previews.iter().filter(|p| p.time >= start && p.time < end).min_by(|a, b| (a.time - target).abs().total_cmp(&(b.time - target).abs()))
    }

    pub(super) fn nearest(&self, st: &PlaybackState, settings: &Settings, time: f64, fullscreen: bool) -> Option<&Preview> {
        // Do not show the previous file's preview before the next UI tick.
        if !settings.timeline_thumbnails || !settings.timeline_hover_enabled(fullscreen) || self.request != Request::from_state(st, settings) {
            return None;
        }
        self.previews.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
    }
}

pub(super) fn filmstrip_height(settings: &Settings, window: &gpui::Window, cx: &mut gpui::Context<super::player::PlayerView>) -> f32 {
    let max = settings.visualization_height(window.is_fullscreen()) as f32;
    let min = settings.timeline_filmstrip_min_height.clamp(12, max as u32) as f32;
    if !settings.timeline_adaptive || window.is_fullscreen() {
        return max;
    }
    let Some(display) = window.display(cx).or_else(|| cx.primary_display()) else {
        return max;
    };
    let width = f32::from(display.bounds().size.width);
    if width <= 0.0 {
        return max;
    }
    let ratio = f32::from(window.viewport_size().width) / width;
    min + (max - min) * ((ratio - 0.25) / 0.5).clamp(0.0, 1.0)
}

pub(super) fn hover_preview(fraction: f32, preview: &Preview, background: Hsla, extra: f32, label: String) -> AnyElement {
    let image = preview.image.clone();
    let w = preview.width as f32;
    let h = preview.height as f32;
    div()
        .absolute()
        .bottom(px(22. + extra))
        .left(relative(fraction))
        .ml(px(-(w + 4.) * fraction))
        .p(px(2.))
        .w(px(w + 4.))
        .flex().flex_col().items_center()
        .bg(background)
        .rounded(px(4.))
        .child(
            canvas(
                |bounds, _, _| bounds,
                move |bounds, _, window, _| {
                    let _ = window.paint_image(bounds, Corners::default(), image.clone(), 0, false);
                },
            )
            .w(px(w))
            .h(px(h)),
        )
        .child(div().w_full().px_1().py(px(3.)).text_center().text_size(px(12.)).text_color(gpui::white()).child(label))
        .into_any_element()
}
