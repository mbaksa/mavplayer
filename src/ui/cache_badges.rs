//! Cache availability for visible playlist rows, independent of preview decoding.
use crate::engine::{MediaSource, thumbnails::{ThumbnailSpec, sample_count, cache}, waveform};
use crate::playlist::PlaylistItem;
use crate::settings::Settings;
use std::{collections::HashMap, path::PathBuf, sync::{Arc, mpsc::{Receiver, TryRecvError}}, time::{Duration, Instant}};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Entry {
    id: u64,
    path: PathBuf,
    video: Option<ThumbnailSpec>,
    audio: Option<usize>,
    directory: PathBuf,
}
impl Entry {
    pub(super) fn new(item: &PlaylistItem, settings: &Settings) -> Option<Self> {
        let MediaSource::File(path) = &item.source else { return None; };
        let video = item.video_stream.zip(item.duration.filter(|d| d.is_finite() && *d > 0.0)).map(|(stream, duration)| ThumbnailSpec {
            stream, count: sample_count(duration), width: 240, height: 135,
        });
        if video.is_none() && item.audio_stream.is_none() { return None; }
        Some(Self { id: item.id, path: path.clone(), video, audio: item.audio_stream, directory: settings.thumbnail_cache_dir() })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Availability { pub video: bool, pub audio: bool }
type Results = Vec<(Entry, Availability, Option<Arc<waveform::Peaks>>)>;
#[derive(Default)]
pub(super) struct CacheBadges {
    visible: Vec<Entry>,
    values: HashMap<u64, (Entry, Availability)>,
    waveforms: HashMap<u64, (Entry, Arc<waveform::Peaks>)>,
    worker: Option<Receiver<Results>>,
    last: Option<Instant>,
    dirty: bool,
}
impl CacheBadges {
    pub(super) fn retain_visible(&mut self, entries: Vec<Entry>) {
        if entries != self.visible {
            self.visible = entries;
            // Entries are keyed by stable playlist item IDs, not row indices.
            // Retain cached statuses across rearrangement/scrolling.
            self.dirty = true;
            // Bound memory while preserving statuses across ordinary reordering.
            if self.values.len() > 512 {
                self.values.retain(|_, (entry, _)| self.visible.contains(entry));
                self.waveforms.retain(|_, (entry, _)| self.visible.contains(entry));
            }
        }
    }
    pub(super) fn get(&self, entry: &Entry) -> Availability {
        self.values.get(&entry.id).filter(|(key, _)| key == entry).map_or_else(Availability::default, |(_, value)| *value)
    }
    pub(super) fn waveform(&self, entry: &Entry) -> Option<Arc<waveform::Peaks>> {
        self.waveforms.get(&entry.id).filter(|(key, _)| key == entry).map(|(_, peaks)| peaks.clone())
    }
    pub(super) fn invalidate(&mut self) { self.dirty = true; }
    pub(super) fn update(&mut self, shown: bool) -> bool {
        if !shown { self.retain_visible(Vec::new()); }
        let mut changed = false;
        if let Some(worker) = &self.worker {
            match worker.try_recv() {
                Ok(results) => {
                    for (entry, status, peaks) in results {
                        if self.visible.contains(&entry) {
                            changed |= self.get(&entry) != status;
                            if let Some(peaks) = peaks {
                                self.waveforms.insert(entry.id, (entry.clone(), peaks));
                            } else {
                                self.waveforms.remove(&entry.id);
                            }
                            self.values.insert(entry.id, (entry, status));
                        }
                    }
                    self.worker = None;
                }
                Err(TryRecvError::Disconnected) => self.worker = None,
                Err(TryRecvError::Empty) => return false,
            }
        }
        if shown && !self.visible.is_empty() && (self.dirty || self.last.is_none_or(|last| last.elapsed() >= Duration::from_secs(3))) {
            let entries = self.visible.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            if std::thread::Builder::new().name("playlist-cache-status".into()).spawn(move || {
                let results = entries.into_iter().map(|entry| {
                    let video = entry.video.is_some_and(|spec| cache::cached(&entry.path, spec, &entry.directory));
                    let peaks = entry.audio.and_then(|stream| waveform::load_cached(&entry.path, stream, &entry.directory).ok()).map(Arc::new);
                    let audio = peaks.is_some();
                    (entry, Availability { video, audio }, peaks)
                }).collect();
                let _ = tx.send(results);
            }).is_ok() {
                self.worker = Some(rx);
                self.dirty = false;
                self.last = Some(Instant::now());
            }
        }
        changed
    }
}
