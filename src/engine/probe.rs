//! Lightweight metadata probing for playlist entries.

use super::demux;
use super::types::*;

#[derive(Clone, Debug, Default)]
pub struct ProbeResult {
    pub duration: f64,
    pub video_stream: Option<usize>,
    pub title: Option<String>,
    pub artist: Option<String>,
}

pub fn probe(source: &MediaSource, cfg: &EngineConfig) -> Option<ProbeResult> {
    if !matches!(source, MediaSource::File(_)) {
        return None;
    }
    let ic = demux::open_input(source, cfg, None).ok()?;
    let info = demux::build_media_info(&ic, source);
    let video_stream = match demux::choose_tracks(&ic, &info, cfg).video {
        Some(TrackId::Stream(i)) if info.tracks.iter().any(|t| t.id == TrackId::Stream(i) && !t.is_cover_art) && info.seekable => Some(i),
        _ => None,
    };
    Some(ProbeResult { duration: info.duration, video_stream, title: info.title.clone(), artist: info.artist.clone() })
}
