//! The demuxer thread: opens the input, starts the decoder threads and keeps
//! their packet queues filled. Also executes seeks and track switches.

use super::av::{self, Dict, FormatCtx, Packet, ff};
use super::decoder::{Decoder, open_codec};
use super::types::*;
use super::{LoadOptions, SessionCtx, audio, present, subs, video};
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const MAX_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const MIN_FRAMES: usize = 25;
/// Read-ahead for files and discs, in seconds (streams use the network cache setting).
const LOCAL_READ_AHEAD_SECS: f64 = 3.0;

pub(super) unsafe extern "C" fn interrupt_cb(opaque: *mut c_void) -> c_int {
    let ctx = unsafe { &*(opaque as *const SessionCtx) };
    (!ctx.alive()) as c_int
}

/// A running decoder thread for one stream.
pub(super) struct Component {
    pub stream: usize,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Component {
    pub fn stop(self) {
        self.stop.store(true, Ordering::Release);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

pub fn open_input(source: &MediaSource, cfg: &EngineConfig, ctx: Option<&SessionCtx>) -> anyhow::Result<FormatCtx> {
    unsafe {
        let mut cd_toc = None;
        let mut ic = match source {
            MediaSource::Cd { device, track } => {
                let (io, toc) = super::cdda::open_track(device, *track)?;
                cd_toc = Some(toc);
                av::custom_io_context(io, 64 * 1024)?
            }
            _ => ff::avformat_alloc_context(),
        };
        if ic.is_null() {
            anyhow::bail!("out of memory");
        }
        if let Some(ctx) = ctx {
            (*ic).interrupt_callback.callback = Some(interrupt_cb);
            (*ic).interrupt_callback.opaque = ctx as *const SessionCtx as *mut c_void;
        }
        let mut opts = Dict::new();
        let mut fmt: *const ff::AVInputFormat = ptr::null();
        let url = match source {
            MediaSource::File(p) => file_url(p),
            MediaSource::Url(u) => {
                opts.set("user_agent", &cfg.user_agent);
                if u.starts_with("http") {
                    opts.set("reconnect", "1");
                    opts.set("reconnect_streamed", "1");
                    opts.set("reconnect_on_network_error", "1");
                    opts.set("reconnect_delay_max", "5");
                }
                opts.set("rw_timeout", "20000000");
                u.clone()
            }
            MediaSource::Dvd { path, title, angle } => {
                let name = av::cstr("dvdvideo");
                fmt = ff::av_find_input_format(name.as_ptr());
                if fmt.is_null() {
                    anyhow::bail!("this FFmpeg build has no DVD-Video support (needs libdvdnav/libdvdread)");
                }
                if *title > 0 {
                    opts.set("title", &title.to_string());
                }
                if *angle > 1 {
                    opts.set("angle", &angle.to_string());
                }
                if cfg.dvd_region > 0 {
                    opts.set("region", &cfg.dvd_region.to_string());
                }
                path.clone()
            }
            MediaSource::Cd { .. } => {
                // Raw CD audio: 44.1 kHz 16-bit stereo, little-endian.
                fmt = ff::av_find_input_format(av::cstr("s16le").as_ptr());
                opts.set("sample_rate", "44100");
                opts.set("ch_layout", "stereo");
                String::new()
            }
        };
        opts.set("scan_all_pmts", "1");
        let curl = av::cstr(&url);
        // Custom I/O belongs to us until the context is opened.
        let io = (*ic).opaque;
        let r = ff::avformat_open_input(&mut ic, curl.as_ptr(), fmt, &mut opts.0);
        if r < 0 {
            // avformat_open_input frees `ic` on failure, but not custom I/O.
            av::free_custom_io(io);
            anyhow::bail!("{}", friendly_open_error(r, source));
        }
        let fctx = FormatCtx(ic);
        if let (Some(toc), MediaSource::Cd { track, .. }) = (&cd_toc, source) {
            set_cd_metadata(ic, toc, *track);
        }
        // The container's header usually lists the streams already: if one
        // is a video, start opening the hardware decoding device while the
        // streams are probed. (Not earlier: it costs about 100 MB, which a
        // music file shouldn't pay.)
        if ctx.is_some() && has_video(ic) {
            super::decoder::prewarm_hw(cfg.hwdec);
            super::upscale::prewarm(cfg);
        }
        let r = ff::avformat_find_stream_info(ic, ptr::null_mut());
        if ctx.is_some() {
            log::debug!("startup: media opened at {:.0} ms", crate::ms_since_start());
        }
        if r < 0 {
            log::warn!("could not find stream info: {}", av::err_str(r));
        }
        Ok(fctx)
    }
}

/// Track names from CD-Text or the cue sheet, as the container's tags.
unsafe fn set_cd_metadata(ic: *mut ff::AVFormatContext, toc: &super::cdda::CdToc, n: u32) {
    let set = |k: &str, v: &str| unsafe {
        ff::av_dict_set(&mut (*ic).metadata, av::cstr(k).as_ptr(), av::cstr(v).as_ptr(), 0);
    };
    let t = toc.track(n);
    set("title", t.and_then(|t| t.title.as_deref()).unwrap_or(&format!("Track {n}")));
    if let Some(a) = t.and_then(|t| t.artist.as_deref()) {
        set("artist", a);
    }
    if let Some(a) = toc.album.as_deref() {
        set("album", a);
    }
    if let Some(a) = toc.artist.as_deref() {
        set("album_artist", a);
    }
    set("track", &format!("{n}/{}", toc.tracks.len()));
}

/// A local path as an FFmpeg URL. The explicit `file:` protocol keeps names
/// such as `concat:a|b`, `subfile:…` or `pipe:0` (from a playlist or the
/// command line) from being interpreted as other FFmpeg protocols.
fn file_url(p: &std::path::Path) -> String {
    format!("file:{}", p.to_string_lossy())
}

fn friendly_open_error(code: c_int, source: &MediaSource) -> String {
    let base = av::err_str(code);
    match source {
        MediaSource::File(p) if !p.exists() => format!("File not found: {}", p.display()),
        MediaSource::Dvd { .. } => format!("Could not read the DVD ({base}). Encrypted discs need libdvdcss."),
        MediaSource::Cd { .. } => format!("Could not read the audio CD ({base})"),
        _ => format!("Could not open media: {base}"),
    }
}

pub fn build_media_info(ic: &FormatCtx, source: &MediaSource) -> MediaInfo {
    unsafe {
        let f = ic.get();
        let mut info = MediaInfo { source_name: source.display_name(), ..Default::default() };
        if !f.iformat.is_null() {
            info.format_name = av::from_cstr((*f.iformat).name).unwrap_or_default();
            info.format_long_name = av::from_cstr((*f.iformat).long_name).unwrap_or_default();
        }
        info.duration = if f.duration > 0 { f.duration as f64 / ff::AV_TIME_BASE as f64 } else { 0.0 };
        info.start_time = if f.start_time != av::NOPTS { f.start_time as f64 / ff::AV_TIME_BASE as f64 } else { 0.0 };
        info.bitrate = f.bit_rate;
        info.is_live = info.duration <= 0.0 && matches!(source, MediaSource::Url(_));
        info.seekable = !f.pb.is_null() && (*f.pb).seekable != 0 || matches!(source, MediaSource::Dvd { .. });
        if f.pb.is_null() && !matches!(source, MediaSource::Url(_)) {
            info.seekable = true;
        }
        info.metadata = av::dict_all(f.metadata);
        info.title = av::dict_get(f.metadata, "title");
        info.artist = av::dict_get(f.metadata, "artist").or_else(|| av::dict_get(f.metadata, "album_artist"));
        info.album = av::dict_get(f.metadata, "album");
        if let MediaSource::File(p) = source {
            info.file_size = std::fs::metadata(p).ok().map(|m| m.len());
        }
        for i in 0..f.nb_chapters as usize {
            let ch = &**f.chapters.add(i);
            let tb = ch.time_base;
            info.chapters.push(Chapter {
                start: av::ts_to_secs(ch.start, tb) - info.start_time,
                end: av::ts_to_secs(ch.end, tb) - info.start_time,
                title: av::dict_get(ch.metadata, "title").unwrap_or_else(|| format!("Chapter {}", i + 1)),
            });
        }
        for i in 0..ic.nb_streams() {
            let st = &*ic.stream(i);
            let par = &*st.codecpar;
            let kind = match par.codec_type {
                ff::AVMediaType::AVMEDIA_TYPE_VIDEO => TrackKind::Video,
                ff::AVMediaType::AVMEDIA_TYPE_AUDIO => TrackKind::Audio,
                ff::AVMediaType::AVMEDIA_TYPE_SUBTITLE => TrackKind::Subtitle,
                _ => continue,
            };
            // Ogg/Vorbis-style streams keep tags on the stream.
            if info.title.is_none() {
                info.title = av::dict_get(st.metadata, "title").filter(|_| kind == TrackKind::Audio);
            }
            if info.artist.is_none() && kind == TrackKind::Audio {
                info.artist = av::dict_get(st.metadata, "artist");
            }
            let disp = st.disposition;
            let fr = ff::av_guess_frame_rate(ic.0, ic.stream(i), ptr::null_mut());
            info.tracks.push(Track {
                id: TrackId::Stream(i),
                kind,
                codec: av::codec_name(par.codec_id),
                language: av::dict_get(st.metadata, "language"),
                title: av::dict_get(st.metadata, "title").filter(|_| kind != TrackKind::Audio || info.title.is_some()),
                default: disp & ff::AV_DISPOSITION_DEFAULT as c_int != 0,
                forced: disp & ff::AV_DISPOSITION_FORCED as c_int != 0,
                hearing_impaired: disp & ff::AV_DISPOSITION_HEARING_IMPAIRED as c_int != 0,
                width: par.width,
                height: par.height,
                fps: av::q2d(fr),
                is_cover_art: disp & ff::AV_DISPOSITION_ATTACHED_PIC as c_int != 0,
                channels: par.ch_layout.nb_channels,
                channel_layout: if kind == TrackKind::Audio {
                    av::channel_layout_name(&par.ch_layout)
                } else {
                    String::new()
                },
                sample_rate: par.sample_rate,
                bitrate: par.bit_rate,
                external_path: None,
            });
        }
        // Don't repeat the container title as every audio track's name.
        info
    }
}

fn pick_by_lang<'a>(tracks: impl Iterator<Item = &'a Track>, langs: &[String]) -> Option<&'a Track> {
    let tracks: Vec<&Track> = tracks.collect();
    for l in langs {
        if let Some(t) = tracks.iter().find(|t| t.language.as_deref().is_some_and(|tl| language_matches(tl, l))) {
            return Some(t);
        }
    }
    None
}

pub(super) fn choose_tracks(ic: &FormatCtx, info: &MediaInfo, cfg: &EngineConfig) -> Selected {
    let best = |ty: ff::AVMediaType, related: c_int| unsafe {
        let r = ff::av_find_best_stream(ic.0, ty, -1, related, ptr::null_mut(), 0);
        if r >= 0 { Some(r as usize) } else { None }
    };
    let video = info
        .tracks_of(TrackKind::Video)
        .find(|t| !t.is_cover_art && t.default)
        .map(|t| t.id)
        .or_else(|| best(ff::AVMediaType::AVMEDIA_TYPE_VIDEO, -1).map(TrackId::Stream))
        .or_else(|| info.tracks_of(TrackKind::Video).next().map(|t| t.id));
    let vidx = match video {
        Some(TrackId::Stream(i)) => i as c_int,
        _ => -1,
    };
    let audio = pick_by_lang(info.tracks_of(TrackKind::Audio), &cfg.audio_langs)
        .map(|t| t.id)
        .or_else(|| best(ff::AVMediaType::AVMEDIA_TYPE_AUDIO, vidx).map(TrackId::Stream));
    let subtitle = if cfg.show_subs { super::types::pick_subtitle(info, &cfg.sub_langs) } else { None };
    Selected { video, audio, subtitle }
}

pub(super) fn stream_index(id: Option<TrackId>) -> Option<usize> {
    match id {
        Some(TrackId::Stream(i)) => Some(i),
        _ => None,
    }
}

pub(super) fn start_component(
    ctx: &Arc<SessionCtx>,
    ic: &FormatCtx,
    kind: TrackKind,
    idx: usize,
    cfg: &EngineConfig,
) -> anyhow::Result<Component> {
    let stream = ic.stream(idx);
    unsafe { (*stream).discard = ff::AVDiscard::AVDISCARD_DEFAULT };
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    match kind {
        TrackKind::Video => {
            // Cover art is one picture: no hardware device for it.
            let cover = unsafe { (*stream).disposition & ff::AV_DISPOSITION_ATTACHED_PIC as c_int != 0 };
            let opened = open_codec(stream, cfg.decoder_threads, (!cover).then_some(cfg.hwdec), None)?;
            let hw = opened.hw.as_ref().map(|h| h.name.clone());
            let decoder_name = opened.decoder_name.clone();
            let dec = Decoder::new(opened, ctx.vq.clone(), stop.clone());
            let info = video::StreamInfo::from_stream(ic, idx);
            ctx.update_state(|st| {
                st.video = Some(VideoParams {
                    width: info.width,
                    height: info.height,
                    display_aspect: info.display_aspect(),
                    hwdec: hw.clone(),
                    fps: info.fps,
                    decoder: decoder_name.clone(),
                    ..Default::default()
                })
            });
            ctx.video_active.store(!info.attached_pic, Ordering::Release);
            ctx.cover_art.store(info.attached_pic, Ordering::Release);
            let (c, s) = (ctx.clone(), stop.clone());
            threads.push(spawn("video-dec", move || video::run_decoder(c, dec, info, s)));
            let (c, s) = (ctx.clone(), stop.clone());
            threads.push(spawn("present", move || present::run(c, s)));
        }
        TrackKind::Audio => {
            let opened = open_codec(stream, cfg.decoder_threads, None, None)?;
            let decoder_name = opened.decoder_name.clone();
            let par = unsafe { &*(*stream).codecpar };
            let in_rate = par.sample_rate;
            let in_layout = av::channel_layout_name(&par.ch_layout);
            let in_format = av::sample_fmt_name(par.format);
            let media_channels = par.ch_layout.nb_channels;
            let tb = unsafe { (*stream).time_base };
            let dec = Decoder::new(opened, ctx.aq.clone(), stop.clone());
            // Opening a device can take a while (or hang on a broken sound
            // server), so do it on the audio thread. Video shows its first
            // frame immediately and starts moving once audio is ready.
            let token = ctx.audio_tokens.fetch_add(1, Ordering::Relaxed) + 1;
            ctx.audio_pending.store(token, Ordering::Release);
            let (c, s, cfg) = (ctx.clone(), stop.clone(), cfg.clone());
            threads.push(spawn("audio-dec", move || {
                let pending = PendingAudio(&c, token);
                let publish = |fmt: &audio::OutFormat| {
                    c.update_state(|st| {
                        st.audio = Some(AudioParams {
                            decoder: decoder_name.clone(),
                            in_rate,
                            in_layout: in_layout.clone(),
                            in_format: in_format.clone(),
                            out_rate: fmt.rate,
                            out_channels: fmt.channels,
                            device: fmt.device.clone(),
                        })
                    });
                    c.audio_active.store(true, Ordering::Release);
                };
                // Preloaded next item: continue on the device that is playing now.
                if let Some((link, fmt)) = audio::adopt_output(&c, &s, &cfg, media_channels, token) {
                    log::debug!("audio queued gaplessly on the open output");
                    publish(&fmt);
                    // `audio_pending` stays set until the output switches over.
                    std::mem::forget(pending);
                    audio::run_decoder(c.clone(), dec, tb, fmt, s);
                    audio::finish_output(&link, c.id, None);
                    return;
                }
                log::debug!("startup: opening audio device at {:.0} ms", crate::ms_since_start());
                match audio::open_output(c.clone(), s.clone(), &cfg, media_channels) {
                    Ok((out_thread, fmt, link)) => {
                        log::info!("audio output ready: {} ({} Hz, {} ch)", fmt.device, fmt.rate, fmt.channels);
                        publish(&fmt);
                        drop(pending);
                        audio::run_decoder(c.clone(), dec, tb, fmt, s);
                        audio::finish_output(&link, c.id, Some(out_thread));
                    }
                    Err(e) => {
                        log::warn!("audio output failed: {e:#}");
                        c.emit(PlayerEvent::Warning(format!("No audio output: {e}")));
                        drop(pending);
                        // Keep the demuxer from stalling on a full audio queue.
                        while c.aq.get(&s).is_some() {}
                    }
                }
            }));
        }
        TrackKind::Subtitle => {
            let opened = open_codec(stream, 1, None, cfg.sub_charset.as_deref())?;
            let tb = unsafe { (*stream).time_base };
            let (c, s) = (ctx.clone(), stop.clone());
            let q = ctx.sq.clone();
            ctx.subs.clear_embedded();
            // DVDs played with their menus draw subpictures (and buttons) themselves.
            #[cfg(has_dvdnav)]
            if let Some(ov) = ctx.spu_overlay.lock().clone() {
                drop(opened);
                threads.push(spawn("sub-dec", move || super::spu::run_decoder(c, ov, q, tb, s)));
                return Ok(Component { stream: idx, stop, threads });
            }
            threads.push(spawn("sub-dec", move || subs::run_decoder(c, opened, q, tb, s)));
        }
    }
    Ok(Component { stream: idx, stop, threads })
}

/// Clears `audio_pending` when dropped, unless a newer audio component has
/// taken over the flag in the meantime.
struct PendingAudio<'a>(&'a SessionCtx, u64);

