//! Waveform display and its independently cancellable analysis job.
use crate::engine::{MediaSource, PlaybackState, Player, Status, TrackId, waveform::{self, Peaks, Progress}};
use crate::settings::Settings;
use gpui::{AnyElement, Corners, ImageId, RenderImage, canvas, prelude::*, px};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}, mpsc::{Receiver, TryRecvError}};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Request { path: PathBuf, stream: Option<usize>, directory: PathBuf }
impl Request {
    fn media(st: &PlaybackState, settings: &Settings) -> Option<Self> {
        if !settings.timeline_waveform || !matches!(st.status, Status::Playing | Status::Paused | Status::Ended) { return None; }
        let (Some(MediaSource::File(path)), Some(TrackId::Stream(stream)), Some(media)) = (&st.source, st.selected.audio, &st.media) else { return None; };
        if !media.seekable || !media.duration.is_finite() || media.duration <= 0.0 { return None; }
        Some(Self { path: path.clone(), stream: Some(stream), directory: settings.thumbnail_cache_dir() })
    }
    fn wanted(st: &PlaybackState, settings: &Settings, fullscreen: bool) -> Option<Self> {
        let auto_audio = st.media.as_ref().is_some_and(|m| if m.has_video() { settings.waveform_auto_video } else { settings.waveform_auto_audio });
        if !auto_audio && !settings.timeline_visualization(fullscreen).uses_audio() { return None; }
        Self::media(st, settings)
    }
}
#[derive(Clone)]
struct Job { request: Request, force: bool }
struct Worker {
    job: Job,
    manual: bool,
    stop: Arc<AtomicBool>,
    progress: Arc<Progress>,
    result: Receiver<anyhow::Result<(Request, Peaks)>>,
}
impl Drop for Worker { fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); } }

pub(super) struct Activity {
    progress: Arc<Progress>,
    name: String,
    position: Option<usize>,
    total: Arc<AtomicUsize>,
}
impl Activity {
    pub(super) fn label(&self) -> String {
        let label = if self.progress.generating.load(Ordering::Relaxed) {
            format!("Generating audio waveform ({}% done)", self.progress.percent.load(Ordering::Relaxed))
        } else { "Checking audio waveform cache".into() };
        if let Some(x) = self.position { format!("{label}\n{} ({x}/{})", self.name, self.total.load(Ordering::Relaxed)) }
        else { format!("{label}\n{}", self.name) }
    }
}

#[derive(Default)]
struct DisplayImage {
    image: Option<(usize, usize, Arc<RenderImage>)>,
    retired: Vec<ImageId>,
}
impl DisplayImage {
    fn invalidate(&mut self) {
        if let Some((_, _, image)) = self.image.take() { self.retired.push(image.id); }
    }
}

fn rasterize_peaks(peaks: &Peaks, columns: usize, height: usize) -> image::RgbaImage {
    let mut image = image::RgbaImage::new(columns as u32, height as u32);
    if height == 0 { return image; }
    let h = height as f32;
    for (x, peak) in reduce_peaks(peaks, columns).iter().enumerate() {
        let lo = peak[0] as f32 / 32768.0;
        let hi = peak[1] as f32 / 32768.0;
        let y = (h * (1.0 - hi) * 0.5).clamp(0.0, h - 1.0);
        let bottom = (h * (1.0 - lo) * 0.5).clamp(y, h);
        // GPUI RenderImage expects BGRA; transparent pixels preserve the background.
        for row in y.floor() as usize..(y + (bottom - y).max(1.0)).ceil().min(h) as usize {
            image.put_pixel(x as u32, row as u32, image::Rgba([0xce, 0xb8, 0x88, 255]));
        }
    }
    image
}

fn reduce_peaks(peaks: &Peaks, columns: usize) -> Peaks {
    if peaks.is_empty() || columns == 0 { return Vec::new(); }
    (0..columns).map(|x| {
        let from = x * peaks.len() / columns;
        let to = ((x + 1) * peaks.len() / columns).max(from + 1).min(peaks.len());
        let slice = &peaks[from..to];
        [slice.iter().map(|p| p[0]).min().unwrap_or(0), slice.iter().map(|p| p[1]).max().unwrap_or(0)]
    }).collect()
}

