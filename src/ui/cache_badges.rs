//! Cache availability for visible playlist rows, independent of preview decoding.
use super::thumbnails::Preview;
use crate::engine::{
    MediaSource,
    thumbnails::{ThumbnailSpec, cache, sample_count},
    waveform,
};
use crate::playlist::{
    PlaylistItem,
    collections::{
        PlaylistId, PlaylistItemId,
        thumbnail_cache::{Pack, Selection},
    },
};
use gpui::{Bounds, Hsla, ImageId, Pixels};
type SharedPack = Arc<parking_lot::Mutex<Option<(PathBuf, PlaylistId, Pack)>>>;
const W: usize = 288;
const H: usize = 176;

fn raster_bits(peaks: &waveform::Peaks) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(!peaks.is_empty(), "empty waveform");
    let mut bits = vec![0u8; W / 8 * H];
    for x in 0..W {
        let from = x * peaks.len() / W;
        let to = ((x + 1) * peaks.len() / W).max(from + 1).min(peaks.len());
        let slice = &peaks[from..to];
        let lo = slice.iter().map(|p| p[0]).min().unwrap_or(0) as f32 / 32768.0;
        let hi = slice.iter().map(|p| p[1]).max().unwrap_or(0) as f32 / 32768.0;
        let top = (H as f32 * (1.0 - hi) * 0.5).clamp(0.0, H as f32 - 1.0) as usize;
        let bottom = ((H as f32 * (1.0 - lo) * 0.5).ceil() as usize).max(top + 1).min(H);
        for y in top..bottom {
            bits[y * (W / 8) + x / 8] |= 0x80 >> (x % 8);
        }
    }
    Ok(bits)
}

fn encode_mask(peaks: &waveform::Peaks) -> anyhow::Result<Vec<u8>> {
    let bits = raster_bits(peaks)?;
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, W as u32, H as u32);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::One);
        encoder.write_header()?.write_image_data(&bits)?;
    }
    Ok(output)
}

fn decode_mask(bytes: &[u8]) -> anyhow::Result<image::GrayImage> {
    // Check dimensions before allocating the decoded image.
    anyhow::ensure!(
        bytes.len() >= 26
            && &bytes[..8] == b"\x89PNG\r\n\x1a\n"
            && u32::from_be_bytes(bytes[16..20].try_into().unwrap()) == W as u32
            && u32::from_be_bytes(bytes[20..24].try_into().unwrap()) == H as u32
            && bytes[24] == 1
            && bytes[25] == 0,
        "invalid waveform mask"
    );
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info()?;
    let mut bits = vec![0; W / 8 * H];
    let info = reader.next_frame(&mut bits)?;
    anyhow::ensure!(
        info.buffer_size() == bits.len() && info.bit_depth == png::BitDepth::One,
        "unexpected waveform mask layout"
    );
    Ok(expand_bits(&bits))
}

fn expand_bits(bits: &[u8]) -> image::GrayImage {
    image::GrayImage::from_fn(W as u32, H as u32, |x, y| {
        let x = x as usize;
        let y = y as usize;
        image::Luma([if bits[y * (W / 8) + x / 8] & (0x80 >> (x % 8)) != 0 { 255 } else { 0 }])
    })
}