impl Drop for PendingAudio<'_> {
    fn drop(&mut self) {
        let _ = self.0.audio_pending.compare_exchange(self.1, 0, Ordering::AcqRel, Ordering::Relaxed);
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new().name(name.into()).spawn(f).expect("spawn thread")
}

pub fn run(ctx: Arc<SessionCtx>, source: MediaSource, opts: LoadOptions) {
    let cfg = ctx.controls.config.lock().clone();
    let _hold = match &source {
        MediaSource::Dvd { path, .. } => super::disc::hold(path),
        _ => None,
    };
    // With libdvdnav, single titles go through the navigator too: FFmpeg's
    // `dvdvideo` demuxer drops audio frames (see `nav::HeldAudio`).
    #[cfg(has_dvdnav)]
    if let MediaSource::Dvd { path, .. } = &source {
        let path = path.clone();
        return super::nav::run(ctx, source, path, &opts);
    }
    let ic = match open_input(&source, &cfg, Some(&ctx)) {
        Ok(ic) => ic,
        Err(e) => {
            if ctx.alive() {
                let msg = e.to_string();
                ctx.update_state(|st| st.status = Status::Error(msg.clone()));
                ctx.emit(PlayerEvent::Error(msg));
            }
            return;
        }
    };
    if !ctx.alive() {
        return;
    }
    let mut info = build_media_info(&ic, &source);
    if let MediaSource::File(path) = &source {
        info.camera = super::camera_metadata::read(&super::camera_metadata::candidates(path));
    }
    let info = Arc::new(info);
    ctx.start_time.set(info.start_time);
    ctx.duration.set(info.duration);
    for i in 0..ic.nb_streams() {
        unsafe { (*ic.stream(i)).discard = ff::AVDiscard::AVDISCARD_ALL };
    }

    let mut selected = choose_tracks(&ic, &info, &cfg);
    let mut video_c: Option<Component> = None;
    let mut audio_c: Option<Component> = None;
    let mut sub_c: Option<Component> = None;

    if let Some(i) = stream_index(selected.audio) {
        match start_component(&ctx, &ic, TrackKind::Audio, i, &cfg) {
            Ok(c) => {
                log::debug!("startup: audio decoder ready at {:.0} ms", crate::ms_since_start());
                audio_c = Some(c)
            }
            Err(e) => {
                log::warn!("audio: {e}");
                selected.audio = None;
            }
        }
    }
    if let Some(i) = stream_index(selected.video) {
        match start_component(&ctx, &ic, TrackKind::Video, i, &cfg) {
            Ok(c) => {
                log::debug!("startup: video decoder ready at {:.0} ms", crate::ms_since_start());
                video_c = Some(c)
            }
            Err(e) => {
                ctx.emit(PlayerEvent::Warning(format!("Video: {e}")));
                selected.video = None;
            }
        }
    }
    if let Some(i) = stream_index(selected.subtitle) {
        match start_component(&ctx, &ic, TrackKind::Subtitle, i, &cfg) {
            Ok(c) => sub_c = Some(c),
            Err(e) => {
                log::warn!("subtitles: {e}");
                selected.subtitle = None;
            }
        }
    }
    if audio_c.is_none() && video_c.is_none() {
        let msg = "No playable audio or video streams".to_string();
        ctx.update_state(|st| st.status = Status::Error(msg.clone()));
        ctx.emit(PlayerEvent::Error(msg));
        return;
    }

    let paused = ctx.paused();
    ctx.update_state(|st| {
        st.media = Some(info.clone());
        st.selected = selected.clone();
        st.status = if paused { Status::Paused } else { Status::Playing };
    });
    ctx.emit(PlayerEvent::Loaded);

    // External subtitles: explicit ones first, then same-name files next to the media.
    let mut ext = opts.external_subs.clone();
    if cfg.auto_load_subs
        && let MediaSource::File(p) = &source
    {
        ext.extend(subs::find_sidecar_subtitles(p));
    }
    let want_external = selected.subtitle.is_none() && cfg.show_subs;
    for (n, p) in ext.into_iter().enumerate() {
        subs::load_external_async(ctx.clone(), p, n == 0 && (want_external || !opts.external_subs.is_empty()));
    }

    if let Some(t) = opts.start_at.filter(|t| *t > 0.5) {
        ctx.request_seek(super::SeekRequest { target: t, relative: false, precise: true, direction: 0, origin: 0.0 });
    }

    let mut pkt = Packet::new();
    let mut was_paused = false;
    let mut eof_reported = false;
    let mut last_read_error_logged = false;
    while ctx.alive() {
        // Pause network streams at the protocol level (e.g. RTSP).
        let paused = ctx.paused();
        if paused != was_paused {
            unsafe {
                if paused {
                    ff::av_read_pause(ic.0);
                } else {
                    ff::av_read_play(ic.0);
                }
            }
            was_paused = paused;
        }

        // Track switches.
        let switches: Vec<_> = std::mem::take(&mut *ctx.switch_req.lock());
        if !switches.is_empty() {
            let pos = ctx.position();
            for (kind, id) in switches {
                let slot = match kind {
                    TrackKind::Video => &mut video_c,
                    TrackKind::Audio => &mut audio_c,
                    TrackKind::Subtitle => &mut sub_c,
                };
                let target = stream_index(id);
                if slot.as_ref().map(|c| c.stream) == target {
                    continue;
                }
                if let Some(c) = slot.take() {
                    unsafe { (*ic.stream(c.stream)).discard = ff::AVDiscard::AVDISCARD_ALL };
                    match kind {
                        TrackKind::Audio => {
                            ctx.audio_active.store(false, Ordering::Release);
                            ctx.aq.flush();
                            ctx.abuf.flush(ctx.aq.serial());
                        }
                        TrackKind::Video => {
                            ctx.video_active.store(false, Ordering::Release);
                            ctx.cover_art.store(false, Ordering::Release);
                            ctx.vq.flush();
                            ctx.frames.clear();
                        }
                        TrackKind::Subtitle => {
                            ctx.sq.flush();
                            ctx.subs.clear_embedded();
                        }
                    }
                    c.stop();
                }
                if let Some(i) = target {
                    match start_component(&ctx, &ic, kind, i, &cfg) {
                        Ok(c) => *slot = Some(c),
                        Err(e) => ctx.emit(PlayerEvent::Warning(format!("Could not switch track: {e}"))),
                    }
                }
                if kind == TrackKind::Video && video_c.is_none() {
                    *ctx.last_frame.lock() = None;
                    *ctx.shared.frame_slot.lock() = None;
                }
            }
            ctx.emit(PlayerEvent::TracksChanged);
            if info.seekable {
                ctx.request_seek(super::SeekRequest {
                    target: pos,
                    relative: false,
                    precise: true,
                    direction: 0,
                    origin: 0.0,
                });
            }
        }

        // Seeks.
        // Release the request lock before performing potentially slow seeks.
        let request = ctx.seek_req.lock().take();
        if let Some(req) = request {
            let start = ctx.start_time.get();
            let dur = ctx.duration.get();
            let mut target = req.target.max(0.0);
            if dur > 0.0 {
                target = target.min((dur - 0.05).max(0.0));
            }
            let ts = ((start + target) * ff::AV_TIME_BASE as f64) as i64;
            // Approximate relative seeks choose a keyframe nearest the target,
            // but never on the opposite side of the original playback position.
            // Exact and absolute seeks retain their previous ranges.
            let origin_ts = ((start + req.origin.max(0.0)) * ff::AV_TIME_BASE as f64) as i64;
            let (min_ts, max_ts) = if req.precise {
                (i64::MIN, ts)
            } else if req.direction > 0 {
                (origin_ts.saturating_add(1).min(ts), i64::MAX)
            } else if req.direction < 0 {
                (i64::MIN, origin_ts.saturating_sub(1).max(ts))
            } else {
                (i64::MIN, i64::MAX)
            };
            let r = unsafe { ff::avformat_seek_file(ic.0, -1, min_ts, ts, max_ts, 0) };
            let r = if r < 0 {
                // Old demuxers may not support avformat_seek_file. Preserve
                // direction in the fallback instead of always seeking backward.
                let flags = if !req.precise && req.direction > 0 { 0 } else { ff::AVSEEK_FLAG_BACKWARD as c_int };
                unsafe { ff::av_seek_frame(ic.0, -1, ts, flags) }
            } else {
                r
            };
            if r < 0 {
                log::warn!("seek failed: {}", av::err_str(r));
                ctx.seeking.store(false, Ordering::Release);
            } else {
                ctx.vq.flush();
                ctx.aq.flush();
                ctx.sq.flush();
                ctx.abuf.flush(ctx.aq.serial());
                let serial = ctx.vq.serial().max(ctx.aq.serial());
                *ctx.seek_target.lock() = if req.precise { Some((start + target, serial)) } else { None };
                ctx.vclk.set(start + target, ctx.vq.serial());
                ctx.eof.store(false, Ordering::Release);
                eof_reported = false;
                if ctx.paused() {
                    ctx.show_one.store(true, Ordering::Release);
                }
                if video_c.is_none() || !ctx.video_active.load(Ordering::Acquire) {
                    // Nothing visual will clear the flag; audio clock will be valid soon.
                    ctx.seeking.store(false, Ordering::Release);
                }
                ctx.update_state(|st| {
                    if st.status == Status::Ended {
                        st.status = if ctx.paused() { Status::Paused } else { Status::Playing };
                    }
                });
            }
            ctx.wake();
        }

        // Cover art to decode again (`SessionCtx::redo_cover`).
        if ctx.cover_redo.swap(false, Ordering::AcqRel) {
            queue_attached_pic(&ctx, &ic, &video_c, true);
        }

        // Back-pressure: stop reading when every queue has enough.
        let enough = |q: &super::sync::PacketQueue, active: bool| !active || q.len() > MIN_FRAMES;
        // How far to read ahead. A stream needs a cushion against the
        // network; a file or disc only against a slow disk, and every second
        // of a high-bitrate film read ahead is several MB held in memory.
        let ahead = if matches!(source, MediaSource::Url(_)) {
            cfg.network_cache_secs.min(if info.is_live { 4.0 } else { 120.0 })
        } else {
            cfg.network_cache_secs.min(LOCAL_READ_AHEAD_SECS)
        };
        let total = ctx.vq.bytes() + ctx.aq.bytes() + ctx.sq.bytes();
        let cached = cache_ahead(&ctx, &ic, &audio_c, &video_c);
        let full = total > MAX_QUEUE_BYTES
            || (enough(&ctx.vq, video_c.is_some() && ctx.video_active.load(Ordering::Acquire))
                && enough(&ctx.aq, audio_c.is_some())
                && cached > ahead.max(1.0));
        update_cache_state(&ctx, cached, read_until(&ctx, &ic, &audio_c, &video_c), audio_c.is_some());
        if full || (ctx.eof.load(Ordering::Acquire) && ctx.seek_req.lock().is_none()) {
            if ctx.eof.load(Ordering::Acquire) {
                super::preload_next(&ctx);
                let audio_done = audio_c.is_none()
                    || (ctx.audio_finished.load(Ordering::Acquire) == ctx.aq.serial() && ctx.abuf.is_empty());
                let video_done = video_c.is_none()
                    || !ctx.video_active.load(Ordering::Acquire)
                    || (ctx.video_finished.load(Ordering::Acquire) == ctx.vq.serial() && ctx.frames.len() == 0);
                if audio_done && video_done && !ctx.paused() && !eof_reported && ctx.is_current() {
                    eof_reported = true;
                    if !super::advance(&ctx) {
                        ctx.update_state(|st| st.status = Status::Ended);
                        ctx.emit(PlayerEvent::EndOfFile);
                    }
                }
            }
            let mut g = ctx.demux_lock.lock();
            // Once the end is reported, nothing changes until a seek, a track
            // switch, a pause or a stop, which all wake this thread: no need to
            // poll 100 times a second. (A seek asked for just before the lock
            // was taken would not wake it, hence the check.)
            let ended = eof_reported && ctx.seek_req.lock().is_none() && ctx.switch_req.lock().is_empty();
            ctx.demux_cv.wait_for(&mut g, Duration::from_millis(if ended { 100 } else { 10 }));
            continue;
        }

        let r = unsafe { ff::av_read_frame(ic.0, pkt.0) };
        if r < 0 {
            // A failed I/O context (after FFmpeg's own reconnect attempts)
            // won't recover either: end the input rather than retry forever.
            let at_eof = r == ff::AVERROR_EOF
                || unsafe {
                    let pb = (*ic.0).pb;
                    !pb.is_null() && (ff::avio_feof(pb) != 0 || (*pb).error < 0)
                };
            if at_eof || r == ff::AVERROR_EXIT {
                if !ctx.eof.swap(true, Ordering::AcqRel) {
                    if video_c.is_some() {
                        ctx.vq.put_drain();
                    }
                    if audio_c.is_some() {
                        ctx.aq.put_drain();
                    }
                    if sub_c.is_some() {
                        ctx.sq.put_drain();
                    }
                }
            } else if !last_read_error_logged {
                log::warn!("read error: {}", av::err_str(r));
                last_read_error_logged = true;
            }
            let mut g = ctx.demux_lock.lock();
            ctx.demux_cv.wait_for(&mut g, Duration::from_millis(10));
            continue;
        }
        last_read_error_logged = false;
        ctx.eof.store(false, Ordering::Release);
        let si = pkt.stream_index() as usize;
        if audio_c.as_ref().is_some_and(|c| c.stream == si) {
            ctx.aq.put(pkt.take());
        } else if video_c.as_ref().is_some_and(|c| c.stream == si) {
            let attached = unsafe { (*ic.stream(si)).disposition & ff::AV_DISPOSITION_ATTACHED_PIC as c_int != 0 };
            if !attached {
                ctx.vq.put(pkt.take());
            } else {
                pkt.unref();
            }
        } else if sub_c.as_ref().is_some_and(|c| c.stream == si) {
            ctx.sq.put(pkt.take());
        } else {
            pkt.unref();
        }
        // Cover art is delivered once via `attached_pic`, not by the demuxer.
        queue_attached_pic(&ctx, &ic, &video_c, false);
    }

    ctx.vq.abort();
    ctx.aq.abort();
    ctx.sq.abort();
    ctx.frames.abort();
    ctx.abuf.abort();
    for c in [video_c, audio_c, sub_c].into_iter().flatten() {
        c.stop();
    }
}