#[derive(Default)]
pub(super) struct Waveform {
    request: Option<Request>,
    worker: Option<Worker>,
    queue: std::collections::VecDeque<Job>,
    peaks: Option<Arc<Peaks>>,
    display_image: Rc<RefCell<DisplayImage>>,
    attempted: bool,
    maintenance: bool,
    manual_done: usize,
    queue_total: Arc<AtomicUsize>,
    directory: Option<PathBuf>,
}
impl Waveform {
    // Changing playback cancels automatic work, not explicitly queued work.
    pub(super) fn reset(&mut self) {
        if let Some(w) = &self.worker && !w.manual { w.stop.store(true, Ordering::Relaxed); }
        self.request = None;
        self.peaks = None;
        self.display_image.borrow_mut().invalidate();
        self.attempted = false;
    }
    pub(super) fn can_regenerate(&self, st: &PlaybackState, settings: &Settings) -> bool { Request::media(st, settings).is_some() }
    pub(super) fn regenerate(&mut self, player: &Player, settings: &Settings) {
        let Some(request) = player.with_state(|st| Request::media(st, settings)) else { return; };
        self.enqueue(request.path, request.stream, settings, true);
        self.update(player, settings, false);
    }
    pub(super) fn enqueue(&mut self, path: PathBuf, stream: Option<usize>, settings: &Settings, force: bool) {
        if !settings.timeline_waveform { return; }
        let request = Request { path, stream, directory: settings.thumbnail_cache_dir() };
        if let Some(job) = self.queue.iter_mut().find(|j| j.request == request) {
            job.force |= force;
            return;
        }
        if let Some(w) = &mut self.worker
            && !w.stop.load(Ordering::Relaxed)
            && w.job.request == request
            && (!force || w.job.force || w.progress.generating.load(Ordering::Relaxed))
        {
            // An explicit request for work already running must survive a media change.
            if !w.manual {
                if self.queue.is_empty() { self.manual_done = 0; }
                w.manual = true;
                self.queue_total.store(self.manual_done + self.queue.len() + 1, Ordering::Relaxed);
            }
            return;
        }
        if self.queue.is_empty() && !self.worker.as_ref().is_some_and(|w| w.manual && !w.stop.load(Ordering::Relaxed)) { self.manual_done = 0; }
        self.queue.push_back(Job { request, force });
        self.queue_total.store(self.manual_done + self.queue.len() + usize::from(self.worker.as_ref().is_some_and(|w| w.manual && !w.stop.load(Ordering::Relaxed))), Ordering::Relaxed);
    }
    pub(super) fn has_generation(&self) -> bool { self.worker.is_some() || !self.queue.is_empty() }
    pub(super) fn retain_generation(&mut self, keep: impl Fn(&std::path::Path) -> bool) -> bool {
        let before = self.queue.len();
        self.queue.retain(|job| keep(&job.request.path));
        let cancelled = self.worker.as_ref().is_some_and(|w| !keep(&w.job.request.path) && !w.stop.swap(true, Ordering::Relaxed));
        if cancelled || before != self.queue.len() {
            self.queue_total.store(self.manual_done + self.queue.len() + usize::from(self.worker.as_ref().is_some_and(|w| w.manual && !w.stop.load(Ordering::Relaxed))), Ordering::Relaxed);
        }
        cancelled || before != self.queue.len()
    }
    pub(super) fn clear_pending(&mut self) {
        self.worker = None;
        self.queue.clear();
        self.manual_done = 0;
        self.queue_total.store(0, Ordering::Relaxed);
        self.attempted = true;
    }
    pub(super) fn take_maintenance(&mut self) -> bool { std::mem::take(&mut self.maintenance) }
    pub(super) fn progress(&self) -> Option<Activity> {
        let w = self.worker.as_ref()?;
        if w.stop.load(Ordering::Relaxed) { return None; }
        if !w.manual && !w.progress.generating.load(Ordering::Relaxed) { return None; }
        let name = w.job.request.path.file_name().unwrap_or(w.job.request.path.as_os_str()).to_string_lossy().into_owned();
        Some(Activity { progress: w.progress.clone(), name, position: w.manual.then_some(self.manual_done + 1), total: self.queue_total.clone() })
    }
    pub(super) fn update(&mut self, player: &Player, settings: &Settings, fullscreen: bool) -> bool {
        let directory = settings.thumbnail_cache_dir();
        if !settings.timeline_waveform || self.directory.as_ref().is_some_and(|d| d != &directory) {
            self.clear_pending();
            self.reset();
        }
        self.directory = Some(directory);
        if !settings.timeline_waveform { return false; }
        let media = player.with_state(|st| Request::media(st, settings));
        let wanted = player.with_state(|st| Request::wanted(st, settings, fullscreen));
        let mut changed = media != self.request;
        if changed {
            if let Some(w) = &self.worker && !w.manual { w.stop.store(true, Ordering::Relaxed); }
            self.peaks = None;
            self.display_image.borrow_mut().invalidate();
            self.attempted = false;
            self.request = media;
        }
        if wanted.is_none() && let Some(w) = &self.worker && !w.manual && !w.stop.swap(true, Ordering::Relaxed) {
            self.attempted = false;
            changed = true;
        }
        if let Some(worker) = &self.worker {
            match worker.result.try_recv() {
                Ok(result) => {
                    if !worker.stop.load(Ordering::Relaxed) {
                        match result {
                            Ok((request, peaks)) => {
                                if self.request.as_ref() == Some(&request) { self.peaks = Some(Arc::new(peaks)); self.display_image.borrow_mut().invalidate(); self.attempted = true; }
                                self.maintenance = true;
                            }
                            Err(e) => log::debug!("waveform: {e}"),
                        }
                        if worker.manual { self.manual_done += 1; }
                    } else {
                        self.maintenance = true;
                    }
                    self.worker = None;
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    if worker.manual && !worker.stop.load(Ordering::Relaxed) { self.manual_done += 1; }
                    self.worker = None;
                    changed = true;
                }
                Err(TryRecvError::Empty) => return changed || self.progress().is_some(),
            }
        }
        if player.with_state(|st| st.status == Status::Loading) { return changed; }
        let preferred = self.request.as_ref().and_then(|request| {
            self.queue.iter().position(|job| &job.request == request)
                .or_else(|| self.queue.iter().position(|job| job.request.path == request.path && job.request.directory == request.directory && job.request.stream.is_none()))
        });
        let next = if let Some(job) = preferred.and_then(|i| self.queue.remove(i)) { Some((job, true)) }
            else if !self.attempted && wanted.is_some() { wanted.map(|request| (Job { request, force: false }, false)) }
            else { self.queue.pop_front().map(|job| (job, true)) };
        if let Some((job, manual)) = next {
            if self.request.as_ref() == Some(&job.request) { self.attempted = true; }
            let work = job.clone();
            let cfg = settings.engine_config();
            let stop = Arc::new(AtomicBool::new(false));
            let cancel = stop.clone();
            let progress = Arc::new(Progress::default());
            let progress_copy = progress.clone();
            let (tx, result) = std::sync::mpsc::channel();
            match std::thread::Builder::new().name("audio-waveform".into()).spawn(move || {
                super::background_priority::lower();
                let output = (|| -> anyhow::Result<(Request, Peaks)> {
                    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "waveform generation cancelled");
                    let mut request = work.request;
                    let stream = request.stream.or_else(|| crate::engine::probe::probe(&MediaSource::File(request.path.clone()), &cfg).and_then(|p| p.audio_stream))
                        .ok_or_else(|| anyhow::anyhow!("no audio stream"))?;
                    request.stream = Some(stream);
                    let peaks = waveform::load_or_generate(&request.path, &cfg, stream, &request.directory, work.force, &cancel, &progress_copy)?;
                    Ok((request, peaks))
                })();
                let _ = tx.send(output);
            }) {
                Ok(_) => self.worker = Some(Worker { job, manual, stop, progress, result }),
                Err(e) => { log::debug!("could not start waveform worker: {e}"); if manual { self.manual_done += 1; } }
            }
            changed = true;
        }
        changed || self.progress().is_some()
    }
    pub(super) fn ready(&self, st: &PlaybackState, settings: &Settings) -> bool {
        self.peaks.is_some() && self.request == Request::media(st, settings)
    }
    pub(super) fn pending(&self, st: &PlaybackState, settings: &Settings, fullscreen: bool) -> bool {
        let Some(request) = Request::wanted(st, settings, fullscreen) else { return false; };
        let matches = |job: &Job| job.request.path == request.path && job.request.directory == request.directory
            && (job.request.stream.is_none() || job.request.stream == request.stream);
        self.request.as_ref() != Some(&request) || !self.attempted
            || self.queue.iter().any(matches)
            || self.worker.as_ref().is_some_and(|w| !w.stop.load(Ordering::Relaxed) && matches(&w.job))
    }
    pub(super) fn take_retired_images(&self) -> Vec<ImageId> { std::mem::take(&mut self.display_image.borrow_mut().retired) }
    pub(super) fn element(&self, height: f32, top: f32, resizing: bool) -> AnyElement {
        let peaks = self.peaks.clone();
        let display_image = self.display_image.clone();
        canvas(|_, _, _| (), move |bounds, _, window, _| {
            let Some(peaks) = &peaks else { return; };
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            if w <= 0.0 || h <= 0.0 || peaks.is_empty() { return; }
            let columns = (w.ceil() as usize).clamp(1, 4096);
            let rows = (h.ceil() as usize).clamp(1, 1024);
            let mut display = display_image.borrow_mut();
            if display.image.as_ref().is_none_or(|(c, r, _)| !resizing && (*c != columns || *r != rows)) {
                display.invalidate();
                let image = rasterize_peaks(peaks, columns, rows);
                display.image = Some((columns, rows, Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(image)]))));
            }
            if let Some((_, _, image)) = &display.image {
                let _ = window.paint_image(bounds, Corners::default(), image.clone(), 0, false);
            }
        }).absolute().top(px(top)).left_0().w_full().h(px(height)).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_peaks_preserve_extrema_and_support_upsampling() {
        let peaks = vec![[-2, 4], [-8, 3], [-1, 9], [-5, 6]];
        assert_eq!(reduce_peaks(&peaks, 2), vec![[-8, 4], [-5, 9]]);
        assert_eq!(reduce_peaks(&vec![[-2, 4]], 3), vec![[-2, 4]; 3]);
        assert!(reduce_peaks(&peaks, 0).is_empty());
        assert!(reduce_peaks(&Vec::new(), 2).is_empty());
    }

    #[test]
    fn waveform_raster_preserves_transparency_and_extrema() {
        let image = rasterize_peaks(&vec![[i16::MIN, i16::MAX], [0, 0]], 2, 8);
        assert_eq!(image.dimensions(), (2, 8));
        for y in 0..8 { assert_eq!(image.get_pixel(0, y).0, [0xce, 0xb8, 0x88, 255]); }
        assert_eq!(image.get_pixel(1, 4).0, [0xce, 0xb8, 0x88, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [0, 0, 0, 0]);
        assert_eq!(image.get_pixel(1, 7).0, [0, 0, 0, 0]);
    }

    #[test]
    fn removing_media_prunes_waveform_jobs_and_cancels_worker() {
        let mut state = Waveform::default();
        let settings = Settings::default();
        let a = PathBuf::from("a.mp3");
        let b = PathBuf::from("b.mp3");
        state.enqueue(a.clone(), Some(0), &settings, false);
        let job = state.queue.pop_front().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (_tx, result) = std::sync::mpsc::channel();
        state.worker = Some(Worker { job, manual: true, stop: stop.clone(), progress: Arc::new(Progress::default()), result });
        state.enqueue(b.clone(), Some(0), &settings, false);
        assert!(state.retain_generation(|p| p == b.as_path()));
        assert!(stop.load(Ordering::Relaxed));
        assert!(state.worker.is_some());
        assert!(state.progress().is_none());
        assert_eq!(state.queue_total.load(Ordering::Relaxed), 1);
        state.enqueue(a, Some(0), &settings, false);
        assert_eq!(state.queue.len(), 2);
        assert_eq!(state.queue_total.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn waveform_queue_keeps_order_and_upgrades_missing_requests() {
        let mut state = Waveform::default();
        let settings = Settings::default();
        state.enqueue(PathBuf::from("a.mp3"), Some(0), &settings, false);
        state.enqueue(PathBuf::from("b.mp3"), Some(0), &settings, true);
        state.enqueue(PathBuf::from("a.mp3"), Some(0), &settings, true);
        state.reset();
        assert_eq!(state.queue.len(), 2);
        assert_eq!(state.queue_total.load(Ordering::Relaxed), 2);
        let first = state.queue.pop_front().unwrap();
        assert_eq!(first.request.path, PathBuf::from("a.mp3"));
        assert!(first.force);
        assert_eq!(state.queue.pop_front().unwrap().request.path, PathBuf::from("b.mp3"));
        state.clear_pending();
        assert!(state.queue.is_empty());
    }
}
