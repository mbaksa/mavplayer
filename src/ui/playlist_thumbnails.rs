//! Bounded, cache-only playlist previews. Explicit regeneration uses a separate worker.
use super::thumbnails::{Preview, TimelineThumbnails};
use crate::engine::{
    MediaSource, PlaybackState, Status, TrackId,
    thumbnails::{
        GenerationProgress, Thumbnail, ThumbnailSpec,
        cache::{
            GenerationControl, decode_representative, load_cached, load_or_generate, representative_jpeg, stream_cached,
        },
    },
};
use crate::playlist::{
    PlaylistItem,
    collections::{PlaylistId, PlaylistItemId, thumbnail_cache},
};
use crate::settings::{PlaylistThumbnails, Settings};
use gpui::{Bounds, ImageId, Pixels, Point};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, TryRecvError},
    },
};

const INLINE_BATCH: usize = 12;
pub(super) const PREFETCH_ROWS: usize = 4;

#[derive(Clone)]
pub(super) struct RepresentativeRow {
    key: Key,
    target: f64,
    item: PlaylistItemId,
}
impl RepresentativeRow {
    pub fn new(item: &PlaylistItem, settings: &Settings) -> Option<Self> {
        let key = Key::new(item, settings)?;
        let target = key.representative_time(settings, item.duration?);
        Some(Self { key, target, item: item.persistent_id })
    }
}

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
        Some(Self {
            path: path.clone(),
            stream: item.video_stream?,
            count: crate::engine::thumbnails::sample_count(duration),
            directory: settings.thumbnail_cache_dir(),
        })
    }
    pub fn selection_key(&self) -> String {
        format!("{}:{}", self.stream, self.path.to_string_lossy())
    }
    pub fn representative_time(&self, settings: &Settings, duration: f64) -> f64 {
        settings
            .playlist_thumbnail_times
            .get(&self.selection_key())
            .copied()
            .filter(|t| t.is_finite() && *t >= 0.0 && *t <= duration)
            .unwrap_or(duration * crate::engine::thumbnails::representative_fraction(&self.path))
    }
    fn spec(&self) -> ThumbnailSpec {
        ThumbnailSpec { stream: self.stream, count: self.count, width: 240, height: 135 }
    }
}

pub(super) struct Hover {
    pub id: u64,
    pub key: Key,
    pub duration: f64,
    pub fraction: f32,
    pub bounds: Bounds<Pixels>,
}
enum JobOutput {
    Media(Key, Vec<Thumbnail>, Option<f64>),
    Existing(Key, f64, Thumbnail, f64),
    Representative(Key, f64, Thumbnail, f64),
    Skipped,
}
type JobResult = anyhow::Result<JobOutput>;

struct Job {
    kind: JobKind,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<GenerationProgress>>,
    result: Receiver<JobResult>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
#[derive(Clone)]
enum JobKind {
    Generate(PathBuf, Option<usize>, bool),
}

struct HoverJob {
    key: Key,
    cancel: Arc<AtomicBool>,
    desired: Arc<AtomicU64>,
    result: Receiver<anyhow::Result<Option<Thumbnail>>>,
}
impl Drop for HoverJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct InlineJob {
    requests: Vec<(Key, f64)>,
    cancel: Arc<AtomicBool>,
    result: Receiver<(Key, f64, Option<Thumbnail>)>,
}
impl Drop for InlineJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn to_bgra(mut thumb: Thumbnail) -> Thumbnail {
    let pixels: &mut [u8] = thumb.image.as_mut();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    thumb
}

fn packed_representative(
    mut pack: Option<&mut thumbnail_cache::Pack>,
    key: &Key,
    target: f64,
    rows: &[&RepresentativeRow],
    display: bool,
) -> Option<Thumbnail> {
    let selection = thumbnail_cache::Selection { path: key.path.clone(), stream: key.stream, count: key.count, target };
    let started = std::time::Instant::now();
    if let Some(p) = pack.as_deref_mut() {
        for row in rows {
            if !display && p.contains(row.item, &selection) {
                return None;
            }
            if let Ok(Some((time, jpeg))) = p.read(row.item, &selection) {
                if !display {
                    return None;
                }
                if let Ok(image) = decode_representative(jpeg, key.spec(), time) {
                    log::trace!("playlist JPEG hit: {:?}, {:?}", key.path, started.elapsed());
                    return Some(to_bgra(image));
                }
            }
        }
    }
    log::trace!("playlist JPEG miss: {:?}", key.path);
    let (time, jpeg) = representative_jpeg(&key.path, key.spec(), &key.directory, target)
        .or_else(|_| representative_jpeg(&key.path, ThumbnailSpec { count: 1, ..key.spec() }, &key.directory, target))
        .ok()?;
    let image = if display { Some(to_bgra(decode_representative(jpeg.clone(), key.spec(), time).ok()?)) } else { None };
    if let Some(p) = pack {
        for row in rows {
            if let Err(err) = p.append(row.item, selection.clone(), time, &jpeg) {
                log::debug!("could not append playlist thumbnail: {err}");
            }
        }
    }
    image
}

/// Reuse the compressed frame if possible. A selected preview can also come
/// from a partially generated filmstrip that has not been published yet.
fn persist_selection(
    pack: &mut thumbnail_cache::Pack,
    row: &RepresentativeRow,
    sample: Option<&Thumbnail>,
) -> anyhow::Result<()> {
    let key = &row.key;
    let selection =
        thumbnail_cache::Selection { path: key.path.clone(), stream: key.stream, count: key.count, target: row.target };
    let matches_time = |time: f64| (time - row.target).abs() <= 0.000001;
    if let Ok(Some((time, _))) = pack.read(row.item, &selection) {
        if sample.is_none() || matches_time(time) {
            return Ok(());
        }
    }
    let cached = representative_jpeg(&key.path, key.spec(), &key.directory, row.target).or_else(|_| {
        representative_jpeg(&key.path, ThumbnailSpec { count: 1, ..key.spec() }, &key.directory, row.target)
    });
    let (time, jpeg) = match (cached, sample) {
        (Ok(frame), None) => frame,
        (Ok(frame), Some(_)) if matches_time(frame.0) => frame,
        (_, Some(sample)) if matches_time(sample.time) => {
            // Preview pixels are BGRA; JPEG expects RGB. Only re-encode when
            // the exact selected compressed frame is not available on disk.
            let mut pixels = sample.image.clone();
            for pixel in pixels.pixels_mut() {
                pixel.0.swap(0, 2);
            }
            let mut image = image::DynamicImage::ImageRgba8(pixels);
            let spec = key.spec();
            if image.width() > spec.width || image.height() > spec.height {
                image = image.thumbnail(spec.width, spec.height);
            }
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85).encode_image(&image.into_rgb8())?;
            (sample.time, jpeg)
        }
        (Err(err), None) => return Err(err),
        _ => anyhow::bail!("selected thumbnail time changed"),
    };
    pack.append(row.item, selection, time, &jpeg)?;
    Ok(())
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
        let Some(ActiveGeneration { progress, filename, index, total }) = activity.as_ref() else {
            return "Thumbnail queue finished".into();
        };
        let (count, percent, extracted) = progress.snapshot();
        let status = if count == 1 && extracted == 0 {
            "Preparing thumbnails".into()
        } else {
            format!("Generating up to {count} thumbnails ({percent}% done, {extracted} generated)")
        };
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
    playlist: Option<PlaylistId>,
    pub(super) pack: Arc<parking_lot::Mutex<Option<(PathBuf, PlaylistId, thumbnail_cache::Pack)>>>,
    background_rows: VecDeque<RepresentativeRow>,
    selection_writes: VecDeque<(PlaylistId, RepresentativeRow, Option<Arc<Thumbnail>>)>,
    invalidated_pack_paths: Vec<PathBuf>,
    visible_rows: Vec<RepresentativeRow>,
    prefetch_rows: Vec<RepresentativeRow>,
    manual: VecDeque<(PathBuf, Option<usize>, bool)>,
    auto_rows: std::collections::HashMap<(PathBuf, usize), (PlaylistId, RepresentativeRow)>,
    batch_total: usize,
    missing_only: Vec<(PathBuf, Option<usize>, bool)>,
    activity: Arc<QueueActivity>,
    worker: Option<Job>,
    inline_worker: Option<InlineJob>,
    directory: Option<PathBuf>,
    mode: Option<PlaylistThumbnails>,
    retired: Vec<ImageId>,
    maintenance_pending: bool,
    completed: Option<(Key, f64)>,
    pack_refresh: Option<(Key, f64, bool)>,
    message: Option<String>,
}
impl PlaylistPreviews {
    pub fn with_wake(wake: futures::channel::mpsc::UnboundedSender<()>) -> Self {
        Self { hover_wake: Some(wake), ..Default::default() }
    }
    pub fn mode(settings: &Settings) -> PlaylistThumbnails {
        settings.playlist_thumbnails
    }
    pub fn retain_visible(
        &mut self,
        playlist: Option<PlaylistId>,
        visible: Vec<RepresentativeRow>,
        prefetch: Vec<RepresentativeRow>,
    ) {
        if self.playlist != playlist {
            if let Some(worker) = &self.inline_worker {
                worker.cancel.store(true, Ordering::Relaxed);
            }
            self.pending.clear();
            self.background_rows.clear();
            self.invalidated_pack_paths.clear();
            self.pack = Arc::default();
            self.playlist = playlist;
        }
        self.pending.retain(|(key, _)| visible.iter().any(|row| &row.key == key));
        self.visible_rows = visible;
        self.prefetch_rows = prefetch;
    }