fn load_audio(
    entry: &Entry,
    playlist: Option<PlaylistId>,
    shared: &SharedPack,
    refresh: bool,
) -> Option<(Arc<waveform::Peaks>, image::GrayImage, bool)> {
    let stream = entry.audio?;
    let selection = Selection { path: entry.path.clone(), stream, count: 0, target: 0.0 };
    let mut slot = shared.lock();
    if let Some(id) = playlist {
        if !slot.as_ref().is_some_and(|(dir, old, _)| dir == &entry.directory && *old == id) {
            *slot = Pack::open(&entry.directory, id).ok().map(|p| (entry.directory.clone(), id, p));
        }
    }
    let mut pack =
        slot.as_mut().filter(|(dir, id, _)| Some(*id) == playlist && dir == &entry.directory).map(|(_, _, p)| p);
    let cached = pack.as_deref_mut().and_then(|p| match p.read_waveform(entry.persistent_id, &selection) {
        Ok(value) => value,
        Err(err) => {
            log::debug!("playlist waveform read {:?}: {err}", entry.path);
            None
        }
    });
    if !refresh {
        if let Some((png, peaks)) = &cached {
            match decode_mask(png) {
                Ok(mask) => return Some((Arc::new(peaks.clone()), mask, true)),
                Err(err) => log::debug!("playlist waveform PNG decode {:?}: {err}", entry.path),
            }
        }
    }
    let peaks = match waveform::load_cached(&entry.path, stream, &entry.directory) {
        Ok(peaks) => peaks,
        Err(err) => {
            log::debug!("media waveform cache {:?}: {err}", entry.path);
            let (_, peaks) = cached.as_ref()?;
            peaks.clone()
        }
    };
    // A refresh checks the source amplitudes, but unchanged peaks can reuse
    // the existing PNG. This also preserves the packed mask when offline.
    if let Some((png, previous)) = &cached {
        if previous == &peaks {
            if let Ok(mask) = decode_mask(png) {
                return Some((Arc::new(peaks), mask, true));
            }
        }
    }
    // Rendering must not depend on PNG encoding or decoding succeeding.
    let bits = raster_bits(&peaks).ok()?;
    let mask = expand_bits(&bits);
    match encode_mask(&peaks) {
        Ok(png) => {
            if let Some(p) = pack {
                if cached.as_ref().is_none_or(|(old_png, old)| old != &peaks || decode_mask(old_png).is_err()) {
                    if let Err(err) = p.append_waveform(entry.persistent_id, selection, &png, &peaks) {
                        log::debug!("playlist waveform append {:?}: {err}", entry.path);
                    }
                }
            }
        }
        Err(err) => log::debug!("playlist waveform PNG encode {:?}: {err}", entry.path),
    }
    Some((Arc::new(peaks), mask, false))
}

// Populate only missing records. Do not decode PNGs or retain images for
// offscreen items, and release the shared file before any media-cache I/O.
fn populate_audio(entry: &Entry, playlist: PlaylistId, shared: &SharedPack) {
    let Some(stream) = entry.audio else {
        return;
    };
    let selection = Selection { path: entry.path.clone(), stream, count: 0, target: 0.0 };
    {
        let mut slot = shared.lock();
        if !slot.as_ref().is_some_and(|(dir, id, _)| dir == &entry.directory && *id == playlist) {
            *slot = Pack::open(&entry.directory, playlist).ok().map(|p| (entry.directory.clone(), playlist, p));
        }
        let Some((_, _, pack)) = slot.as_ref() else {
            return;
        };
        if pack.contains(entry.persistent_id, &selection) {
            return;
        }
    }
    let Ok(peaks) = waveform::load_cached(&entry.path, stream, &entry.directory) else {
        return;
    };
    let Ok(png) = encode_mask(&peaks) else {
        return;
    };
    let mut slot = shared.lock();
    // The active pack may have changed while reading the source cache.
    if let Some((_, _, pack)) = slot.as_mut().filter(|(dir, id, _)| dir == &entry.directory && *id == playlist) {
        if !pack.contains(entry.persistent_id, &selection) {
            if let Err(err) = pack.append_waveform(entry.persistent_id, selection, &png, &peaks) {
                log::debug!("background playlist waveform append {:?}: {err}", entry.path);
            }
        }
    }
}

const BACKGROUND_BATCH: usize = 2;

use crate::settings::Settings;
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Entry {
    id: u64,
    persistent_id: PlaylistItemId,
    path: PathBuf,
    video: Option<ThumbnailSpec>,
    audio: Option<usize>,
    audio_only: bool,
    directory: PathBuf,
}
impl Entry {
    pub(super) fn id(&self) -> u64 {
        self.id
    }

