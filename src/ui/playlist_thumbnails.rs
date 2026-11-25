//! Bounded, cache-only playlist previews. Explicit regeneration uses a separate worker.
use super::thumbnails::{Preview, TimelineThumbnails};
use crate::engine::{MediaSource, PlaybackState, thumbnails::{GenerationProgress, Thumbnail, ThumbnailSpec, cache::{load_cached, load_or_generate, stream_cached}}};
use crate::playlist::PlaylistItem;
use crate::settings::{PlaylistThumbnails, Settings};
use gpui::{Bounds, ImageId, Pixels, Point};
use std::{collections::VecDeque, path::PathBuf, sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}, mpsc::{Receiver, TryRecvError}}};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    pub path: PathBuf,
    pub stream: usize,
    pub count: u32,
    directory: PathBuf,
}
impl Key {
    pub(super) fn new(item: &PlaylistItem, settings: &Settings) -> Option<Self> {
        let MediaSource::File(path) = &item.source else { return None };
        let duration = item.duration.filter(|d| d.is_finite() && *d > 0.0)?;
        Some(Self { path: path.clone(), stream: item.video_stream?, count: crate::engine::thumbnails::sample_count(duration), directory: settings.thumbnail_cache_dir() })
    }
    pub fn selection_key(&self) -> String { format!("{}:{}", self.stream, self.path.to_string_lossy()) }
    pub fn representative_time(&self, settings: &Settings, duration: f64) -> f64 {
        settings.playlist_thumbnail_times.get(&self.selection_key()).copied().filter(|t| t.is_finite() && *t >= 0.0 && *t <= duration).unwrap_or(duration * 0.05)
    }
    fn spec(&self) -> ThumbnailSpec { ThumbnailSpec { stream: self.stream, count: self.count, width: 240, height: 135 } }
}

pub(super) struct Hover {
    pub id: u64,
    pub key: Key,
    pub duration: f64,
    pub fraction: f32,
    pub bounds: Bounds<Pixels>,
}
enum JobOutput { Media(Key, Vec<Thumbnail>, Option<f64>), Existing(Key, f64, Thumbnail), Skipped }
type JobResult = anyhow::Result<JobOutput>;

struct Job {
    kind: JobKind,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<GenerationProgress>>,
    result: Receiver<JobResult>,
}
impl Drop for Job { fn drop(&mut self) { self.cancel.store(true, Ordering::Relaxed); } }
#[derive(Clone)]
enum JobKind { Generate(PathBuf, Option<usize>) }

struct HoverJob {
    key: Key,
    cancel: Arc<AtomicBool>,
    desired: Arc<AtomicU64>,
    result: Receiver<anyhow::Result<Option<Thumbnail>>>,
}
impl Drop for HoverJob { fn drop(&mut self) { self.cancel.store(true, Ordering::Relaxed); } }

struct InlineJob {
    requests: Vec<(Key, f64)>,
    cancel: Arc<AtomicBool>,
    result: Receiver<(Key, f64, Option<Thumbnail>)>,
}
impl Drop for InlineJob { fn drop(&mut self) { self.cancel.store(true, Ordering::Relaxed); } }

fn to_bgra(mut thumb: Thumbnail) -> Thumbnail {
    let pixels: &mut [u8] = thumb.image.as_mut();
    for pixel in pixels.as_chunks_mut::<4>().0 { pixel.swap(0, 2); }
    thumb
}

struct ActiveGeneration {
    progress: Arc<GenerationProgress>,
    filename: String,
    index: usize,
    total: usize,
}
#[derive(Default)]
pub(super) struct QueueActivity(parking_lot::Mutex<Option<ActiveGeneration>>);
impl QueueActivity {
    pub fn label(&self) -> String {
        let activity = self.0.lock();
        let Some(ActiveGeneration { progress, filename, index, total }) = activity.as_ref() else { return "Thumbnail queue finished".into(); };
        let (count, percent, extracted) = progress.snapshot();
        let status = if count == 1 && extracted == 0 { "Preparing thumbnails".into() }
            else { format!("Generating up to {count} thumbnails ({percent}% done, {extracted} generated)") };
        format!("{status}\n{filename} ({index}/{total})")
    }
}

