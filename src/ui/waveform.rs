//! Waveform display and its independently cancellable analysis job.
use crate::engine::{MediaSource, PlaybackState, Player, Status, TrackId, waveform::{self, Peaks, Progress}};
use crate::settings::Settings;
use gpui::{AnyElement, Bounds, canvas, point, prelude::*, px, size};
use std::path::PathBuf;
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
        let auto_audio = settings.waveform_auto_audio && st.media.as_ref().is_some_and(|m| !m.has_video());
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
pub(super) struct Waveform {
    request: Option<Request>,
    worker: Option<Worker>,
    queue: std::collections::VecDeque<Job>,
    peaks: Option<Arc<Peaks>>,
    attempted: bool,
    maintenance: bool,
    manual_done: usize,
    queue_total: Arc<AtomicUsize>,
    directory: Option<PathBuf>,
}
impl Waveform {
    // Changing playback cancels automatic work, not explicitly queued work.
    pub(super) fn reset(&mut self) {
        if self.worker.as_ref().is_some_and(|w| !w.manual) { self.worker = None; }
        self.request = None;
        self.peaks = None;
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
        if self.queue.is_empty() && !self.worker.as_ref().is_some_and(|w| w.manual) { self.manual_done = 0; }
        self.queue.push_back(Job { request, force });
        self.queue_total.store(self.manual_done + self.queue.len() + usize::from(self.worker.as_ref().is_some_and(|w| w.manual)), Ordering::Relaxed);
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
            if self.worker.as_ref().is_some_and(|w| !w.manual) { self.worker = None; }
            self.peaks = None;
            self.attempted = false;
            self.request = media;
        }
        if wanted.is_none() && self.worker.as_ref().is_some_and(|w| !w.manual) {
            self.worker = None;
            self.attempted = false;
            changed = true;
        }
        if let Some(worker) = &self.worker {
            match worker.result.try_recv() {
                Ok(result) => {
                    match result {
                        Ok((request, peaks)) => {
                            if self.request.as_ref() == Some(&request) { self.peaks = Some(Arc::new(peaks)); self.attempted = true; }
                            self.maintenance = true;
                        }
                        Err(e) => log::debug!("waveform: {e}"),
                    }
                    if worker.manual { self.manual_done += 1; }
                    self.worker = None;
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    if worker.manual { self.manual_done += 1; }
                    self.worker = None;
                    changed = true;
                }
                Err(TryRecvError::Empty) => return changed || self.progress().is_some(),
            }
        }
        let next = if let Some(job) = self.queue.pop_front() { Some((job, true)) }
            else if !self.attempted { wanted.map(|request| (Job { request, force: false }, false)) } else { None };
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
                let output = (|| -> anyhow::Result<(Request, Peaks)> {
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
    pub(super) fn element(&self, height: f32, top: f32) -> AnyElement {
        let peaks = self.peaks.clone();
        canvas(|_, _, _| (), move |bounds, _, window, _| {
            let Some(peaks) = &peaks else { return; };
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            if w <= 0.0 || h <= 0.0 || peaks.is_empty() { return; }
            let columns = (w.ceil() as usize).clamp(1, 4096);
            let step = w / columns as f32;
            let color: gpui::Hsla = gpui::rgb(0x88b8ce).into();
            for x in 0..columns {
                let from = x * peaks.len() / columns;
                let to = ((x + 1) * peaks.len() / columns).max(from + 1).min(peaks.len());
                let slice = &peaks[from..to];
                let lo = slice.iter().map(|p| p[0]).min().unwrap_or(0) as f32 / 32768.0;
                let hi = slice.iter().map(|p| p[1]).max().unwrap_or(0) as f32 / 32768.0;
                let y = (h * (1.0 - hi) * 0.5).clamp(0.0, (h - 1.0).max(0.0));
                let bottom = (h * (1.0 - lo) * 0.5).clamp(y, h);
                window.paint_quad(gpui::fill(Bounds::new(point(bounds.origin.x + px(x as f32 * step), bounds.origin.y + px(y)), size(px(step), px((bottom - y).max(1.0).min(h)))), color));
            }
        }).absolute().top(px(top)).left_0().w_full().h(px(height)).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
