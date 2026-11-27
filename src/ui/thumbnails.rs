//! Shared timeline previews, backed by a cancellable local-file worker.

use crate::engine::thumbnails::{
    GenerationProgress, Thumbnail, ThumbnailSpec,
    cache::{GenerationControl, Policy, Stats, load_cached, load_or_generate, maintain},
};
use crate::engine::{MediaSource, PlaybackState, Player, Status, TrackId};
use crate::settings::{Settings, TimelineVisualization};
use gpui::{
    AnyElement, Bounds, ContentMask, Corners, Hsla, ImageId, RenderImage, canvas, div, point, prelude::*, px, relative,
    size,
};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, OnceLock};

#[derive(Clone, PartialEq, Eq)]
struct Request {
    path: PathBuf,
    stream: usize,
    directory: PathBuf,
    count: u32,
    auto_generate: bool,
}

impl Request {
    fn from_state(st: &PlaybackState, settings: &Settings) -> Option<Self> {
        if !settings.timeline_thumbnails
            || (!settings.filmstrip_auto_playback
                && settings.playlist_thumbnails == crate::settings::PlaylistThumbnails::None
                && !settings.chapter_thumbnails
                && ![false, true].into_iter().any(|fullscreen| {
                    settings.timeline_visualization(fullscreen).uses_video()
                        || settings.timeline_hover_enabled(fullscreen)
                }))
        {
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
        if !media.seekable
            || !media.duration.is_finite()
            || media.duration <= 0.0
            || !media.tracks.iter().any(|t| t.id == TrackId::Stream(stream) && !t.is_cover_art)
        {
            return None;
        }
        Some(Self {
            path: path.clone(),
            stream,
            directory: settings.thumbnail_cache_dir(),
            count: crate::engine::thumbnails::sample_count(media.duration),
            auto_generate: settings.filmstrip_auto_playback,
        })
    }
}

struct Worker {
    request: Request,
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
        Self {
            time: t.time,
            image: Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(t.image)])),
            width,
            height,
        }
    }

    /// A copy of the picture (BGRA, as extracted) for analysis.
    fn sample(&self) -> Option<Thumbnail> {
        let bytes = self.image.as_bytes(0)?.to_vec();
        Some(Thumbnail { time: self.time, image: image::RgbaImage::from_raw(self.width, self.height, bytes)? })
    }

    pub(super) fn element(&self, width: f32, height: f32) -> AnyElement {
        let image = self.image.clone();
        let aspect = self.width as f32 / self.height as f32;
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);
                let tw = w.min(h * aspect);
                let th = h.min(w / aspect);
                let target = Bounds::new(
                    point(bounds.origin.x + px((w - tw) * 0.5), bounds.origin.y + px((h - th) * 0.5)),
                    size(px(tw), px(th)),
                );
                let _ = window.paint_image(target, Corners::default(), image.clone(), 0, false);
            },
        )
        .w(px(width))
        .h(px(height))
        .flex_none()
        .into_any_element()
    }
}

type FilmstripSelection = Rc<RefCell<Option<(usize, f64, Vec<usize>)>>>;

#[derive(Default)]
struct BarcodeDisplay {
    image: Option<(TimelineVisualization, u32, u32, Arc<RenderImage>)>,
    retired: Vec<ImageId>,
}

impl BarcodeDisplay {
    fn invalidate(&mut self) {
        if let Some((_, _, _, image)) = self.image.take() {
            self.retired.push(image.id);
        }
    }
}

#[derive(Default)]
pub(super) struct TimelineThumbnails {
    pub(super) waveform: super::waveform::Waveform,
    request: Option<Request>,
    worker: Option<Worker>,
    attempted: bool,
    previews: Arc<Vec<Preview>>,
    filmstrip_selection: FilmstripSelection,
    barcodes: Vec<(TimelineVisualization, Arc<image::RgbaImage>)>,
    barcode_display: Rc<RefCell<BarcodeDisplay>>,
    barcode_worker: Option<(TimelineVisualization, Arc<AtomicBool>, Receiver<image::RgbaImage>)>,
    barcode_fingerprint: Arc<OnceLock<u64>>,
    barcode_failed: Vec<TimelineVisualization>,
    barcode_requested: Option<TimelineVisualization>,
    displayed: [Cell<Option<TimelineVisualization>>; 2],
    policy: Option<Policy>,
    maintenance: Option<Receiver<anyhow::Result<Stats>>>,
    maintenance_pending: bool,
    clear_pending: Option<Policy>,
    maintenance_error: bool,
    stats: Option<Stats>,
    completed_path: Option<(PathBuf, bool)>,
}