#[derive(Default)]
pub(super) struct PlaylistPreviews {
    pub hover: Option<Hover>,
    pub pointer: Option<Point<Pixels>>,
    entries: VecDeque<(Key, f64, Option<Preview>)>,
    full: Option<(Key, Vec<Preview>)>,
    recent: VecDeque<(Key, Vec<Preview>)>,
    partial: Vec<Preview>,
    hover_worker: Option<HoverJob>,
    hover_wake: Option<futures::channel::mpsc::UnboundedSender<()>>,
    pending: VecDeque<(Key, f64)>,
    manual: VecDeque<(PathBuf, Option<usize>)>,
    batch_total: usize,
    missing_only: Vec<(PathBuf, Option<usize>)>,
    activity: Arc<QueueActivity>,
    enabled: Option<bool>,
    worker: Option<Job>,
    inline_worker: Option<InlineJob>,
    directory: Option<PathBuf>,
    mode: Option<PlaylistThumbnails>,
    retired: Vec<ImageId>,
    maintenance_pending: bool,
    completed: Option<(Key, f64)>,
    message: Option<String>,
}
impl PlaylistPreviews {
    pub fn with_wake(wake: futures::channel::mpsc::UnboundedSender<()>) -> Self {
        Self { hover_wake: Some(wake), ..Default::default() }
    }
    pub fn mode(settings: &Settings) -> PlaylistThumbnails {
        if settings.timeline_thumbnails { settings.playlist_thumbnails } else { PlaylistThumbnails::None }
    }
    pub fn retain_visible(&mut self, visible: &[Key]) { self.pending.retain(|(key, _)| visible.contains(key)); }