    pub fn needs_pack_rows(&self, playlist: Option<PlaylistId>) -> bool {
        playlist.is_some() && self.playlist != playlist
    }

    pub fn set_pack_rows(&mut self, rows: Vec<RepresentativeRow>) {
        self.background_rows = rows.into();
    }

    pub fn refresh_pack_rows(&mut self, path: &std::path::Path, rows: Vec<RepresentativeRow>, regenerated: bool) {
        if self.playlist.is_none() {
            return;
        }
        if regenerated && !self.invalidated_pack_paths.iter().any(|p| p == path) {
            self.invalidated_pack_paths.push(path.to_path_buf());
        }
        self.background_rows.retain(|row| row.key.path != path);
        for row in rows.into_iter().rev() {
            self.background_rows.push_front(row);
        }
    }

    /// Persist explicit selections independently of the decoded-image LRU and viewport.
    pub fn queue_selection_write(
        &mut self,
        playlist: PlaylistId,
        rows: Vec<RepresentativeRow>,
        sample: Option<Arc<Thumbnail>>,
    ) {
        for row in rows {
            self.selection_writes.retain(|(id, old, _)| *id != playlist || old.item != row.item);
            if self.playlist == Some(playlist) {
                for old in self.background_rows.iter_mut().chain(&mut self.visible_rows).chain(&mut self.prefetch_rows)
                {
                    if old.item == row.item {
                        *old = row.clone();
                    }
                }
            }
            self.selection_writes.push_back((playlist, row, sample.clone()));
        }
        if let Some(wake) = &self.hover_wake {
            let _ = wake.unbounded_send(());
        }
    }

    fn start_selection_write(&mut self) {
        let Some((playlist, first, sample)) = self.selection_writes.pop_front() else {
            return;
        };
        let directory = first.key.directory.clone();
        let mut rows = vec![(first, sample)];
        while rows.len() < INLINE_BATCH
            && self
                .selection_writes
                .front()
                .is_some_and(|(id, row, _)| *id == playlist && row.key.directory == directory)
        {
            let (_, row, sample) = self.selection_writes.pop_front().unwrap();
            rows.push((row, sample));
        }
        let retry = rows.clone();
        let pack_slot = self.pack.clone();
        let active = self.playlist == Some(playlist);
        let mut invalidations = Vec::new();
        if active {
            self.invalidated_pack_paths.retain(|path| {
                if rows.iter().any(|(row, _)| &row.key.path == path) {
                    invalidations.push(path.clone());
                    false
                } else {
                    true
                }
            });
        }
        let retry_invalidations = invalidations.clone();
        let wake = self.hover_wake.clone();
        let (tx, result) = std::sync::mpsc::channel();
        match std::thread::Builder::new().name("playlist-selection-write".into()).spawn(move || {
            let mut slot = pack_slot.lock();
            let matching = slot.as_ref().is_some_and(|(dir, id, _)| *dir == directory && *id == playlist);
            let mut temporary = None;
            if !matching {
                match thumbnail_cache::Pack::open(&directory, playlist) {
                    Ok(pack) if active => *slot = Some((directory.clone(), playlist, pack)),
                    Ok(pack) => temporary = Some(pack),
                    Err(err) => log::debug!("could not open playlist for thumbnail selection: {err}"),
                }
            }
            let pack = if matching || active {
                slot.as_mut().filter(|(dir, id, _)| *dir == directory && *id == playlist).map(|(_, _, pack)| pack)
            } else {
                temporary.as_mut()
            };
            if let Some(pack) = pack {
                for path in invalidations {
                    if let Err(err) = pack.invalidate_path(&path) {
                        log::debug!("could not invalidate playlist thumbnails: {err}");
                    }
                }
                for (row, sample) in rows {
                    if let Err(err) = persist_selection(pack, &row, sample.as_deref()) {
                        log::debug!("could not save selected playlist thumbnail: {err}");
                    }
                }
            }
            drop(temporary);
            drop(slot);
            drop(tx);
            if let Some(wake) = &wake {
                let _ = wake.unbounded_send(());
            }
        }) {
            Ok(_) => {
                self.inline_worker =
                    Some(InlineJob { requests: Vec::new(), cancel: Arc::new(AtomicBool::new(false)), result })
            }
            Err(err) => {
                for (row, sample) in retry.into_iter().rev() {
                    self.selection_writes.push_front((playlist, row, sample));
                }
                self.invalidated_pack_paths.extend(retry_invalidations);
                log::debug!("could not start playlist thumbnail selection write: {err}");
            }
        }
    }