fn display_during_transition(
    available: TimelineVisualization,
    previous: TimelineVisualization,
    pending: bool,
) -> TimelineVisualization {
    if pending && previous != TimelineVisualization::None { previous } else { available }
}

impl TimelineThumbnails {
    pub(super) fn reset(&mut self) -> Vec<ImageId> {
        if let Some(worker) = &self.worker {
            worker.cancel.store(true, Ordering::Relaxed);
        }
        // Keep the worker until it acknowledges cancellation, so the playlist
        // queue cannot start a second extractor for the same file and stream.
        self.request = None;
        self.attempted = false;
        for mode in &self.displayed {
            mode.set(None);
        }
        self.take_images()
    }

    fn take_images(&mut self) -> Vec<ImageId> {
        let mut retired: Vec<_> = self.previews.iter().map(|p| p.image.id).collect();
        self.previews = Arc::default();
        self.filmstrip_selection = Rc::default();
        if let Some((_, stop, _)) = self.barcode_worker.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.barcode_fingerprint = Arc::default();
        self.barcodes.clear();
        self.barcode_failed.clear();
        self.barcode_requested = None;
        self.barcode_display.borrow_mut().invalidate();
        retired.extend(self.take_retired_barcode_images());
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
        self.worker
            .as_ref()
            .is_some_and(|w| w.request.path.as_path() == path && stream.is_none_or(|s| s == w.request.stream))
    }