thread_local! {
    static ATTACHED_SERIAL: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
}

/// Queues the cover art picture for decoding: once per serial (after each
/// seek), or again with `again` (the picture shows at once even if paused).
fn queue_attached_pic(ctx: &SessionCtx, ic: &FormatCtx, video_c: &Option<Component>, again: bool) {
    let Some(c) = video_c else { return };
    let st = ic.stream(c.stream);
    unsafe {
        if (*st).disposition & ff::AV_DISPOSITION_ATTACHED_PIC as c_int == 0 {
            return;
        }
        let serial = ctx.vq.serial();
        if ATTACHED_SERIAL.with(|s| s.get()) == serial && !again {
            return;
        }
        ATTACHED_SERIAL.with(|s| s.set(serial));
        let ap = Packet(ff::av_packet_clone(&(*st).attached_pic));
        if !ap.0.is_null() {
            if again && ctx.paused() {
                ctx.show_one.store(true, Ordering::Release);
            }
            ctx.vq.put(ap);
            ctx.vq.put_drain();
        }
    }
}

/// A stream that plays as video (not cover art).
unsafe fn has_video(ic: *mut ff::AVFormatContext) -> bool {
    unsafe {
        (0..(*ic).nb_streams as usize).any(|i| {
            let st = *(*ic).streams.add(i);
            (*(*st).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_VIDEO
                && (*st).disposition & ff::AV_DISPOSITION_ATTACHED_PIC as c_int == 0
        })
    }
}

fn cache_ahead(ctx: &SessionCtx, ic: &FormatCtx, a: &Option<Component>, v: &Option<Component>) -> f64 {
    let dur = |q: &super::sync::PacketQueue, c: &Option<Component>| -> f64 {
        match c {
            Some(c) => {
                let tb = unsafe { (*ic.stream(c.stream)).time_base };
                q.duration_ts() as f64 * av::q2d(tb)
            }
            None => f64::INFINITY,
        }
    };
    let ad = dur(&ctx.aq, a);
    let vd = if ctx.video_active.load(Ordering::Acquire) { dur(&ctx.vq, v) } else { f64::INFINITY };
    let m = ad.min(vd);
    if m.is_finite() { m } else { 0.0 }
}

/// How far the demuxer has read, for the seek bar: where the packets read
/// last end (the stream furthest behind counts), or the whole duration once
/// it reached the end. Unlike `cache_ahead`, this doesn't drop each time a
/// decoder takes packets off a queue.
fn read_until(ctx: &SessionCtx, ic: &FormatCtx, a: &Option<Component>, v: &Option<Component>) -> f64 {
    if ctx.eof.load(Ordering::Acquire) {
        return ctx.duration.get();
    }
    let end = |q: &super::sync::PacketQueue, c: &Option<Component>| -> Option<f64> {
        let c = c.as_ref()?;
        let tb = unsafe { (*ic.stream(c.stream)).time_base };
        Some(q.end_ts().map_or(f64::NAN, |e| e as f64 * av::q2d(tb)))
    };
    let video = if ctx.video_active.load(Ordering::Acquire) { end(&ctx.vq, v) } else { None };
    // NaN (a stream with nothing read yet) wins, so the bar waits for both.
    let furthest = match (end(&ctx.aq, a), video) {
        (Some(x), Some(y)) if x.is_nan() || y.is_nan() => f64::NAN,
        (Some(x), Some(y)) => x.min(y),
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => f64::NAN,
    };
    furthest - ctx.start_time.get()
}

fn update_cache_state(ctx: &SessionCtx, cached: f64, read_until: f64, has_audio: bool) {
    if ctx.is_current()
        && let Some(mut st) = ctx.shared.state.try_lock()
    {
        st.cache_secs = cached;
        st.cache_end = read_until;
        let empty = (has_audio && ctx.aq.len() == 0 && ctx.abuf.is_empty()) && !ctx.eof.load(Ordering::Acquire);
        st.buffering = empty && !ctx.paused();
        st.seeking = ctx.seeking.load(Ordering::Acquire);
    }
}
