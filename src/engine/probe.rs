//! Lightweight metadata probing for playlist entries.

use super::av::{self, ff};
use super::demux;
use super::types::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

mod cache;

#[derive(Clone, Debug, Default)]
pub struct ProbeResult {
    pub duration: f64,
    pub video_stream: Option<usize>,
    pub audio_stream: Option<usize>,
    pub has_subtitles: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// False for the initial cache-only result or an inaccessible source.
    pub source_validated: bool,
}

/// The selected streams are independent of playlist identity and user titles.
/// Store FFmpeg's default audio choice separately from language preferences.
#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    info: MediaInfo,
    video_stream: Option<usize>,
    default_audio_stream: Option<usize>,
    sidecars: Option<Sidecars>,
    video_formats: Vec<VideoFormat>,
    #[serde(default)]
    camera_sidecars: Option<Vec<super::camera_metadata::SidecarStamp>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Sidecars {
    directories: Vec<(PathBuf, SystemTime)>,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct VideoFormat {
    stream: usize,
    pixel_format: String,
    bit_depth: Option<i32>,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
    color_range: Option<String>,
}

impl Metadata {
    fn refresh_camera(&mut self, path: &Path) -> bool {
        let stamps = super::camera_metadata::candidates(path);
        if self.camera_sidecars.as_ref() == Some(&stamps) {
            return false;
        }
        self.info.camera = super::camera_metadata::read(&stamps);
        self.camera_sidecars = Some(stamps);
        true
    }

    /// Only the playlist subtitle indicator needs sidecars, and only when
    /// there is no embedded subtitle stream. Directory timestamps let us
    /// avoid repeating the directory scan for unchanged media.
    fn refresh_sidecars(&mut self, path: &Path) -> bool {
        if self.video_stream.is_none() || self.info.tracks.iter().any(|t| t.kind == TrackKind::Subtitle) {
            return false;
        }
        let Some(parent) = path.parent() else { return false };
        let mut directories = Vec::new();
        let mut stamp = |dir: &Path| {
            let Ok(meta) = std::fs::metadata(dir) else { return false };
            if !meta.is_dir() {
                return false;
            }
            let Ok(modified) = meta.modified() else { return false };
            directories.push((dir.to_owned(), modified));
            true
        };
        if !stamp(parent) {
            return false;
        }
        for name in ["Subs", "subs", "Subtitles", "subtitles"] {
            let dir = parent.join(name);
            if stamp(&dir) {
                stamp(&dir.join(path.file_stem().unwrap_or_default()));
            }
        }
        if self.sidecars.as_ref().is_some_and(|s| s.directories == directories) {
            return false;
        }
        self.sidecars = Some(Sidecars { directories, paths: super::subs::find_sidecar_subtitles(path) });
        true
    }

    fn result(self, cfg: &EngineConfig, source_validated: bool) -> ProbeResult {
        let preferred_audio = cfg.audio_langs.iter().find_map(|lang| {
            self.info.tracks_of(TrackKind::Audio).find_map(|track| {
                if track.language.as_deref().is_some_and(|l| language_matches(l, lang)) {
                    demux::stream_index(Some(track.id))
                } else {
                    None
                }
            })
        });
        let audio_stream = if self.info.seekable { preferred_audio.or(self.default_audio_stream) } else { None };
        let has_subtitles = self.info.tracks.iter().any(|t| t.kind == TrackKind::Subtitle)
            || self.sidecars.as_ref().is_some_and(|s| !s.paths.is_empty());
        ProbeResult {
            duration: self.info.duration,
            video_stream: self.video_stream,
            audio_stream,
            has_subtitles,
            title: self.info.title,
            artist: self.info.artist,
            source_validated,
        }
    }
}

/// Generation workers require an accessible source; never use an offline hit.
pub fn probe(source: &MediaSource, cfg: &EngineConfig) -> Option<ProbeResult> {
    probe_cached(source, cfg, false)
}

/// Read only the local cache, without touching the source file. This pass
/// lets a whole playlist become browsable even if a NAS is slow or offline.
pub fn cached_playlist(source: &MediaSource, cfg: &EngineConfig) -> Option<ProbeResult> {
    let MediaSource::File(path) = source else { return None };
    Some(cache::snapshot(path, &cache::directory())?.result(cfg, false))
}

/// Playlist browsing may use the last known metadata of an offline source.
pub fn probe_playlist(source: &MediaSource, cfg: &EngineConfig) -> Option<ProbeResult> {
    probe_cached(source, cfg, true)
}

fn probe_cached(source: &MediaSource, cfg: &EngineConfig, allow_offline: bool) -> Option<ProbeResult> {
    let MediaSource::File(path) = source else { return None };
    let directory = cache::directory();
    let (metadata, source_validated) =
        cache::load_or_probe(path, &directory, allow_offline, || read_metadata(source, cfg))?;
    Some(metadata.result(cfg, source_validated))
}

fn read_metadata(source: &MediaSource, cfg: &EngineConfig) -> Option<Metadata> {
    let ic = demux::open_input(source, cfg, None).ok()?;
    let mut info = demux::build_media_info(&ic, source);
    // FFmpeg can report missing chapter timestamps. Do not write non-finite
    // values that would deserialize as invalid JSON numbers on the next load.
    info.chapters.retain(|chapter| chapter.start.is_finite() && chapter.end.is_finite());
    let mut defaults = cfg.clone();
    defaults.audio_langs.clear();
    let selected = demux::choose_tracks(&ic, &info, &defaults);
    let video_stream = match selected.video {
        Some(TrackId::Stream(i))
            if info.tracks.iter().any(|t| t.id == TrackId::Stream(i) && !t.is_cover_art) && info.seekable =>
        {
            Some(i)
        }
        _ => None,
    };
    let default_audio_stream = match selected.audio {
        Some(TrackId::Stream(i)) if info.seekable => Some(i),
        _ => None,
    };
    // Codec parameters are already available after probing; no extra decoding.
    let video_formats = info
        .tracks_of(TrackKind::Video)
        .filter_map(|track| {
            let TrackId::Stream(stream) = track.id else { return None };
            unsafe {
                let par = &*(*ic.stream(stream)).codecpar;
                let bit_depth = if par.bits_per_raw_sample > 0 {
                    Some(par.bits_per_raw_sample)
                } else if par.format >= 0 {
                    let desc = ff::av_pix_fmt_desc_get(std::mem::transmute::<i32, ff::AVPixelFormat>(par.format));
                    if desc.is_null() || (*desc).nb_components == 0 { None } else { Some((*desc).comp[0].depth) }
                } else {
                    None
                };
                Some(VideoFormat {
                    stream,
                    pixel_format: av::pix_fmt_name(par.format),
                    bit_depth,
                    color_primaries: av::from_cstr(ff::av_color_primaries_name(par.color_primaries)),
                    color_transfer: av::from_cstr(ff::av_color_transfer_name(par.color_trc)),
                    color_space: av::from_cstr(ff::av_color_space_name(par.color_space)),
                    color_range: av::from_cstr(ff::av_color_range_name(par.color_range)),
                })
            }
        })
        .collect();
    Some(Metadata { info, video_stream, default_audio_stream, sidecars: None, video_formats, camera_sidecars: None })
}