    pub(super) fn new(item: &PlaylistItem, settings: &Settings) -> Option<Self> {
        let MediaSource::File(path) = &item.source else {
            return None;
        };
        let video = item
            .video_stream
            .zip(item.duration.filter(|d| d.is_finite() && *d > 0.0))
            .map(|(stream, duration)| ThumbnailSpec { stream, count: sample_count(duration), width: 240, height: 135 });
        if video.is_none() && item.audio_stream.is_none() {
            return None;
        }
        Some(Self {
            id: item.id,
            persistent_id: item.persistent_id,
            path: path.clone(),
            video,
            audio: item.audio_stream,
            audio_only: item.video_stream.is_none(),
            directory: settings.thumbnail_cache_dir(),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Availability {
    pub video: bool,
    pub audio: bool,
}
type ResultEntry = (Entry, Availability, Option<(Arc<waveform::Peaks>, image::GrayImage, bool)>);
#[derive(Clone)]
pub(super) struct AudioHover {
    pub entry: Entry,
    pub duration: f64,
    pub fraction: f32,
    pub bounds: Bounds<Pixels>,
}

#[derive(Default)]
pub(super) struct CacheBadges {
    pub audio_hover: Option<AudioHover>,
    visible: Vec<Entry>,
    background_scope: Option<(PlaylistId, usize, PathBuf)>,
    background: VecDeque<Entry>,
    values: HashMap<u64, (Entry, Availability)>,
    waveforms: HashMap<u64, (Entry, Arc<waveform::Peaks>, image::GrayImage, bool, Option<(Hsla, Preview)>)>,
    retired: Vec<ImageId>,
    refresh: bool,
    worker: Option<Receiver<ResultEntry>>,
    wake: Option<futures::channel::mpsc::UnboundedSender<()>>,
    last: Option<Instant>,
    dirty: bool,
}
impl CacheBadges {
    pub(super) fn with_wake(wake: futures::channel::mpsc::UnboundedSender<()>) -> Self {
        Self { wake: Some(wake), ..Self::default() }
    }
    pub(super) fn retain_visible(&mut self, entries: Vec<Entry>) {
        if entries != self.visible {
            self.visible = entries;
            // A virtual list can request a temporary measurement range.
            // Pointer geometry, not that range, owns the audio hover lifetime.
            // Entries are keyed by stable playlist item IDs, not row indices.
            // Retain cached statuses across rearrangement/scrolling.
            self.dirty = true;
            if let Some(wake) = &self.wake {
                let _ = wake.unbounded_send(());
            }
            // Bound memory while preserving statuses across ordinary reordering.
            if self.values.len() > 512 {
                self.values.retain(|_, (entry, _)| self.visible.contains(entry));
            }
            if self.waveforms.len() > 192 {
                let visible = &self.visible;
                let retired = &mut self.retired;
                self.waveforms.retain(|_, (entry, _, _, _, preview)| {
                    let keep = visible.contains(entry);
                    if !keep {
                        if let Some((_, preview)) = preview {
                            retired.push(preview.image.id);
                        }
                    }
                    keep
                });
            }
        }
    }
    pub(super) fn needs_background(&mut self, playlist: Option<PlaylistId>, count: usize, directory: PathBuf) -> bool {
        let scope = playlist.map(|id| (id, count, directory));
        if self.background_scope == scope {
            return false;
        }
        self.background_scope = scope;
        self.background.clear();
        self.background_scope.is_some()
    }
    pub(super) fn set_background(&mut self, entries: Vec<Entry>) {
        self.background = entries.into_iter().filter(|entry| entry.audio_only).collect();
        if let Some(wake) = &self.wake {
            let _ = wake.unbounded_send(());
        }
    }
    pub(super) fn queue_background_entry(&mut self, playlist: Option<PlaylistId>, entry: Entry) {
        if entry.audio_only
            && self
                .background_scope
                .as_ref()
                .is_some_and(|(id, _, dir)| Some(*id) == playlist && dir == &entry.directory)
            && !self.background.contains(&entry)
        {
            self.background.push_back(entry);
            if let Some(wake) = &self.wake {
                let _ = wake.unbounded_send(());
            }
        }
    }
    pub(super) fn get(&self, entry: &Entry) -> Availability {
        self.values
            .get(&entry.id)
            .filter(|(key, _)| key == entry)
            .map_or_else(Availability::default, |(_, value)| *value)
    }
    pub(super) fn audio_peaks(&self, entry: &Entry) -> Option<Arc<waveform::Peaks>> {
        let (key, peaks, _, _, _) = self.waveforms.get(&entry.id)?;
        (key == entry && !peaks.is_empty()).then(|| peaks.clone())
    }

    pub(super) fn waveform(&mut self, entry: &Entry, color: Hsla) -> Option<Preview> {
        let (key, _, mask, _, preview) = self.waveforms.get_mut(&entry.id)?;
        if key != entry {
            return None;
        }
        if preview.as_ref().is_none_or(|(old, _)| *old != color) {
            if let Some((_, old)) = preview.take() {
                self.retired.push(old.image.id);
            }
            let rgb = gpui::Rgba::from(color);
            let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            let raster = image::RgbaImage::from_fn(W as u32, H as u32, |x, y| {
                image::Rgba([
                    byte(rgb.b),
                    byte(rgb.g),
                    byte(rgb.r),
                    byte(rgb.a * mask.get_pixel(x, y)[0] as f32 / 255.0),
                ])
            });
            *preview =
                Some((color, Preview::from_bgra(crate::engine::thumbnails::Thumbnail { time: 0.0, image: raster })));
        }
        preview.as_ref().map(|(_, p)| p.clone())
    }
    pub(super) fn take_retired_images(&mut self) -> Vec<ImageId> {
        std::mem::take(&mut self.retired)
    }
    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
        self.refresh = true;
        self.background_scope = None;
        self.background.clear();
    }
    pub(super) fn update(&mut self, shown: bool, playlist: Option<PlaylistId>, pack: SharedPack) -> bool {
        if !shown {
            self.retain_visible(Vec::new());
        }
        let mut changed = false;
        if let Some(worker) = self.worker.take() {
            loop {
                match worker.try_recv() {
                    Ok((entry, status, peaks)) => {
                        if self.visible.contains(&entry) {
                            changed |= self.get(&entry) != status;
                            if let Some((peaks, mask, from_png)) = peaks {
                                if !self.waveforms.get(&entry.id).is_some_and(
                                    |(old, previous, _, previous_source, _)| {
                                        old == &entry && previous == &peaks && *previous_source == from_png
                                    },
                                ) {
                                    if let Some((_, _, _, _, Some((_, old)))) = self.waveforms.remove(&entry.id) {
                                        self.retired.push(old.image.id);
                                    }
                                    self.waveforms.insert(entry.id, (entry.clone(), peaks, mask, from_png, None));
                                    changed = true;
                                }
                            } else if let Some((_, _, _, _, preview)) = self.waveforms.remove(&entry.id) {
                                if let Some((_, old)) = preview {
                                    self.retired.push(old.image.id);
                                }
                                changed = true;
                            }
                            self.values.insert(entry.id, (entry, status));
                        }
                    }
                    Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {
                        self.worker = Some(worker);
                        return changed;
                    }
                }
            }
        }
        // Work done in this window marks the badges dirty. The periodic check
        // only catches other windows and changed files, so it can be slow.
        if shown
            && !self.visible.is_empty()
            && (self.dirty || self.last.is_none_or(|last| last.elapsed() >= Duration::from_secs(30)))
        {
            let refresh = self.refresh || self.last.is_some_and(|last| last.elapsed() >= Duration::from_secs(30));
            let mut entries: Vec<_> = self
                .visible
                .iter()
                .filter(|entry| {
                    refresh
                        || !self.values.get(&entry.id).is_some_and(|(key, _)| key == *entry)
                        || (entry.audio_only && self.get(entry).audio && !self.waveforms.contains_key(&entry.id))
                })
                .cloned()
                .collect();
            entries.sort_by_key(|entry| !entry.audio_only);
            if entries.is_empty() {
                self.dirty = false;
                self.refresh = false;
                self.last = Some(Instant::now());
            } else {
                let wake = self.wake.clone();
                let visible_pack = pack.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                if std::thread::Builder::new()
                    .name("playlist-cache-status".into())
                    .spawn(move || {
                        for entry in entries {
                            let video =
                                entry.video.is_some_and(|spec| cache::cached(&entry.path, spec, &entry.directory));
                            let peaks = if entry.audio_only {
                                load_audio(&entry, playlist, &visible_pack, refresh)
                            } else {
                                None
                            };
                            let audio = peaks.is_some()
                                || (!entry.audio_only
                                    && entry.audio.is_some_and(|stream| {
                                        waveform::load_cached(&entry.path, stream, &entry.directory).is_ok()
                                    }));
                            if tx.send((entry, Availability { video, audio }, peaks)).is_err() {
                                break;
                            }
                            if let Some(wake) = &wake {
                                let _ = wake.unbounded_send(());
                            }
                        }
                        drop(tx);
                        if let Some(wake) = &wake {
                            let _ = wake.unbounded_send(());
                        }
                    })
                    .is_ok()
                {
                    self.worker = Some(rx);
                    self.dirty = false;
                    self.refresh = false;
                    self.last = Some(Instant::now());
                }
            }
        }
        // Use the same worker slot: a new visible request wins at the next
        // batch boundary. Offscreen population never produces UI textures.
        if shown
            && self.worker.is_none()
            && !self.background.is_empty()
            && let Some(id) =
                playlist.filter(|id| self.background_scope.as_ref().is_some_and(|(scope, _, _)| scope == id))
        {
            let count = self.background.len().min(BACKGROUND_BATCH);
            let entries: Vec<_> = self.background.iter().take(count).cloned().collect();
            let wake = self.wake.clone();
            let (tx, rx) = std::sync::mpsc::channel::<ResultEntry>();
            if std::thread::Builder::new()
                .name("playlist-waveform-populate".into())
                .spawn(move || {
                    super::background_priority::lower();
                    for entry in entries {
                        populate_audio(&entry, id, &pack);
                    }
                    drop(tx);
                    if let Some(wake) = &wake {
                        let _ = wake.unbounded_send(());
                    }
                })
                .is_ok()
            {
                self.background.drain(..count);
                self.worker = Some(rx);
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_scope_restarts_only_when_needed() {
        let mut badges = CacheBadges::default();
        let id = PlaylistId::new();
        let dir = PathBuf::from("cache");
        assert!(badges.needs_background(Some(id), 20, dir.clone()));
        assert!(!badges.needs_background(Some(id), 20, dir.clone()));
        assert!(badges.needs_background(Some(id), 21, dir.clone()));
        badges.invalidate();
        assert!(badges.needs_background(Some(id), 21, dir.clone()));
        assert!(!badges.needs_background(None, 21, dir));
    }

    #[test]
    fn waveform_mask_is_one_bit_and_preserves_silence() -> anyhow::Result<()> {
        let png = encode_mask(&vec![[0, 0]; 4096])?;
        assert_eq!(png[24], 1);
        assert_eq!(png[25], 0);
        let mask = decode_mask(&png)?;
        assert_eq!(mask.dimensions(), (288, 176));
        assert_eq!(mask.get_pixel(0, 88)[0], 255);
        assert_eq!(mask.get_pixel(0, 0)[0], 0);
        Ok(())
    }
}
