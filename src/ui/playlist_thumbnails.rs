//! Bounded, cache-only playlist previews. Explicit regeneration uses a separate worker.
use super::thumbnails::{Preview, TimelineThumbnails};
use crate::engine::{MediaSource, PlaybackState, thumbnails::{GenerationProgress, Thumbnail, ThumbnailSpec, cache::{load_cached, load_or_generate}}};
use crate::playlist::PlaylistItem;
use crate::settings::{PlaylistThumbnails, Settings};
use gpui::{Bounds, ImageId, Pixels, Point};
use std::{collections::VecDeque, path::PathBuf, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::{Receiver, TryRecvError}}};

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
type JobResult = anyhow::Result<(Key, Vec<Thumbnail>, Option<f64>)>;

struct Job {
    kind: JobKind,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<GenerationProgress>>,
    result: Receiver<JobResult>,
}
impl Drop for Job { fn drop(&mut self) { self.cancel.store(true, Ordering::Relaxed); } }
#[derive(Clone)]
enum JobKind { Inline(Key, f64), Hover(Key), Generate(PathBuf, Option<usize>) }

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
    pending: VecDeque<(Key, f64)>,
    manual: VecDeque<(PathBuf, Option<usize>)>,
    batch_total: usize,
    activity: Arc<QueueActivity>,
    enabled: Option<bool>,
    worker: Option<Job>,
    directory: Option<PathBuf>,
    mode: Option<PlaylistThumbnails>,
    retired: Vec<ImageId>,
    maintenance_pending: bool,
    completed: Option<(Key, f64)>,
    message: Option<String>,
}
impl PlaylistPreviews {
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
        if let Some(p) = self.scrub(key, target).cloned() {
            self.remember(key.clone(), target, Some(p.clone()));
            return Some(p);
        }
        if !known_missing && self.pending.len() < 192 && !self.pending.iter().any(|(k, t)| k == key && *t == target)
            && !self.worker.as_ref().is_some_and(|w| matches!(&w.kind, JobKind::Inline(k, t) if k == key && *t == target)) {
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
            && !self.retired.contains(&id) {
            self.retired.push(id);
        }
    }
    pub fn scrub(&self, key: &Key, time: f64) -> Option<&Preview> {
        let (k, images) = self.full.as_ref()?;
        if k != key { return None; }
        images.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
    }
    pub fn regenerate(&mut self, path: PathBuf, stream: Option<usize>) {
        let active = self.worker.as_ref().is_some_and(|w| matches!(&w.kind, JobKind::Generate(p, st) if p == &path && *st == stream));
        if active || self.manual.iter().any(|(p, st)| p == &path && *st == stream) { return; }
        if self.manual.is_empty() && !self.worker.as_ref().is_some_and(|w| w.progress.is_some()) { self.batch_total = 0; }
        self.manual.push_back((path, stream));
        self.batch_total += 1;
        if self.worker.as_ref().is_some_and(|w| w.progress.is_none()) { self.worker = None; }
        self.refresh_activity();
    }
    pub fn invalidate(&mut self, path: &std::path::Path) {
        self.pending.retain(|(k, _)| k.path.as_path() != path);
        if self.worker.as_ref().is_some_and(|w| match &w.kind { JobKind::Inline(k, _) | JobKind::Hover(k) => k.path.as_path() == path, JobKind::Generate(..) => false }) { self.worker = None; }
        let mut kept = VecDeque::new();
        let mut removed = Vec::new();
        for (k, target, p) in self.entries.drain(..) {
            if k.path.as_path() == path { if let Some(p) = p { removed.push(p.image.id); } }
            else { kept.push_back((k, target, p)); }
        }
        self.entries = kept;
        if self.full.as_ref().is_some_and(|(k, _)| k.path.as_path() == path) { self.drop_full(); }
        for id in removed { self.retire_if_unused(id); }
    }
    fn drop_full(&mut self) {
        if let Some((_, images)) = self.full.take() { for p in images { self.retire_if_unused(p.image.id); } }
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
            let JobKind::Generate(path, _) = &worker.kind else { return None; };
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
    pub fn update(&mut self, settings: &Settings, visible: bool, timeline: &TimelineThumbnails, current: &PlaybackState) -> (bool, Vec<ImageId>) {
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
                if self.worker.as_ref().is_some_and(|w| w.progress.is_none()) { self.worker = None; }
            }
            changed = true;
        }
        if !visible {
            self.hover = None;
            self.pending.clear();
            if self.worker.as_ref().is_some_and(|w| w.progress.is_none()) { self.worker = None; }
        }
        if !settings.timeline_thumbnails { return (changed, std::mem::take(&mut self.retired)); }
        if let Some(worker) = &self.worker {
            let result = match worker.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return (changed, std::mem::take(&mut self.retired)),
                Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("playlist thumbnail worker disconnected")),
            };
            let kind = worker.kind.clone();
            self.worker = None;
            match kind {
                JobKind::Inline(key, target) => {
                    let preview = result.ok().and_then(|(_, mut images, _)| images.pop()).map(Preview::from_bgra);
                    self.remember(key, target, preview);
                }
                JobKind::Hover(key) => {
                    self.drop_full();
                    self.full = Some((key, result.map(|(_, images, _)| images.into_iter().map(Preview::from_bgra).collect()).unwrap_or_default()));
                }
                JobKind::Generate(path, _) => {
                    match result {
                        Ok((key, images, Some(duration))) => {
                            self.invalidate(&path);
                            self.completed = Some((key.clone(), duration));
                            if mode != PlaylistThumbnails::None {
                                self.drop_full();
                                self.full = Some((key, images.into_iter().map(Preview::from_bgra).collect()));
                            }
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
        // Manual generation takes precedence; visible rows never generate media.
        let kind = if let Some((path, stream)) = self.manual.front() {
            if timeline.busy_for(path, *stream) { None }
            else { self.manual.pop_front().map(|(path, stream)| JobKind::Generate(path, stream)) }
        }
        else if let Some(h) = &self.hover && mode != PlaylistThumbnails::None
            && self.full.as_ref().is_none_or(|(k, _)| k != &h.key)
            && timeline.for_playlist(&h.key.path, h.key.stream, h.key.count, 0.0).is_none()
            && !matches!(&current.source, Some(MediaSource::File(path)) if path == &h.key.path && timeline.generating())
        { Some(JobKind::Hover(h.key.clone())) }
        else if mode.inline() { self.pending.pop_front().map(|(k, d)| JobKind::Inline(k, d)) }
        else { None };
        if let Some(kind) = kind {
            let work = kind.clone();
            let cfg = settings.engine_config();
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = cancel.clone();
            let progress = matches!(&kind, JobKind::Generate(..)).then(|| Arc::new(GenerationProgress::new(1)));
            let report = progress.clone();
            let (tx, result) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("playlist-thumbnails".into()).spawn(move || {
                let result = (|| -> JobResult {
                    match work {
                        JobKind::Inline(key, target) => { let images = load_cached(&key.path, key.spec(), &key.directory, Some(target))?; Ok((key, images, None)) }
                        JobKind::Hover(key) => { let images = load_cached(&key.path, key.spec(), &key.directory, None)?; Ok((key, images, None)) }
                        JobKind::Generate(path, selected_stream) => {
                            let probe = crate::engine::probe::probe(&MediaSource::File(path.clone()), &cfg).ok_or_else(|| anyhow::anyhow!("could not probe media"))?;
                            let stream = selected_stream.or(probe.video_stream).ok_or_else(|| anyhow::anyhow!("no video stream"))?;
                            let key = Key { path, stream, count: crate::engine::thumbnails::sample_count(probe.duration), directory };
                            let progress = report.as_ref().expect("manual generation progress");
                            progress.set_total(key.count);
                            let images = load_or_generate(&key.path, &cfg, key.spec(), &key.directory, &stop, true, &AtomicBool::new(false), progress)?;
                            Ok((key, images, Some(probe.duration)))
                        }
                    }
                })().map(|(key, mut images, duration)| {
                    for thumb in &mut images {
                        let pixels: &mut [u8] = thumb.image.as_mut();
                        for pixel in pixels.as_chunks_mut::<4>().0 { pixel.swap(0, 2); }
                    }
                    (key, images, duration)
                });
                let _ = tx.send(result);
            });
            match spawned {
                Ok(_) => { self.worker = Some(Job { kind, cancel, progress, result }); changed = true; }
                Err(err) if matches!(kind, JobKind::Generate(..)) => {
                    self.message = Some(format!("Could not start thumbnail generation: {err}"));
                    changed = true;
                }
                Err(err) => log::debug!("could not start playlist thumbnail lookup: {err}"),
            }
        }
        self.refresh_activity();
        (changed, std::mem::take(&mut self.retired))
    }
    pub fn take_message(&mut self) -> Option<String> { self.message.take() }
    pub fn take_completed(&mut self) -> Option<(Key, f64)> { self.completed.take() }
    pub fn clear(&mut self) {
        self.worker = None;
        self.manual.clear();
        self.batch_total = 0;
        self.pending.clear();
        self.hover = None;
        self.drop_full();
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
