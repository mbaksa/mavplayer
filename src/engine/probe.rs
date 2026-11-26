//! Lightweight metadata probing for playlist entries.

use super::demux;
use super::types::*;

#[derive(Clone, Debug, Default)]
pub struct ProbeResult {
    pub duration: f64,
    pub video_stream: Option<usize>,
    pub audio_stream: Option<usize>,
    pub has_subtitles: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
}

pub fn probe(source: &MediaSource, cfg: &EngineConfig) -> Option<ProbeResult> {
    if !matches!(source, MediaSource::File(_)) {
        return None;
    }
    let ic = demux::open_input(source, cfg, None).ok()?;
    let info = demux::build_media_info(&ic, source);
    let selected = demux::choose_tracks(&ic, &info, cfg);
    let video_stream = match selected.video {
        Some(TrackId::Stream(i)) if info.tracks.iter().any(|t| t.id == TrackId::Stream(i) && !t.is_cover_art) && info.seekable => Some(i),
        _ => None,
    };
    let audio_stream = match selected.audio { Some(TrackId::Stream(i)) if info.seekable => Some(i), _ => None };
    let has_subtitles = info.tracks.iter().any(|t| t.kind == TrackKind::Subtitle)
        || (video_stream.is_some() && matches!(source, MediaSource::File(path) if !super::subs::find_sidecar_subtitles(path).is_empty()));
    Some(ProbeResult { duration: info.duration, video_stream, audio_stream, has_subtitles, title: info.title.clone(), artist: info.artist.clone() })
}