    pub fn inline_pair(&mut self, key: &Key, target: f64, default: f64) -> Option<Preview> {
        let chosen = self.inline(key, target);
        if target == default {
            return chosen;
        }
        let original = self.inline(key, default);
        chosen.or(original)
    }
    fn inline(&mut self, key: &Key, target: f64) -> Option<Preview> {
        let mut known_missing = false;
        if let Some(index) = self.entries.iter().position(|(k, t, _)| k == key && *t == target) {
            let entry = self.entries.remove(index)?;
            let image = entry.2.clone();
            self.entries.push_back(entry);
            if image.is_some() {
                return image;
            }
            known_missing = true;
        }
        if let Some(p) = self.full_scrub(key, target).cloned() {
            self.remember(key.clone(), target, Some(p.clone()));
            return Some(p);
        }
        if !known_missing
            && self.pending.len() < 192
            && !self.pending.iter().any(|(k, t)| k == key && *t == target)
            && !self.inline_worker.as_ref().is_some_and(|w| {
                !w.cancel.load(Ordering::Relaxed) && w.requests.iter().any(|(k, t)| k == key && *t == target)
            })
        {
            let was_empty = self.pending.is_empty();
            self.pending.push_back((key.clone(), target));
            if was_empty && let Some(wake) = &self.hover_wake {
                let _ = wake.unbounded_send(());
            }
        }
        None
    }
    fn remember(&mut self, key: Key, target: f64, preview: Option<Preview>) {
        let old =
            self.entries.iter().position(|(k, t, _)| k == &key && *t == target).and_then(|i| self.entries.remove(i));
        let preview = preview.or_else(|| old.as_ref().and_then(|(_, _, p)| p.clone()));
        let preview = preview.map(|p| {
            self.entries
                .iter()
                .find_map(|(k, _, existing)| {
                    if k == &key { existing.as_ref().filter(|existing| existing.time == p.time).cloned() } else { None }
                })
                .unwrap_or(p)
        });
        self.entries.push_back((key, target, preview));
        if let Some((_, _, Some(p))) = old {
            self.retire_if_unused(p.image.id);
        }
        if self.entries.len() > 192
            && let Some((_, _, Some(p))) = self.entries.pop_front()
        {
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
            && !self.retired.contains(&id)
        {
            self.retired.push(id);
        }
    }
    pub(super) fn cached_representative(&self, key: &Key, target: f64, default: f64) -> Option<&Preview> {
        self.full_scrub(key, target).or_else(|| {
            [target, default].into_iter().find_map(|time| {
                self.entries.iter().find_map(|(k, t, p)| if k == key && *t == time { p.as_ref() } else { None })
            })
        })
    }
    pub fn scrub(&self, key: &Key, time: f64) -> Option<&Preview> {
        self.full_scrub(key, time).or_else(|| {
            if self.hover_worker.as_ref().is_some_and(|job| &job.key == key) {
                self.partial.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
            } else {
                None
            }
        })
    }
    fn full_scrub(&self, key: &Key, time: f64) -> Option<&Preview> {
        let (_, images) = self.full.iter().chain(self.recent.iter()).find(|(k, _)| k == key)?;
        images.iter().min_by(|a, b| (a.time - time).abs().total_cmp(&(b.time - time).abs()))
    }
    pub fn regenerate(&mut self, path: PathBuf, stream: Option<usize>) {
        self.enqueue(path, stream, false, false);
    }
    pub fn generate_missing(&mut self, path: PathBuf, stream: Option<usize>) {
        self.enqueue(path, stream, false, true);
    }
    pub fn generate_representative(&mut self, path: PathBuf, stream: Option<usize>) {
        self.enqueue(path, stream, true, true);
    }
    pub fn generate_auto_representative(&mut self, playlist: PlaylistId, row: RepresentativeRow) {
        let key = (row.key.path.clone(), row.key.stream);
        self.auto_rows.insert(key.clone(), (playlist, row));
        self.generate_representative(key.0, Some(key.1));
    }
    fn enqueue(&mut self, path: PathBuf, stream: Option<usize>, single: bool, missing: bool) {
        let request = (path, stream, single);
        let same = |p: &PathBuf, s: Option<usize>, one: bool| {
            p == &request.0 && (s.is_none() || stream.is_none() || s == stream) && (one == single || single)
        };
        if !missing {
            self.missing_only.retain(|r| r != &request);
        }
        if self.worker.as_ref().is_some_and(|w| {
            !w.cancel.load(Ordering::Relaxed) && matches!(&w.kind, JobKind::Generate(p, s, one) if same(p, *s, *one))
        }) || self.manual.iter().any(|(p, s, one)| same(p, *s, *one))
        {
            return;
        }
        if !single {
            let before = self.manual.len();
            self.manual
                .retain(|(p, s, one)| !(*one && p == &request.0 && (s.is_none() || stream.is_none() || *s == stream)));
            self.missing_only
                .retain(|(p, s, one)| !(*one && p == &request.0 && (s.is_none() || stream.is_none() || *s == stream)));
            self.batch_total = self.batch_total.saturating_sub(before - self.manual.len());
        }
        if self.manual.is_empty() && !self.worker.as_ref().is_some_and(|w| w.progress.is_some()) {
            self.batch_total = 0;
        }
        if missing {
            self.missing_only.push(request.clone());
        }
        self.manual.push_back(request);
        self.batch_total += 1;
        self.refresh_activity();
    }
    pub fn has_generation(&self) -> bool {
        self.worker.is_some() || !self.manual.is_empty()
    }
    pub fn retain_generation(&mut self, keep: impl Fn(&std::path::Path) -> bool) -> bool {
        let before = self.manual.len();
        self.manual.retain(|(path, _, _)| keep(path));
        self.missing_only.retain(|(path, _, _)| keep(path));
        self.batch_total = self.batch_total.saturating_sub(before - self.manual.len());
        let cancelled = self.worker.as_ref().is_some_and(|w| {
            let JobKind::Generate(path, _, _) = &w.kind;
            !keep(path) && !w.cancel.swap(true, Ordering::Relaxed)
        });
        if cancelled || before != self.manual.len() {
            self.refresh_activity();
        }
        cancelled || before != self.manual.len()
    }
    pub fn invalidate(&mut self, path: &std::path::Path) {
        self.pending.retain(|(k, _)| k.path.as_path() != path);
        self.prefetch_rows.retain(|row| row.key.path.as_path() != path);
        if self.inline_worker.as_ref().is_some_and(|w| w.requests.iter().any(|(k, _)| k.path.as_path() == path)) {
            self.inline_worker = None;
        }
        if self.hover_worker.as_ref().is_some_and(|w| w.key.path.as_path() == path) {
            self.cancel_hover();
        }
        let mut kept = VecDeque::new();
        let mut removed = Vec::new();
        for (k, target, p) in self.entries.drain(..) {
            if k.path.as_path() == path {
                if let Some(p) = p {
                    removed.push(p.image.id);
                }
            } else {
                kept.push_back((k, target, p));
            }
        }
        self.entries = kept;
        if self.full.as_ref().is_some_and(|(k, _)| k.path.as_path() == path) {
            self.drop_full();
        }
        self.recent.retain(|(k, images)| {
            if k.path.as_path() == path {
                removed.extend(images.iter().map(|p| p.image.id));
                false
            } else {
                true
            }
        });
        for id in removed {
            self.retire_if_unused(id);
        }
    }
    fn drop_full(&mut self) {
        if let Some((_, images)) = self.full.take() {
            for p in images {
                self.retire_if_unused(p.image.id);
            }
        }
    }
    fn replace_full(&mut self, key: Key, images: Vec<Preview>) {
        if let Some((old_key, old_images)) = self.full.take() {
            if old_key != key && !old_images.is_empty() {
                self.recent.push_back((old_key, old_images));
            } else {
                for p in old_images {
                    self.retire_if_unused(p.image.id);
                }
            }
        }
        self.full = Some((key, images));
        // Keep at most two older sets and at most 32 MiB of additional pixels.
        while self.recent.len() > 2
            || self
                .recent
                .iter()
                .flat_map(|(_, images)| images)
                .map(|p| u64::from(p.width) * u64::from(p.height) * 4)
                .sum::<u64>()
                > 32 * 1024 * 1024
        {
            if let Some((_, images)) = self.recent.pop_front() {
                for p in images {
                    self.retire_if_unused(p.image.id);
                }
            }
        }
    }
    pub fn resume_hover(&mut self) {
        if let Some(h) = self.hover.as_mut() {
            if let Some(pos) = self.pointer.filter(|p| h.bounds.contains(p)) {
                let width = f32::from(h.bounds.size.width);
                if width > 0.0 {
                    h.fraction = (f32::from(pos.x - h.bounds.left()) / width).clamp(0.0, 1.0);
                }
            } else {
                self.hover = None;
            }
        }
    }
    fn refresh_activity(&self) {
        let active = self.worker.as_ref().filter(|w| !w.cancel.load(Ordering::Relaxed)).and_then(|worker| {
            let JobKind::Generate(path, _, _) = &worker.kind;
            Some(ActiveGeneration {
                progress: worker.progress.clone()?,
                filename: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                index: self.batch_total.saturating_sub(self.manual.len()),
                total: self.batch_total,
            })
        });
        *self.activity.0.lock() = active;
    }
    pub fn progress(&self) -> Option<Arc<QueueActivity>> {
        self.activity.0.lock().is_some().then(|| self.activity.clone())
    }
    pub fn generating_for(&self, path: &std::path::Path, stream: Option<usize>) -> bool {
        self.worker.as_ref().is_some_and(|w| matches!(&w.kind, JobKind::Generate(p, s, _) if p.as_path() == path && (s.is_none() || stream.is_none() || *s == stream)))
    }
    /// Read the visible representatives as a batch, separately from the
    /// potentially slow FFmpeg generation and full scrub-preview jobs.
    pub(super) fn update_inline(&mut self, visible: bool) -> bool {
        if !visible {
            // Let the current bounded batch finish; do not start more hidden work.
            self.pending.clear();
            self.visible_rows.clear();
            self.prefetch_rows.clear();
        }
        let mut changed = false;
        if let Some(mut job) = self.inline_worker.take() {
            let mut finished = false;
            loop {
                match job.result.try_recv() {
                    Ok((key, target, image)) => {
                        job.requests.retain(|(k, t)| k != &key || *t != target);
                        if !job.cancel.load(Ordering::Relaxed) {
                            self.remember(key, target, image.map(Preview::from_bgra));
                            changed = true;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
            if !finished {
                self.inline_worker = Some(job);
            }
        }
        if self.inline_worker.is_none() && !self.selection_writes.is_empty() {
            self.start_selection_write();
        }
        if !visible {
            return changed;
        }
        // Promote visible rows ahead of any queued prefetch.
        let visible = self.visible_rows.clone();
        for row in &visible {
            let _ = self.inline(&row.key, row.target);
        }
        self.pending
            .make_contiguous()
            .sort_by_key(|(key, target)| !visible.iter().any(|row| row.key == *key && row.target == *target));
        if self.inline_worker.is_none() && self.pending.is_empty() {
            for row in self.prefetch_rows.clone() {
                let _ = self.inline(&row.key, row.target);
            }
        }
        if self.inline_worker.is_none() && (!self.pending.is_empty() || !self.background_rows.is_empty()) {
            let count = self.pending.len().min(INLINE_BATCH);
            let requests: Vec<_> = self.pending.drain(..count).collect();
            let playlist = self.playlist;
            let rows: Vec<_> = self.visible_rows.iter().chain(&self.prefetch_rows).cloned().collect();
            let batch = requests.clone();
            let background: Vec<_> = if batch.is_empty() {
                let count = self.background_rows.len().min(INLINE_BATCH);
                self.background_rows.drain(..count).collect()
            } else {
                Vec::new()
            };
            let pack_slot = self.pack.clone();
            let invalidations = std::mem::take(&mut self.invalidated_pack_paths);
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = cancel.clone();
            let (tx, result) = std::sync::mpsc::channel();
            let wake = self.hover_wake.clone();
            match std::thread::Builder::new().name("playlist-representatives".into()).spawn(move || {
                let directory = batch
                    .first()
                    .map(|(key, _)| &key.directory)
                    .or_else(|| background.first().map(|row| &row.key.directory))
                    .unwrap()
                    .clone();
                let mut slot = pack_slot.lock();
                if let Some(id) = playlist {
                    if !slot.as_ref().is_some_and(|(dir, old, _)| dir == &directory && *old == id) {
                        *slot = thumbnail_cache::Pack::open(&directory, id).ok().map(|pack| (directory, id, pack));
                    }
                } else {
                    *slot = None;
                }
                let mut pack = slot.as_mut().map(|(_, _, pack)| pack);
                if let Some(p) = pack.as_deref_mut() {
                    for path in invalidations {
                        if let Err(err) = p.invalidate_path(&path) {
                            log::debug!("could not invalidate playlist thumbnails: {err}");
                        }
                    }
                }
                for (key, target) in batch {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let matching: Vec<_> = rows.iter().filter(|row| row.key == key && row.target == target).collect();
                    let image = packed_representative(pack.as_deref_mut(), &key, target, &matching, true);
                    if tx.send((key, target, image)).is_err() {
                        break;
                    }
                    if let Some(wake) = &wake {
                        let _ = wake.unbounded_send(());
                    }
                }
                for row in background {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = packed_representative(pack.as_deref_mut(), &row.key, row.target, &[&row], false);
                }
                drop(slot);
                drop(tx);
                if let Some(wake) = &wake {
                    let _ = wake.unbounded_send(());
                }
            }) {
                Ok(_) => self.inline_worker = Some(InlineJob { requests, cancel, result }),
                Err(err) => {
                    log::debug!("could not start representative lookup: {err}");
                }
            }
        }
        changed
    }

    fn cancel_hover(&mut self) {
        self.hover_worker = None;
        let images = std::mem::take(&mut self.partial);
        for p in images {
            self.retire_if_unused(p.image.id);
        }
    }

    /// Called directly by pointer events and by worker wake-ups, not just the UI tick.
    pub fn update_hover(
        &mut self,
        settings: &Settings,
        visible: bool,
        panel_item: Option<u64>,
        timeline: &TimelineThumbnails,
    ) -> bool {
        let wanted = self
            .hover
            .as_ref()
            .filter(|h| {
                visible
                    && (Self::mode(settings).scrubbing() || panel_item == Some(h.id))
                    && h.key.directory == settings.thumbnail_cache_dir()
                    && timeline.for_playlist(&h.key.path, h.key.stream, h.key.count, 0.0).is_none()
                    && !timeline.busy_for(&h.key.path, Some(h.key.stream))
            })
            .map(|h| (h.key.clone(), h.duration * f64::from(h.fraction)));
        let Some((key, time)) = wanted else {
            self.cancel_hover();
            return false;
        };
        if self.hover_worker.as_ref().is_some_and(|job| job.key != key) {
            self.cancel_hover();
        }
        let mut changed = false;
        if let Some(job) = self.hover_worker.take() {
            job.desired.store(time.to_bits(), Ordering::Relaxed);
            let mut finished = None;
            loop {
                match job.result.try_recv() {
                    Ok(Ok(Some(thumb))) => {
                        self.partial.push(Preview::from_bgra(thumb));
                        changed = true;
                    }
                    Ok(Ok(None)) => {
                        finished = Some(true);
                        break;
                    }
                    Ok(Err(err)) => {
                        log::debug!("playlist hover cache: {err}");
                        finished = Some(false);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some(false);
                        break;
                    }
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
            } else {
                self.hover_worker = Some(job);
            }
        }
        if self.hover_worker.is_none()
            && self.full.as_ref().is_none_or(|(k, _)| k != &key)
            && let Some(index) = self.recent.iter().position(|(k, _)| k == &key)
            && let Some((key, images)) = self.recent.remove(index)
        {
            self.replace_full(key, images);
            changed = true;
        }
        if self.hover_worker.is_none() && self.full.as_ref().is_none_or(|(k, _)| k != &key) {
            let cancel = Arc::new(AtomicBool::new(false));
            let desired = Arc::new(AtomicU64::new(time.to_bits()));
            let (stop, target, work, wake) = (cancel.clone(), desired.clone(), key.clone(), self.hover_wake.clone());
            let (tx, result) = std::sync::mpsc::channel();
            if std::thread::Builder::new()
                .name("playlist-hover".into())
                .spawn(move || {
                    let mut count = 0;
                    let outcome = stream_cached(&work.path, work.spec(), &work.directory, &target, &stop, |thumb| {
                        if stop.load(Ordering::Relaxed) || tx.send(Ok(Some(to_bgra(thumb)))).is_err() {
                            return false;
                        }
                        count += 1;
                        if (count == 1 || count % 8 == 0)
                            && let Some(wake) = &wake
                        {
                            let _ = wake.unbounded_send(());
                        }
                        true
                    });
                    let _ = tx.send(outcome.map(|_| None));
                    if let Some(wake) = &wake {
                        let _ = wake.unbounded_send(());
                    }
                })
                .is_ok()
            {
                self.hover_worker = Some(HoverJob { key, cancel, desired, result });
            }
        }
        changed
    }

    pub fn update(
        &mut self,
        settings: &Settings,
        visible: bool,
        panel_item: Option<u64>,
        timeline: &TimelineThumbnails,
        current: &PlaybackState,
    ) -> (bool, Vec<ImageId>) {
        let mode = Self::mode(settings);
        let directory = settings.thumbnail_cache_dir();
        let mut changed = false;
        if self.directory.as_ref() != Some(&directory) || self.mode != Some(mode) {
            let directory_changed = self.directory.as_ref().is_some_and(|d| d != &directory);
            self.directory = Some(directory.clone());
            self.mode = Some(mode);
            if directory_changed {
                self.clear();
            } else if !mode.scrubbing() {
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
        if !settings.timeline_thumbnails {
            let before = self.manual.len();
            self.manual.retain(|(_, _, single)| *single);
            self.missing_only.retain(|(_, _, single)| *single);
            self.batch_total = self.batch_total.saturating_sub(before - self.manual.len());
            if let Some(worker) = &self.worker
                && matches!(&worker.kind, JobKind::Generate(_, _, false))
            {
                worker.cancel.store(true, Ordering::Relaxed);
            }
            if before != self.manual.len() {
                self.refresh_activity();
                changed = true;
            }
        }
        if !mode.inline() && self.playlist.is_some() {
            self.visible_rows.clear();
            self.prefetch_rows.clear();
            self.playlist = None;
            self.background_rows.clear();
            self.invalidated_pack_paths.clear();
            self.pack = Arc::default();
        }
        changed |= self.update_inline(visible && (mode.inline() || panel_item.is_some()));
        changed |= self.update_hover(settings, visible, panel_item, timeline);
        if let Some(worker) = &self.worker {
            let result = match worker.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return (changed, std::mem::take(&mut self.retired)),
                Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("playlist thumbnail worker disconnected")),
            };
            let kind = worker.kind.clone();
            let cancelled = worker.cancel.load(Ordering::Relaxed);
            self.worker = None;
            if cancelled {
                self.maintenance_pending = true;
                self.batch_total = self.batch_total.saturating_sub(1);
                self.refresh_activity();
                return (true, std::mem::take(&mut self.retired));
            }
            match kind {
                JobKind::Generate(path, _, _) => {
                    match result {
                        Ok(JobOutput::Media(key, images, Some(duration))) => {
                            self.invalidate(&path);
                            self.completed = Some((key.clone(), duration));
                            self.pack_refresh = Some((key.clone(), duration, true));
                            if mode != PlaylistThumbnails::None && self.hover.as_ref().is_none_or(|h| h.key == key) {
                                self.replace_full(key, images.into_iter().map(Preview::from_bgra).collect());
                            }
                        }
                        Ok(JobOutput::Representative(key, target, image, duration)) => {
                            self.pack_refresh = Some((key.clone(), duration, true));
                            self.invalidate(&path);
                            self.remember(key, target, Some(Preview::from_bgra(image)));
                        }
                        Ok(JobOutput::Existing(key, target, image, duration)) => {
                            self.pack_refresh = Some((key.clone(), duration, false));
                            // A cache may have been created by another instance since
                            // the last lookup. Discard only negative entries for it.
                            self.entries.retain(|(k, _, p)| k != &key || p.is_some());
                            if self.full.as_ref().is_some_and(|(k, images)| k == &key && images.is_empty()) {
                                self.drop_full();
                            }
                            self.remember(key, target, Some(Preview::from_bgra(image)));
                        }
                        Err(err) => {
                            log::debug!("playlist thumbnail generation: {err}");
                            self.message = Some(format!("Thumbnail generation failed: {err}"));
                        }
                        _ => {}
                    }
                    self.maintenance_pending = true;
                }
            }
            changed = true;
        }
        // Let playback finish opening the file before starting more I/O.
        if current.status == Status::Loading {
            self.refresh_activity();
            return (changed, std::mem::take(&mut self.retired));
        }
        // Explicit and automatic generation requests share this queue. Hover has its own worker.
        let preferred = if let (Some(MediaSource::File(path)), Some(TrackId::Stream(stream))) =
            (&current.source, current.selected.video)
        {
            self.manual
                .iter()
                .position(|(p, s, _)| p == path && *s == Some(stream))
                .or_else(|| self.manual.iter().position(|(p, s, _)| p == path && s.is_none()))
        } else {
            None
        };
        let index = preferred.unwrap_or(0);
        let kind = if let Some((path, stream, _)) = self.manual.get(index) {
            // The timeline worker may already be preparing this media. Other
            // queued jobs can proceed while that independent worker finishes.
            let index = if timeline.busy_for(path, *stream) {
                self.manual.iter().position(|(p, s, _)| !timeline.busy_for(p, *s))
            } else {
                Some(index)
            };
            index
                .and_then(|i| self.manual.remove(i))
                .map(|(path, stream, single)| JobKind::Generate(path, stream, single))
        } else {
            None
        };
        if let Some(kind) = kind {
            let work = kind.clone();
            let JobKind::Generate(path, stream, single) = &kind;
            let missing_only = self
                .missing_only
                .iter()
                .position(|(p, st, one)| p == path && st == stream && one == single)
                .map(|i| self.missing_only.remove(i))
                .is_some();
            let auto = if *single && missing_only {
                stream.and_then(|stream| self.auto_rows.remove(&(path.clone(), stream)))
            } else {
                None
            };
            let pack_slot = self.pack.clone();
            let cfg = settings.engine_config();
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = cancel.clone();
            let progress = Some(Arc::new(GenerationProgress::new(1)));
            let report = progress.clone();
            let (tx, result) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("playlist-thumbnails".into()).spawn(move || {
                super::background_priority::lower();
                let result = (|| -> JobResult {
                    anyhow::ensure!(!stop.load(Ordering::Relaxed), "thumbnail generation cancelled");
                    match work {
                        JobKind::Generate(path, selected_stream, single) => {
                            if let Some((id, row)) = auto {
                                let selection = thumbnail_cache::Selection {
                                    path: row.key.path.clone(),
                                    stream: row.key.stream,
                                    count: row.key.count,
                                    target: row.target,
                                };
                                let slot = pack_slot.lock();
                                if slot.as_ref().is_some_and(|(dir, old, _)| dir == &row.key.directory && *old == id) {
                                    if slot.as_ref().is_some_and(|(_, _, pack)| pack.contains(row.item, &selection)) {
                                        return Ok(JobOutput::Skipped);
                                    }
                                } else if let Ok(pack) = thumbnail_cache::Pack::open(&row.key.directory, id) {
                                    if pack.contains(row.item, &selection) {
                                        return Ok(JobOutput::Skipped);
                                    }
                                }
                            }
                            let probe = crate::engine::probe::probe(&MediaSource::File(path.clone()), &cfg)
                                .ok_or_else(|| anyhow::anyhow!("could not probe media"))?;
                            let Some(stream) = selected_stream.or(probe.video_stream) else {
                                if missing_only {
                                    return Ok(JobOutput::Skipped);
                                }
                                return Err(anyhow::anyhow!("no video stream"));
                            };
                            let key = Key {
                                path,
                                stream,
                                count: crate::engine::thumbnails::sample_count(probe.duration),
                                directory,
                            };
                            let target = probe.duration * crate::engine::thumbnails::representative_fraction(&key.path);
                            if missing_only
                                && let Ok(mut images) = load_cached(&key.path, key.spec(), &key.directory, Some(target))
                                && let Some(image) = images.pop()
                            {
                                return Ok(JobOutput::Existing(key, target, to_bgra(image), probe.duration));
                            }
                            let progress = report.as_ref().expect("manual generation progress");
                            if single {
                                let spec = ThumbnailSpec { count: 1, ..key.spec() };
                                if missing_only
                                    && let Ok(mut images) = load_cached(&key.path, spec, &key.directory, Some(target))
                                    && let Some(image) = images.pop()
                                {
                                    return Ok(JobOutput::Existing(key, target, to_bgra(image), probe.duration));
                                }
                                let mut images = load_or_generate(
                                    &key.path,
                                    &cfg,
                                    spec,
                                    &key.directory,
                                    !missing_only,
                                    GenerationControl { stop: &stop, generating: &AtomicBool::new(false), progress },
                                )?;
                                let image =
                                    images.pop().ok_or_else(|| anyhow::anyhow!("no representative thumbnail"))?;
                                return Ok(JobOutput::Representative(key, target, to_bgra(image), probe.duration));
                            }
                            progress.set_total(key.count);
                            let images = load_or_generate(
                                &key.path,
                                &cfg,
                                key.spec(),
                                &key.directory,
                                true,
                                GenerationControl { stop: &stop, generating: &AtomicBool::new(false), progress },
                            )?;
                            Ok(JobOutput::Media(key, images.into_iter().map(to_bgra).collect(), Some(probe.duration)))
                        }
                    }
                })();
                let _ = tx.send(result);
            });
            match spawned {
                Ok(_) => {
                    self.worker = Some(Job { kind, cancel, progress, result });
                    changed = true;
                }
                Err(err) => {
                    self.message = Some(format!("Could not start thumbnail generation: {err}"));
                    changed = true;
                }
            }
        }
        self.refresh_activity();
        (changed, std::mem::take(&mut self.retired))
    }
    pub fn take_message(&mut self) -> Option<String> {
        self.message.take()
    }
    pub fn take_completed(&mut self) -> Option<(Key, f64)> {
        self.completed.take()
    }
    pub fn take_pack_refresh(&mut self) -> Option<(Key, f64, bool)> {
        self.pack_refresh.take()
    }
    pub fn clear(&mut self) {
        self.pack_refresh = None;
        self.worker = None;
        self.cancel_hover();
        self.inline_worker = None;
        self.manual.clear();
        self.auto_rows.clear();
        self.missing_only.clear();
        self.batch_total = 0;
        self.pending.clear();
        self.visible_rows.clear();
        self.prefetch_rows.clear();
        self.playlist = None;
        self.background_rows.clear();
        self.selection_writes.clear();
        self.invalidated_pack_paths.clear();
        self.pack = Arc::default();
        self.hover = None;
        self.drop_full();
        let recent = std::mem::take(&mut self.recent);
        for (_, images) in recent {
            for p in images {
                self.retire_if_unused(p.image.id);
            }
        }
        let removed: Vec<_> = self.entries.drain(..).filter_map(|(_, _, p)| p.map(|p| p.image.id)).collect();
        for id in removed {
            self.retire_if_unused(id);
        }
        self.refresh_activity();
    }
    pub fn take_maintenance(&mut self) -> bool {
        std::mem::take(&mut self.maintenance_pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_selection_persists_preview_without_media_cache() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("mav-selection-{}", uuid::Uuid::new_v4()));
        let result = (|| -> anyhow::Result<()> {
            let playlist = PlaylistId::new();
            let row = RepresentativeRow {
                key: Key { path: root.join("offline.mp4"), stream: 0, count: 20, directory: root.clone() },
                target: 35.,
                item: PlaylistItemId::new(),
            };
            let mut pack = thumbnail_cache::Pack::open(&root, playlist)?;
            let sample =
                Thumbnail { time: 35., image: image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 0, 255, 255])) };
            persist_selection(&mut pack, &row, Some(&sample))?;
            drop(pack);
            let mut pack = thumbnail_cache::Pack::open(&root, playlist)?;
            let selection =
                thumbnail_cache::Selection { path: row.key.path.clone(), stream: 0, count: 20, target: 35. };
            let (time, jpeg) = pack.read(row.item, &selection)?.expect("selected frame");
            assert_eq!(time, 35.);
            let decoded = decode_representative(jpeg, row.key.spec(), time)?;
            assert!(decoded.image.get_pixel(0, 0).0[0] > 240);
            assert!(decoded.image.get_pixel(0, 0).0[2] < 15);
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(root);
        result
    }

    #[test]
    fn explicit_selection_queue_coalesces_and_survives_playlist_switch() {
        let mut previews = PlaylistPreviews::default();
        let playlist = PlaylistId::new();
        let mut row = RepresentativeRow {
            key: Key { path: "movie.mp4".into(), stream: 0, count: 20, directory: "cache".into() },
            target: 5.,
            item: PlaylistItemId::new(),
        };
        previews.retain_visible(Some(playlist), vec![row.clone()], Vec::new());
        previews.set_pack_rows(vec![row.clone()]);
        previews.queue_selection_write(playlist, vec![row.clone()], None);
        row.target = 35.;
        previews.queue_selection_write(playlist, vec![row.clone()], None);
        assert_eq!(previews.selection_writes.len(), 1);
        assert_eq!(previews.visible_rows[0].target, 35.);
        assert_eq!(previews.background_rows[0].target, 35.);
        previews.retain_visible(Some(PlaylistId::new()), Vec::new(), Vec::new());
        assert_eq!(previews.selection_writes[0].0, playlist);
        assert_eq!(previews.selection_writes[0].1.target, 35.);
    }

    #[test]
    fn viewport_change_prunes_old_requests_and_switch_cancels_worker() {
        let mut previews = PlaylistPreviews::default();
        let playlist = PlaylistId::new();
        let row = |path: &str| RepresentativeRow {
            key: Key { path: path.into(), stream: 0, count: 20, directory: "cache".into() },
            target: 5.,
            item: PlaylistItemId::new(),
        };
        let a = row("a.mp4");
        let b = row("b.mp4");
        previews.retain_visible(Some(playlist), vec![a.clone(), b.clone()], Vec::new());
        assert!(previews.inline(&a.key, a.target).is_none());
        assert!(previews.inline(&b.key, b.target).is_none());
        previews.retain_visible(Some(playlist), vec![b.clone()], vec![a.clone()]);
        assert_eq!(previews.pending.len(), 1);
        assert!(previews.pending[0].0 == b.key);
        let cancel = Arc::new(AtomicBool::new(false));
        let (_tx, result) = std::sync::mpsc::channel();
        previews.inline_worker =
            Some(InlineJob { requests: vec![(b.key.clone(), b.target)], cancel: cancel.clone(), result });
        previews.retain_visible(Some(PlaylistId::new()), vec![a], Vec::new());
        assert!(cancel.load(Ordering::Relaxed));
        assert!(previews.pending.is_empty());
        assert!(previews.inline_worker.is_some());
    }

    #[test]
    fn playlist_thumbnail_mode_is_independent_of_timeline() {
        let mut settings = Settings::default();
        settings.timeline_thumbnails = false;
        settings.playlist_thumbnails = PlaylistThumbnails::StaticInline;
        assert_eq!(PlaylistPreviews::mode(&settings), PlaylistThumbnails::StaticInline);
        assert!(settings.playlist_thumbnail_auto_video);
        assert!(!settings.filmstrip_auto_video);
        assert!(!settings.filmstrip_auto_playback);
    }

    #[test]
    fn playlist_representative_pair_survives_inline_mode_switch() {
        let mut settings = Settings::default();
        settings.playlist_thumbnails = PlaylistThumbnails::Inline;
        settings.timeline_thumbnails = true;
        let key = Key { path: "movie.mp4".into(), stream: 0, count: 20, directory: settings.thumbnail_cache_dir() };
        let make = |time| Preview::from_bgra(Thumbnail { time, image: image::RgbaImage::new(2, 2) });
        let default = make(5.0);
        let chosen = make(35.0);
        let mut previews = PlaylistPreviews {
            directory: Some(key.directory.clone()),
            mode: Some(PlaylistThumbnails::Inline),
            ..Default::default()
        };
        previews.full = Some((key.clone(), vec![default.clone(), chosen.clone()]));
        assert!(Arc::ptr_eq(&previews.inline_pair(&key, 35., 5.).unwrap().image, &chosen.image));
        previews.drop_full();
        settings.playlist_thumbnails = PlaylistThumbnails::InlineSide;
        let (_, retired) =
            previews.update(&settings, true, None, &TimelineThumbnails::default(), &PlaybackState::default());
        assert!(retired.is_empty());
        assert!(Arc::ptr_eq(&previews.inline_pair(&key, 5., 5.).unwrap().image, &default.image));
        assert!(Arc::ptr_eq(&previews.inline_pair(&key, 35., 5.).unwrap().image, &chosen.image));
        assert!(previews.pending.is_empty());
    }

    #[test]
    fn panel_representative_lookup_prefers_selected_and_falls_back_to_default() {
        let mut previews = PlaylistPreviews::default();
        let key = Key { path: "movie.mp4".into(), stream: 0, count: 20, directory: "cache".into() };
        let make = |time| Preview::from_bgra(Thumbnail { time, image: image::RgbaImage::new(2, 2) });
        assert!(previews.cached_representative(&key, 35., 5.).is_none());
        previews.remember_selection(key.clone(), 5., make(5.));
        assert_eq!(previews.cached_representative(&key, 35., 5.).unwrap().time, 5.);
        previews.remember_selection(key.clone(), 35., make(35.));
        assert_eq!(previews.cached_representative(&key, 35., 5.).unwrap().time, 35.);
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
        let request = (path.clone(), Some(0), false);
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
    fn filmstrip_request_supersedes_pending_single_thumbnail() {
        let mut previews = PlaylistPreviews::default();
        let path = PathBuf::from("movie.mp4");
        previews.generate_representative(path.clone(), Some(0));
        previews.generate_missing(path.clone(), Some(0));
        previews.generate_representative(path.clone(), Some(0));
        assert_eq!(previews.manual, VecDeque::from([(path, Some(0), false)]));
    }

    #[test]
    fn removing_media_prunes_queue_and_cancels_active_generation() {
        let mut previews = PlaylistPreviews::default();
        let a = PathBuf::from("a.mp4");
        let b = PathBuf::from("b.mp4");
        let c = PathBuf::from("c.mp4");
        previews.generate_missing(a.clone(), Some(0));
        let (path, stream, single) = previews.manual.pop_front().unwrap();
        previews.missing_only.clear();
        let cancel = Arc::new(AtomicBool::new(false));
        let (_tx, result) = std::sync::mpsc::channel();
        previews.worker = Some(Job {
            kind: JobKind::Generate(path, stream, single),
            cancel: cancel.clone(),
            progress: Some(Arc::new(GenerationProgress::new(20))),
            result,
        });
        previews.generate_missing(b.clone(), Some(0));
        previews.generate_missing(c.clone(), Some(0));
        assert!(previews.retain_generation(|p| p == c.as_path()));
        assert!(cancel.load(Ordering::Relaxed));
        assert!(previews.worker.is_some()); // Wait for acknowledgement before starting another job.
        assert_eq!(previews.manual, VecDeque::from([(c.clone(), Some(0), false)]));
        assert_eq!(previews.missing_only, vec![(c, Some(0), false)]);
        assert!(previews.progress().is_none());
        previews.generate_missing(a, Some(0)); // Re-adding a cancelled file is allowed.
        assert_eq!(previews.manual.len(), 2);
    }

    #[test]
    fn queued_thumbnail_requests_do_not_cancel_current_generation() {
        let mut previews = PlaylistPreviews::default();
        let a = PathBuf::from("first.mp4");
        let b = PathBuf::from("second.mp4");
        let c = PathBuf::from("third.mp4");
        previews.regenerate(a.clone(), Some(0));
        let (path, stream, single) = previews.manual.pop_front().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (_tx, result) = std::sync::mpsc::channel();
        previews.worker = Some(Job {
            kind: JobKind::Generate(path, stream, single),
            cancel: cancel.clone(),
            progress: Some(Arc::new(GenerationProgress::new(20))),
            result,
        });
        previews.regenerate(b.clone(), Some(0));
        previews.regenerate(c.clone(), Some(0));
        previews.regenerate(a, Some(0));
        previews.regenerate(b.clone(), Some(0));
        assert!(!cancel.load(Ordering::Relaxed));
        assert_eq!(previews.batch_total, 3);
        assert_eq!(previews.manual.pop_front(), Some((b, Some(0), false)));
        assert_eq!(previews.manual.pop_front(), Some((c, Some(0), false)));
    }
}