    /// Called from the existing UI tick and engine events. Never blocks on extraction.
    pub(super) fn update(
        &mut self,
        player: &Player,
        settings: &Settings,
        fullscreen: bool,
        playlist_busy: bool,
    ) -> (bool, Vec<ImageId>) {
        let mut changed = self.update_cache(settings);
        let request = player.with_state(|st| Request::from_state(st, settings));
        let request_changed = self.request != request;
        changed |= request_changed;
        let mut retired = if request_changed {
            // Selecting a video visualization can start a new thumbnail request
            // while the current audio waveform is already visible and valid.
            let keep_waveform = player.with_state(|st| self.waveform.ready(st, settings));
            let previous = self.displayed.each_ref().map(|mode| mode.get());
            let retired = self.reset();
            if keep_waveform {
                for (slot, old) in self.displayed.iter().zip(previous) {
                    if old.is_some_and(TimelineVisualization::uses_audio) {
                        slot.set(Some(TimelineVisualization::Waveform));
                    }
                }
            }
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
                        // The previews are the only copy of the pixels: the barcode
                        // worker reads them back when it needs samples.
                        self.previews = Arc::new(images.thumbnails.into_iter().map(Preview::from_bgra).collect());
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
            && !playlist_busy
        {
            self.attempted = true;
            let request = request.clone();
            let worker_request = request.clone();
            let config = settings.engine_config();
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = cancel.clone();
            let generating = Arc::new(AtomicBool::new(false));
            let generation_flag = generating.clone();
            let progress = Arc::new(GenerationProgress::new(request.count));
            let generation_progress = progress.clone();
            let (tx, result) = std::sync::mpsc::channel();
            let auto_generate = request.auto_generate;
            match std::thread::Builder::new().name("timeline-thumbnails".into()).spawn(move || {
                super::background_priority::lower();
                let spec = ThumbnailSpec { stream: request.stream, count: request.count, width: 240, height: 135 };
                let thumbnails = if auto_generate {
                    load_or_generate(
                        &request.path,
                        &config,
                        spec,
                        &request.directory,
                        false,
                        GenerationControl { stop: &stop, generating: &generation_flag, progress: &generation_progress },
                    )
                } else {
                    load_cached(&request.path, spec, &request.directory, None)
                }
                .map(|mut thumbnails| {
                    // Use the persisted JPEG samples after a successful extraction,
                    // so barcode keys are identical on the first and later opens.
                    if generation_flag.load(Ordering::Relaxed)
                        && !stop.load(Ordering::Relaxed)
                        && let Ok(cached) = load_cached(&request.path, spec, &request.directory, None)
                    {
                        thumbnails = cached;
                    }
                    // Extracted and cached images are RGBA; GPUI's RenderImage expects BGRA.
                    for thumbnail in &mut thumbnails {
                        let pixels: &mut [u8] = thumbnail.image.as_mut();
                        for pixel in pixels.as_chunks_mut::<4>().0 {
                            pixel.swap(0, 2);
                        }
                    }
                    WorkerImages { thumbnails, generated: generation_flag.load(Ordering::Relaxed) }
                });
                let _ = tx.send(thumbnails);
            }) {
                Ok(_) => self.worker = Some(Worker { request: worker_request, cancel, generating, progress, result }),
                Err(err) => {
                    log::debug!("could not start thumbnail worker: {err}");
                    changed = true;
                }
            }
        }
        let duration = player.with_state(|st| st.media.as_ref().map_or(0.0, |m| m.duration));
        changed |= self.update_barcodes(settings, duration, fullscreen);
        (changed || self.generating(), retired)
    }

    fn update_barcodes(&mut self, settings: &Settings, duration: f64, fullscreen: bool) -> bool {
        let mut changed = false;
        let mode = settings.timeline_visualization(fullscreen);
        if self.barcode_requested != Some(mode) {
            self.barcode_requested = Some(mode);
            self.barcode_failed.retain(|m| *m != mode);
        }
        if let Some((mode, _, receiver)) = &self.barcode_worker {
            match receiver.try_recv() {
                Ok(image) => {
                    self.barcodes.push((*mode, Arc::new(image)));
                    self.barcode_worker = None;
                    self.maintenance_pending = true;
                    changed = true;
                }
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => {
                    self.barcode_failed.push(*mode);
                    self.barcode_worker = None;
                    changed = true;
                }
            }
        }
        // Analyze only the currently selected visualization, not the hidden
        // windowed/fullscreen preference. Never redo an analysis during playback.
        if !self.previews.is_empty()
            && !self.barcode_failed.contains(&mode)
            && !matches!(
                mode,
                TimelineVisualization::None
                    | TimelineVisualization::Filmstrip
                    | TimelineVisualization::Waveform
                    | TimelineVisualization::FilmstripWaveform
            )
            && !self.barcodes.iter().any(|(existing, _)| *existing == mode)
        {
            let previews = self.previews.clone();
            let fingerprint = self.barcode_fingerprint.clone();
            let directory = settings.thumbnail_cache_dir();
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            if std::thread::Builder::new()
                .name("timeline-barcode".into())
                .spawn(move || {
                    super::background_priority::lower();
                    use crate::engine::thumbnails::barcode_cache;
                    let samples: Vec<Thumbnail> = previews.iter().filter_map(Preview::sample).collect();
                    drop(previews);
                    let id = *fingerprint.get_or_init(|| barcode_cache::fingerprint(&samples));
                    let Some(key) = super::barcode::cache_key(mode, id, duration) else {
                        return;
                    };
                    let image = barcode_cache::load(&directory, &key).unwrap_or_else(|_| {
                        let image = super::barcode::build(mode, &samples, duration);
                        if !worker_stop.load(Ordering::Relaxed)
                            && let Err(err) = barcode_cache::save(&directory, &key, &image, &worker_stop)
                        {
                            log::debug!("barcode cache: {err}");
                        }
                        image
                    });
                    let _ = tx.send(image);
                })
                .is_ok()
            {
                self.barcode_worker = Some((mode, stop, rx));
            } else {
                self.barcode_failed.push(mode);
                changed = true;
            }
        }
        changed
    }

    fn update_cache(&mut self, settings: &Settings) -> bool {
        let policy = Policy {
            directory: settings.thumbnail_cache_dir(),
            days: settings.thumbnail_cache_days,
            mb: settings.thumbnail_cache_mb,
        };
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
                        Ok(stats) if !self.maintenance_pending => {
                            self.stats = Some(stats);
                            self.maintenance_error = false;
                        }
                        Ok(_) => {}
                        Err(err) => {
                            self.maintenance_error = true;
                            log::debug!("thumbnail cache maintenance: {err}");
                        }
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => return changed,
                Err(TryRecvError::Disconnected) => {
                    self.maintenance = None;
                    self.maintenance_error = true;
                    changed = true;
                }
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
                Err(err) => {
                    self.maintenance_error = true;
                    log::debug!("could not start cache maintenance: {err}");
                }
            }
        }
        changed
    }

    pub(super) fn take_completed_path(&mut self) -> Option<(PathBuf, bool)> {
        self.completed_path.take()
    }

    pub(super) fn refresh_cache_stats(&mut self) {
        self.maintenance_pending = true;
    }

    pub(super) fn clear_cache(&mut self, settings: &Settings) {
        self.waveform.clear_pending();
        if let Some((_, stop, _)) = &self.barcode_worker {
            stop.store(true, Ordering::Relaxed);
        }
        self.clear_pending = Some(Policy {
            directory: settings.thumbnail_cache_dir(),
            days: settings.thumbnail_cache_days,
            mb: settings.thumbnail_cache_mb,
        });
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

    pub(super) fn available_visualization(
        &self,
        st: &PlaybackState,
        settings: &Settings,
        fullscreen: bool,
    ) -> TimelineVisualization {
        let mode = settings.timeline_visualization(fullscreen);
        let displayed = &self.displayed[usize::from(fullscreen)];
        if mode == TimelineVisualization::None {
            displayed.set(None);
            return mode;
        }
        let request = Request::from_state(st, settings);
        let samples_ready = request.is_some() && self.request == request && !self.previews.is_empty();
        let video_ready = |mode| {
            samples_ready
                && (matches!(mode, TimelineVisualization::Filmstrip | TimelineVisualization::FilmstripWaveform)
                    || self.barcodes.iter().any(|(m, _)| *m == mode))
        };
        let audio = self.waveform.ready(st, settings);
        let video = video_ready(mode);
        let available = if mode.uses_video()
            && !video
            && audio
            && settings.waveform_auto_audio
            && st.media.as_ref().is_some_and(|m| !m.has_video())
        {
            TimelineVisualization::Waveform
        } else {
            mode.available(video, audio)
        };
        let video_pending = mode.uses_video()
            && !video
            && request.as_ref().is_some_and(|request| {
                if samples_ready {
                    !self.barcode_failed.contains(&mode)
                } else {
                    self.request.as_ref() != Some(request)
                        || !self.attempted
                        || self
                            .worker
                            .as_ref()
                            .is_some_and(|w| &w.request == request && !w.cancel.load(Ordering::Relaxed))
                }
            });
        let audio_pending = mode.uses_audio() && !audio && self.waveform.pending(st, settings, fullscreen);
        // Keep the last usable visualization while the new one is being loaded.
        // Revalidate its data against the current media and enabled settings.
        let previous =
            displayed.get().map_or(TimelineVisualization::None, |old| old.available(video_ready(old), audio));
        let visible = display_during_transition(available, previous, video_pending || audio_pending);
        displayed.set((visible != TimelineVisualization::None).then_some(visible));
        visible
    }

    pub(super) fn has_previews(&self, st: &PlaybackState, settings: &Settings, fullscreen: bool) -> bool {
        self.available_visualization(st, settings, fullscreen) != TimelineVisualization::None
    }

    pub(super) fn take_retired_barcode_images(&self) -> Vec<ImageId> {
        std::mem::take(&mut self.barcode_display.borrow_mut().retired)
    }

    pub(super) fn visualization(
        &self,
        mode: TimelineVisualization,
        duration: f64,
        height: f32,
        opacity: f32,
        resizing: bool,
    ) -> AnyElement {
        let element = match mode {
            TimelineVisualization::Filmstrip => self.filmstrip(duration, height),
            TimelineVisualization::Waveform => self.waveform.element(height, 0.0, resizing),
            TimelineVisualization::FilmstripWaveform => div()
                .absolute()
                .size_full()
                .child(self.filmstrip(duration, height * 0.6))
                .child(self.waveform.element(height * 0.4, height * 0.6, resizing))
                .into_any_element(),
            TimelineVisualization::None => div().into_any_element(),
            _ => {
                let image = self.barcodes.iter().find(|(m, _)| *m == mode).map(|(_, image)| image.clone());
                let display = self.barcode_display.clone();
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let Some(source) = &image else {
                            return;
                        };
                        let w = f32::from(bounds.size.width);
                        let h = f32::from(bounds.size.height);
                        if w <= 0.0 || h <= 0.0 {
                            return;
                        }
                        let scale = window.scale_factor();
                        let width = (w * scale).round().clamp(1.0, 4096.0) as u32;
                        let height = (h * scale).round().clamp(1.0, 1024.0) as u32;
                        let mut display = display.borrow_mut();
                        // Stretch the existing texture during panel drags; rebuild at
                        // the final physical size when the slider is released.
                        if display
                            .image
                            .as_ref()
                            .is_none_or(|(m, w, h, _)| *m != mode || (!resizing && (*w != width || *h != height)))
                        {
                            display.invalidate();
                            let raster = super::barcode::display_raster(source, width, height);
                            display.image = Some((
                                mode,
                                width,
                                height,
                                Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(raster)])),
                            ));
                        }
                        if let Some((_, _, _, image)) = &display.image {
                            let _ = window.paint_image(bounds, Corners::default(), image.clone(), 0, false);
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .w_full()
                .h(px(height))
                .into_any_element()
            }
        };
        div().absolute().top_0().left_0().w_full().h(px(height)).opacity(opacity).child(element).into_any_element()
    }

    fn filmstrip(&self, duration: f64, filmstrip_height: f32) -> AnyElement {
        let previews = self.previews.clone();
        let selection = self.filmstrip_selection.clone();
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
                    let mut selection = selection.borrow_mut();
                    if selection.as_ref().is_none_or(|(n, d, _)| *n != count || *d != duration) {
                        let indices = (0..count)
                            .filter_map(|i| {
                                let time = (i as f64 + 0.5) / count as f64 * duration;
                                previews
                                    .iter()
                                    .enumerate()
                                    .min_by(|(_, a), (_, b)| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
                                    .map(|(index, _)| index)
                            })
                            .collect();
                        *selection = Some((count, duration, indices));
                    }
                    let Some((_, _, indices)) = selection.as_ref() else {
                        return;
                    };
                    for (i, &index) in indices.iter().enumerate() {
                        let preview = &previews[index];
                        let scale = ((cell - 1.0).max(0.0) / preview.width as f32).min(height / preview.height as f32);
                        let w = preview.width as f32 * scale;
                        let h = preview.height as f32 * scale;
                        let target = Bounds::new(
                            point(
                                bounds.origin.x + px(i as f32 * cell + (cell - w) * 0.5),
                                bounds.origin.y + px((height - h) * 0.5),
                            ),
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

    pub(super) fn for_playlist(
        &self,
        path: &std::path::Path,
        stream: usize,
        count: u32,
        time: f64,
    ) -> Option<&Preview> {
        let request = self.request.as_ref()?;
        if request.path.as_path() != path || request.stream != stream || request.count != count {
            return None;
        }
        self.previews.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
    }

    pub(super) fn for_chapter(
        &self,
        st: &PlaybackState,
        settings: &Settings,
        start: f64,
        end: f64,
    ) -> Option<&Preview> {
        if !settings.timeline_thumbnails
            || !settings.chapter_thumbnails
            || self.request != Request::from_state(st, settings)
        {
            return None;
        }
        let target = start + (end - start) * 0.25;
        self.previews
            .iter()
            .filter(|p| p.time >= start && p.time < end)
            .min_by(|a, b| (a.time - target).abs().total_cmp(&(b.time - target).abs()))
    }

    pub(super) fn nearest(
        &self,
        st: &PlaybackState,
        settings: &Settings,
        time: f64,
        fullscreen: bool,
    ) -> Option<&Preview> {
        if !settings.timeline_hover_enabled(fullscreen) {
            return None;
        }
        self.nearest_sample(st, settings, time)
    }

    pub(super) fn nearest_sample(&self, st: &PlaybackState, settings: &Settings, time: f64) -> Option<&Preview> {
        // Do not show the previous file's preview before the next UI tick.
        if !settings.timeline_thumbnails || self.request != Request::from_state(st, settings) {
            return None;
        }
        self.previews.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
    }
}

pub(super) fn filmstrip_height(
    settings: &Settings,
    window: &gpui::Window,
    cx: &mut gpui::Context<super::player::PlayerView>,
) -> f32 {
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

pub(super) fn hover_preview(
    fraction: f32,
    preview: &Preview,
    background: Hsla,
    extra: f32,
    label: String,
) -> AnyElement {
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
        .flex()
        .flex_col()
        .items_center()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visualization_transition_keeps_previous_until_ready_or_failed() {
        use TimelineVisualization::{BarcodeAverage, Filmstrip, FilmstripWaveform, None};
        assert_eq!(display_during_transition(None, Filmstrip, true), Filmstrip);
        assert_eq!(display_during_transition(BarcodeAverage, Filmstrip, false), BarcodeAverage);
        assert_eq!(display_during_transition(None, Filmstrip, false), None);
        assert_eq!(display_during_transition(None, None, true), None);
        assert_eq!(display_during_transition(Filmstrip, BarcodeAverage, true), BarcodeAverage);
        assert_eq!(display_during_transition(FilmstripWaveform, BarcodeAverage, false), FilmstripWaveform);
    }

    #[test]
    fn disconnected_barcode_worker_is_not_retried_until_reselection() {
        let mode = TimelineVisualization::BarcodeAverage;
        let mut settings = Settings::default();
        settings.set_timeline_visualization(false, mode);
        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        let mut timeline = TimelineThumbnails {
            barcode_worker: Some((mode, Arc::new(AtomicBool::new(false)), rx)),
            barcode_requested: Some(mode),
            ..Default::default()
        };
        assert!(timeline.update_barcodes(&settings, 10.0, false));
        assert!(timeline.barcode_failed.contains(&mode));
        assert!(!timeline.update_barcodes(&settings, 10.0, false));
        assert!(timeline.barcode_failed.contains(&mode));
        settings.set_timeline_visualization(false, TimelineVisualization::Filmstrip);
        timeline.update_barcodes(&settings, 10.0, false);
        settings.set_timeline_visualization(false, mode);
        timeline.update_barcodes(&settings, 10.0, false);
        assert!(!timeline.barcode_failed.contains(&mode));
    }

    #[test]
    fn cancelling_timeline_worker_keeps_its_media_reserved() {
        let request =
            Request { path: "movie.mp4".into(), stream: 0, directory: "cache".into(), count: 20, auto_generate: false };
        let cancel = Arc::new(AtomicBool::new(false));
        let (_tx, result) = std::sync::mpsc::channel();
        let mut timeline = TimelineThumbnails {
            request: Some(request.clone()),
            worker: Some(Worker {
                request: request.clone(),
                cancel: cancel.clone(),
                generating: Arc::new(AtomicBool::new(true)),
                progress: Arc::new(GenerationProgress::new(20)),
                result,
            }),
            ..Default::default()
        };
        assert!(timeline.reset().is_empty());
        assert!(cancel.load(Ordering::Relaxed));
        assert!(timeline.busy_for(&request.path, Some(0)));
        assert!(!timeline.busy_for(&request.path, Some(1)));
        assert!(!timeline.generating());
    }
}