    pub fn inline_pair(&mut self, key: &Key, target: f64, default: f64) -> Option<Preview> {
        let chosen = self.inline(key, target);
        if target == default { return chosen; }
        let original = self.inline(key, default);
        chosen.or(original)
    }
    fn inline(&mut self, key: &Key, target: f64) -> Option<Preview> {
        let mut known_missing = false;
        if let Some(index) = self.entries.iter().position(|(k, t, _)| k == key && *t == target) {
            let entry = self.entries.remove(index)?;
            let image = entry.2.clone();
            self.entries.push_back(entry);
            if image.is_some() { return image; }
            known_missing = true;
        }
        if let Some(p) = self.full_scrub(key, target).cloned() {
            self.remember(key.clone(), target, Some(p.clone()));
            return Some(p);
        }
        if !known_missing && self.pending.len() < 192 && !self.pending.iter().any(|(k, t)| k == key && *t == target)
            && !self.inline_worker.as_ref().is_some_and(|w| w.requests.iter().any(|(k, t)| k == key && *t == target)) {
            self.pending.push_back((key.clone(), target));
        }
        None
    }
    fn remember(&mut self, key: Key, target: f64, preview: Option<Preview>) {
        let old = self.entries.iter().position(|(k, t, _)| k == &key && *t == target).and_then(|i| self.entries.remove(i));
        let preview = preview.or_else(|| old.as_ref().and_then(|(_, _, p)| p.clone()));
        let preview = preview.map(|p| self.entries.iter().find_map(|(k, _, existing)| {
            if k == &key { existing.as_ref().filter(|existing| existing.time == p.time).cloned() } else { None }
        }).unwrap_or(p));
        self.entries.push_back((key, target, preview));
        if let Some((_, _, Some(p))) = old { self.retire_if_unused(p.image.id); }
        if self.entries.len() > 192 && let Some((_, _, Some(p))) = self.entries.pop_front() {
            self.retire_if_unused(p.image.id);
        }
    }
    pub fn remember_selection(&mut self, key: Key, time: f64, preview: Preview) {
        self.pending.retain(|(k, t)| k != &key || *t != time);
        self.remember(key, time, Some(preview));
    }
    fn retire_if_unused(&mut self, id: ImageId) {
        if !self.entries.iter().any(|(_, _, p)| p.as_ref().is_some_and(|p| p.image.id == id))
            && !self.full.as_ref().is_some_and(|(_, images)| images.iter().any(|p| p.image.id == id))
            && !self.recent.iter().any(|(_, images)| images.iter().any(|p| p.image.id == id))
            && !self.partial.iter().any(|p| p.image.id == id)
            && !self.retired.contains(&id) {
            self.retired.push(id);
        }
    }
    pub fn scrub(&self, key: &Key, time: f64) -> Option<&Preview> {
        self.full_scrub(key, time).or_else(|| {
            if self.hover_worker.as_ref().is_some_and(|job| &job.key == key) {
                self.partial.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
            } else { None }
        })
    }
    fn full_scrub(&self, key: &Key, time: f64) -> Option<&Preview> {
        let (_, images) = self.full.iter().chain(self.recent.iter()).find(|(k, _)| k == key)?;
        images.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
    }
    pub fn regenerate(&mut self, path: PathBuf, stream: Option<usize>) {
        let active = self.worker.as_ref().is_some_and(|w| matches!(&w.kind, JobKind::Generate(p, st) if p == &path && *st == stream));
        self.missing_only.retain(|(p, st)| p != &path || *st != stream);
        if active || self.manual.iter().any(|(p, st)| p == &path && *st == stream) { return; }
        if self.manual.is_empty() && !self.worker.as_ref().is_some_and(|w| w.progress.is_some()) { self.batch_total = 0; }
        self.manual.push_back((path, stream));
        self.batch_total += 1;
        self.refresh_activity();
    }
    pub fn generate_missing(&mut self, path: PathBuf, stream: Option<usize>) {
        let request = (path.clone(), stream);
        if self.generating_for(&path, stream) || self.manual.contains(&request) { return; }
        self.regenerate(path, stream);
        self.missing_only.push(request);
    }
    pub fn invalidate(&mut self, path: &std::path::Path) {
        self.pending.retain(|(k, _)| k.path.as_path() != path);
        if self.inline_worker.as_ref().is_some_and(|w| w.requests.iter().any(|(k, _)| k.path.as_path() == path)) { self.inline_worker = None; }
        if self.hover_worker.as_ref().is_some_and(|w| w.key.path.as_path() == path) { self.cancel_hover(); }
        let mut kept = VecDeque::new();
        let mut removed = Vec::new();
        for (k, target, p) in self.entries.drain(..) {
            if k.path.as_path() == path { if let Some(p) = p { removed.push(p.image.id); } }
            else { kept.push_back((k, target, p)); }
        }
        self.entries = kept;
        if self.full.as_ref().is_some_and(|(k, _)| k.path.as_path() == path) { self.drop_full(); }
        self.recent.retain(|(k, images)| {
            if k.path.as_path() == path { removed.extend(images.iter().map(|p| p.image.id)); false } else { true }
        });
        for id in removed { self.retire_if_unused(id); }
    }
    fn drop_full(&mut self) {
        if let Some((_, images)) = self.full.take() { for p in images { self.retire_if_unused(p.image.id); } }
    }
    fn replace_full(&mut self, key: Key, images: Vec<Preview>) {
        if let Some((old_key, old_images)) = self.full.take() {
            if old_key != key && !old_images.is_empty() {
                self.recent.push_back((old_key, old_images));
            } else {
                for p in old_images { self.retire_if_unused(p.image.id); }
            }
        }
        self.full = Some((key, images));
        // Keep at most two older sets and at most 32 MiB of additional pixels.
        while self.recent.len() > 2 || self.recent.iter().flat_map(|(_, images)| images).map(|p| u64::from(p.width) * u64::from(p.height) * 4).sum::<u64>() > 32 * 1024 * 1024 {
            if let Some((_, images)) = self.recent.pop_front() {
                for p in images { self.retire_if_unused(p.image.id); }
            }
        }
    }
    pub fn resume_hover(&mut self) {
        if let Some(h) = self.hover.as_mut() {
            if let Some(pos) = self.pointer.filter(|p| h.bounds.contains(p)) {
                let width = f32::from(h.bounds.size.width);
                if width > 0.0 { h.fraction = (f32::from(pos.x - h.bounds.left()) / width).clamp(0.0, 1.0); }
            } else { self.hover = None; }
        }
    }
    fn refresh_activity(&self) {
        let active = self.worker.as_ref().and_then(|worker| {
            let JobKind::Generate(path, _) = &worker.kind;
            Some(ActiveGeneration { progress: worker.progress.clone()?, filename: path.file_name().unwrap_or_default().to_string_lossy().into_owned(), index: self.batch_total.saturating_sub(self.manual.len()), total: self.batch_total })
        });
        *self.activity.0.lock() = active;
    }
    pub fn progress(&self) -> Option<Arc<QueueActivity>> {
        self.activity.0.lock().is_some().then(|| self.activity.clone())
    }
    pub fn generating_for(&self, path: &std::path::Path, stream: Option<usize>) -> bool {
        self.worker.as_ref().is_some_and(|w| matches!(&w.kind, JobKind::Generate(p, s) if p.as_path() == path && (s.is_none() || stream.is_none() || *s == stream)))
    }
    /// Read the visible representatives as a batch, separately from the
    /// potentially slow FFmpeg generation and full scrub-preview jobs.
    fn update_inline(&mut self, visible: bool) -> bool {
        if !visible {
            self.inline_worker = None;
            self.pending.clear();
            return false;
        }
        let mut changed = false;
        if let Some(mut job) = self.inline_worker.take() {
            let mut finished = false;
            loop {
                match job.result.try_recv() {
                    Ok((key, target, image)) => {
                        job.requests.retain(|(k, t)| k != &key || *t != target);
                        self.remember(key, target, image.map(Preview::from_bgra));
                        changed = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => { finished = true; break; }
                }
            }
            if !finished { self.inline_worker = Some(job); }
        }
        if self.inline_worker.is_none() && !self.pending.is_empty() {
            let requests: Vec<_> = self.pending.drain(..).collect();
            let batch = requests.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = cancel.clone();
            let (tx, result) = std::sync::mpsc::channel();
            match std::thread::Builder::new().name("playlist-representatives".into()).spawn(move || {
                for (key, target) in batch {
                    if stop.load(Ordering::Relaxed) { break; }
                    let image = load_cached(&key.path, key.spec(), &key.directory, Some(target)).ok().and_then(|mut images| images.pop()).map(to_bgra);
                    if tx.send((key, target, image)).is_err() { break; }
                }
            }) {
                Ok(_) => self.inline_worker = Some(InlineJob { requests, cancel, result }),
                Err(err) => { log::debug!("could not start representative lookup: {err}"); }
            }
        }
        changed
    }

    fn cancel_hover(&mut self) {
        self.hover_worker = None;
        let images = std::mem::take(&mut self.partial);
        for p in images { self.retire_if_unused(p.image.id); }
    }

    /// Called directly by pointer events and by worker wake-ups, not just the UI tick.
    pub fn update_hover(&mut self, settings: &Settings, visible: bool, timeline: &TimelineThumbnails) -> bool {
        let wanted = self.hover.as_ref().filter(|h| visible && Self::mode(settings) != PlaylistThumbnails::None
            && h.key.directory == settings.thumbnail_cache_dir()
            && timeline.for_playlist(&h.key.path, h.key.stream, h.key.count, 0.0).is_none()
            && !timeline.busy_for(&h.key.path, Some(h.key.stream)))
            .map(|h| (h.key.clone(), h.duration * f64::from(h.fraction)));
        let Some((key, time)) = wanted else { self.cancel_hover(); return false; };
        if self.hover_worker.as_ref().is_some_and(|job| job.key != key) { self.cancel_hover(); }
        let mut changed = false;
        if let Some(job) = self.hover_worker.take() {
            job.desired.store(time.to_bits(), Ordering::Relaxed);
            let mut finished = None;
            loop {
                match job.result.try_recv() {
                    Ok(Ok(Some(thumb))) => { self.partial.push(Preview::from_bgra(thumb)); changed = true; }
                    Ok(Ok(None)) => { finished = Some(true); break; }
                    Ok(Err(err)) => { log::debug!("playlist hover cache: {err}"); finished = Some(false); break; }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => { finished = Some(false); break; }
                }
            }
            if let Some(success) = finished {
                if success {
                    let images = std::mem::take(&mut self.partial);
                    self.replace_full(key.clone(), images);
                } else {
                    self.cancel_hover();
                    self.replace_full(key.clone(), Vec::new());
                }
                changed = true;
            } else { self.hover_worker = Some(job); }
        }
        if self.hover_worker.is_none() && self.full.as_ref().is_none_or(|(k, _)| k != &key)
            && let Some(index) = self.recent.iter().position(|(k, _)| k == &key)
            && let Some((key, images)) = self.recent.remove(index) {
            self.replace_full(key, images);
            changed = true;
        }
        if self.hover_worker.is_none() && self.full.as_ref().is_none_or(|(k, _)| k != &key) {
            let cancel = Arc::new(AtomicBool::new(false));
            let desired = Arc::new(AtomicU64::new(time.to_bits()));
            let (stop, target, work, wake) = (cancel.clone(), desired.clone(), key.clone(), self.hover_wake.clone());
            let (tx, result) = std::sync::mpsc::channel();
            if std::thread::Builder::new().name("playlist-hover".into()).spawn(move || {
                let mut count = 0;
                let outcome = stream_cached(&work.path, work.spec(), &work.directory, &target, &stop, |thumb| {
                    if stop.load(Ordering::Relaxed) || tx.send(Ok(Some(to_bgra(thumb)))).is_err() { return false; }
                    count += 1;
                    if (count == 1 || count % 8 == 0) && let Some(wake) = &wake { let _ = wake.unbounded_send(()); }
                    true
                });
                let _ = tx.send(outcome.map(|_| None));
                if let Some(wake) = &wake { let _ = wake.unbounded_send(()); }
            }).is_ok() { self.hover_worker = Some(HoverJob { key, cancel, desired, result }); }
        }
        changed
    }

    pub fn update(&mut self, settings: &Settings, visible: bool, timeline: &TimelineThumbnails, _current: &PlaybackState) -> (bool, Vec<ImageId>) {
        let mode = Self::mode(settings);
        let directory = settings.thumbnail_cache_dir();
        let mut changed = false;
        if self.directory.as_ref() != Some(&directory) || self.mode != Some(mode) || self.enabled != Some(settings.timeline_thumbnails) {
            let directory_changed = self.directory.as_ref().is_some_and(|d| d != &directory);
            self.directory = Some(directory.clone());
            self.mode = Some(mode);
            self.enabled = Some(settings.timeline_thumbnails);
            if directory_changed || !settings.timeline_thumbnails {
                self.clear();
            } else if mode == PlaylistThumbnails::None {
                self.hover = None;
                self.pending.clear();
                self.cancel_hover();
            }
            changed = true;
        }
        if !visible {
            self.hover = None;
            self.pending.clear();
            self.cancel_hover();
        }
        if !settings.timeline_thumbnails { return (changed, std::mem::take(&mut self.retired)); }
        changed |= self.update_inline(visible && mode.inline());
        changed |= self.update_hover(settings, visible, timeline);
        if let Some(worker) = &self.worker {
            let result = match worker.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return (changed, std::mem::take(&mut self.retired)),
                Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("playlist thumbnail worker disconnected")),
            };
            let kind = worker.kind.clone();
            self.worker = None;
            match kind {
                JobKind::Generate(path, _) => {
                    match result {
                        Ok(JobOutput::Media(key, images, Some(duration))) => {
                            self.invalidate(&path);
                            self.completed = Some((key.clone(), duration));
                            if mode != PlaylistThumbnails::None && self.hover.as_ref().is_none_or(|h| h.key == key) {
                                self.replace_full(key, images.into_iter().map(Preview::from_bgra).collect());
                            }
                        }
                        Ok(JobOutput::Existing(key, target, image)) => {
                            // A cache may have been created by another instance since
                            // the last lookup. Discard only negative entries for it.
                            self.entries.retain(|(k, _, p)| k != &key || p.is_some());
                            if self.full.as_ref().is_some_and(|(k, images)| k == &key && images.is_empty()) { self.drop_full(); }
                            self.remember(key, target, Some(Preview::from_bgra(image)));
                        }
                        Err(err) => {
                            log::debug!("playlist thumbnail generation: {err}");
                            self.message = Some(format!("Thumbnail generation failed: {err}"));
                        }
                        _ => {},
                    }
                    self.maintenance_pending = true;
                }
            }
            changed = true;
        }
        // Only explicit generation requests enter this queue. Hover has its own worker.
        let kind = if let Some((path, stream)) = self.manual.front() {
            if timeline.busy_for(path, *stream) { None }
            else { self.manual.pop_front().map(|(path, stream)| JobKind::Generate(path, stream)) }
        } else { None };
        if let Some(kind) = kind {
            let work = kind.clone();
            let JobKind::Generate(path, stream) = &kind;
            let missing_only = self.missing_only.iter().position(|(p, st)| p == path && st == stream).map(|i| self.missing_only.remove(i)).is_some();
            let cfg = settings.engine_config();
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = cancel.clone();
            let progress = Some(Arc::new(GenerationProgress::new(1)));
            let report = progress.clone();
            let (tx, result) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("playlist-thumbnails".into()).spawn(move || {
                let result = (|| -> JobResult {
                    match work {
                        JobKind::Generate(path, selected_stream) => {
                            let probe = crate::engine::probe::probe(&MediaSource::File(path.clone()), &cfg).ok_or_else(|| anyhow::anyhow!("could not probe media"))?;
                            let Some(stream) = selected_stream.or(probe.video_stream) else {
                                if missing_only { return Ok(JobOutput::Skipped); }
                                return Err(anyhow::anyhow!("no video stream"));
                            };
                            let key = Key { path, stream, count: crate::engine::thumbnails::sample_count(probe.duration), directory };
                            let target = probe.duration * 0.05;
                            if missing_only && let Ok(mut images) = load_cached(&key.path, key.spec(), &key.directory, Some(target))
                                && let Some(image) = images.pop() {
                                return Ok(JobOutput::Existing(key, target, to_bgra(image)));
                            }
                            let progress = report.as_ref().expect("manual generation progress");
                            progress.set_total(key.count);
                            let images = load_or_generate(&key.path, &cfg, key.spec(), &key.directory, &stop, true, &AtomicBool::new(false), progress)?;
                            Ok(JobOutput::Media(key, images.into_iter().map(to_bgra).collect(), Some(probe.duration)))
                        }
                    }
                })();
                let _ = tx.send(result);
            });
            match spawned {
                Ok(_) => { self.worker = Some(Job { kind, cancel, progress, result }); changed = true; }
                Err(err) => {
                    self.message = Some(format!("Could not start thumbnail generation: {err}"));
                    changed = true;
                }
            }
        }
        self.refresh_activity();
        (changed, std::mem::take(&mut self.retired))
    }
    pub fn take_message(&mut self) -> Option<String> { self.message.take() }
    pub fn take_completed(&mut self) -> Option<(Key, f64)> { self.completed.take() }
    pub fn clear(&mut self) {
        self.worker = None;
        self.cancel_hover();
        self.inline_worker = None;
        self.manual.clear();
        self.missing_only.clear();
        self.batch_total = 0;
        self.pending.clear();
        self.hover = None;
        self.drop_full();
        let recent = std::mem::take(&mut self.recent);
        for (_, images) in recent { for p in images { self.retire_if_unused(p.image.id); } }
        let removed: Vec<_> = self.entries.drain(..).filter_map(|(_, _, p)| p.map(|p| p.image.id)).collect();
        for id in removed { self.retire_if_unused(id); }
        self.refresh_activity();
    }
    pub fn take_maintenance(&mut self) -> bool { std::mem::take(&mut self.maintenance_pending) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_representative_pair_survives_inline_mode_switch() {
        let mut settings = Settings::default();
        settings.playlist_thumbnails = PlaylistThumbnails::Inline;
        settings.timeline_thumbnails = true;
        let key = Key { path: "movie.mp4".into(), stream: 0, count: 20, directory: settings.thumbnail_cache_dir() };
        let make = |time| Preview::from_bgra(Thumbnail { time, image: image::RgbaImage::new(2, 2) });
        let default = make(5.0);
        let chosen = make(35.0);
        let mut previews = PlaylistPreviews { directory: Some(key.directory.clone()), mode: Some(PlaylistThumbnails::Inline), enabled: Some(true), ..Default::default() };
        previews.full = Some((key.clone(), vec![default.clone(), chosen.clone()]));
        assert!(Arc::ptr_eq(&previews.inline_pair(&key, 35., 5.).unwrap().image, &chosen.image));
        previews.drop_full();
        settings.playlist_thumbnails = PlaylistThumbnails::InlineSide;
        let (_, retired) = previews.update(&settings, true, &TimelineThumbnails::default(), &PlaybackState::default());
        assert!(retired.is_empty());
        assert!(Arc::ptr_eq(&previews.inline_pair(&key, 5., 5.).unwrap().image, &default.image));
        assert!(Arc::ptr_eq(&previews.inline_pair(&key, 35., 5.).unwrap().image, &chosen.image));
        assert!(previews.pending.is_empty());
    }

    #[test]
    fn recent_scrub_sets_are_reused_and_invalidated() {
        let mut previews = PlaylistPreviews::default();
        let make_key = |path: &str| Key { path: path.into(), stream: 0, count: 20, directory: "cache".into() };
        let make = |time| Preview::from_bgra(Thumbnail { time, image: image::RgbaImage::new(2, 2) });
        let a = make_key("a.mp4");
        let b = make_key("b.mp4");
        let original = make(5.0);
        previews.replace_full(a.clone(), vec![original.clone()]);
        previews.replace_full(b, vec![make(10.0)]);
        assert!(Arc::ptr_eq(&previews.scrub(&a, 5.0).unwrap().image, &original.image));
        previews.invalidate(&a.path);
        assert!(previews.scrub(&a, 5.0).is_none());
    }

    #[test]
    fn missing_thumbnail_requests_preserve_forced_regeneration() {
        let mut previews = PlaylistPreviews::default();
        let path = PathBuf::from("movie.mp4");
        let request = (path.clone(), Some(0));
        previews.generate_missing(path.clone(), Some(0));
        previews.generate_missing(path.clone(), Some(0));
        assert_eq!(previews.manual.len(), 1);
        assert_eq!(previews.missing_only, vec![request.clone()]);
        previews.regenerate(path.clone(), Some(0));
        previews.generate_missing(path, Some(0));
        assert_eq!(previews.manual.len(), 1);
        assert!(previews.missing_only.is_empty());
        assert_eq!(previews.manual.pop_front(), Some(request));
    }

    #[test]
    fn queued_thumbnail_requests_do_not_cancel_current_generation() {
        let mut previews = PlaylistPreviews::default();
        let a = PathBuf::from("first.mp4");
        let b = PathBuf::from("second.mp4");
        let c = PathBuf::from("third.mp4");
        previews.regenerate(a.clone(), Some(0));
        let (path, stream) = previews.manual.pop_front().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (_tx, result) = std::sync::mpsc::channel();
        previews.worker = Some(Job { kind: JobKind::Generate(path, stream), cancel: cancel.clone(), progress: Some(Arc::new(GenerationProgress::new(20))), result });
        previews.regenerate(b.clone(), Some(0));
        previews.regenerate(c.clone(), Some(0));
        previews.regenerate(a, Some(0));
        previews.regenerate(b.clone(), Some(0));
        assert!(!cancel.load(Ordering::Relaxed));
        assert_eq!(previews.batch_total, 3);
        assert_eq!(previews.manual.pop_front(), Some((b, Some(0))));
        assert_eq!(previews.manual.pop_front(), Some((c, Some(0))));
    }
}
