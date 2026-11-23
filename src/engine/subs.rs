//! Subtitles: decoding embedded streams, loading external files, and
//! answering "what is on screen at time t".

use super::SessionCtx;
use super::av::{self, Packet, ff};
use super::decoder::OpenedCodec;
use super::demux;
use super::sync::PacketQueue;
use super::types::*;
use parking_lot::Mutex;
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static BITMAP_IDS: AtomicU64 = AtomicU64::new(1);

/// Largest subtitle bitmap accepted (a full 8K frame).
const MAX_SUB_BITMAP_PIXELS: u64 = 7680 * 4320;

/// How long bitmap subtitles are kept after they end: enough for any
/// realistic subtitle delay (the UI shows those of `position - delay`).
const KEEP_PAST: f64 = 60.0;

/// How far from the lookup time an external bitmap event keeps its BGRA
/// picture once shown.
const KEEP_EXPANDED: f64 = 2.0;

/// A decoded subtitle event, with bitmaps in whatever form `B` its user
/// keeps them.
struct Cue<B> {
    start: f64,
    end: f64,
    content: CueContent<B>,
}

enum CueContent<B> {
    Text(String),
    Bitmaps(Vec<B>),
}

impl Cue<SubBitmap> {
    fn into_event(self) -> SubEvent {
        let content = match self.content {
            CueContent::Text(s) => SubContent::Text(s),
            CueContent::Bitmaps(b) => SubContent::Bitmap(Arc::new(b)),
        };
        SubEvent { start: self.start, end: self.end, content }
    }
}

struct ExternalTrack {
    /// Sorted by start time.
    events: Vec<Cue<PackedBitmap>>,
    /// The longest event, which bounds how far back a lookup must search.
    max_len: f64,
    /// BGRA pictures of the bitmap events shown lately (by index in
    /// `events`), so that each keeps its `Arc` and ids while on screen.
    expanded: Vec<(usize, Arc<Vec<SubBitmap>>)>,
}

/// A bitmap rectangle of an external subtitle file, kept the way FFmpeg
/// decoded it: palette indices (PackBits-compressed) and the palette. It is
/// expanded to BGRA only while near the playback position: a film's worth of
/// 1080p PGS subtitles is hundreds of MB as BGRA, a few MB like this.
struct PackedBitmap {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    canvas_w: u32,
    canvas_h: u32,
    /// Shared by consecutive events with the same palette.
    palette: Arc<[u32; 256]>,
    pixels: Box<[u8]>,
}

impl PackedBitmap {
    fn expand(&self) -> SubBitmap {
        let len = self.w as usize * self.h as usize * 4;
        let mut bgra = Vec::with_capacity(len);
        let mut p = self.pixels.iter();
        while bgra.len() < len {
            let Some(&head) = p.next() else { break };
            if head < 128 {
                for &i in p.by_ref().take(head as usize + 1) {
                    bgra.extend_from_slice(&self.palette[i as usize].to_le_bytes());
                }
            } else {
                let Some(&i) = p.next() else { break };
                let px = self.palette[i as usize].to_le_bytes();
                for _ in 0..head as usize - 126 {
                    bgra.extend_from_slice(&px);
                }
            }
        }
        bgra.resize(len, 0);
        SubBitmap {
            id: BITMAP_IDS.fetch_add(1, Ordering::Relaxed),
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
            canvas_w: self.canvas_w,
            canvas_h: self.canvas_h,
            bgra,
        }
    }
}

/// PackBits: a header byte `n` < 128 is followed by `n + 1` literal bytes;
/// `n` ≥ 128 by one byte repeated `n - 126` (2 to 129) times.
fn packbits(src: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < src.len() {
        let run = src[i..].iter().take(129).take_while(|&&b| b == src[i]).count();
        if run >= 2 {
            out.push((run + 126) as u8);
            out.push(src[i]);
            i += run;
            continue;
        }
        let start = i;
        i += 1;
        // A literal ends where a run of 3 begins (a run of 2 saves nothing).
        while i < src.len() && i - start < 128 && !(i + 2 < src.len() && src[i] == src[i + 1] && src[i] == src[i + 2]) {
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&src[start..i]);
    }
}

/// The BGRA pictures of external bitmap event `i`, expanded on first use.
fn expanded_bitmaps(
    cache: &mut Vec<(usize, Arc<Vec<SubBitmap>>)>,
    i: usize,
    bitmaps: &[PackedBitmap],
) -> Arc<Vec<SubBitmap>> {
    if let Some((_, b)) = cache.iter().find(|(j, _)| *j == i) {
        return b.clone();
    }
    let b = Arc::new(bitmaps.iter().map(PackedBitmap::expand).collect::<Vec<_>>());
    cache.push((i, b.clone()));
    b
}

pub struct SubStore {
    embedded: Mutex<Vec<SubEvent>>,
    external: Mutex<Vec<ExternalTrack>>,
    active_external: Mutex<Option<usize>>,
}

impl SubStore {
    pub fn new() -> Self {
        Self { embedded: Mutex::new(Vec::new()), external: Mutex::new(Vec::new()), active_external: Mutex::new(None) }
    }

    pub fn clear_embedded(&self) {
        self.embedded.lock().clear();
    }

    pub fn select_external(&self, i: Option<usize>) {
        *self.active_external.lock() = i;
        for tr in self.external.lock().iter_mut() {
            tr.expanded.clear();
        }
    }

    pub(super) fn add_event(&self, ev: SubEvent) {
        let mut g = self.embedded.lock();
        // Bitmap formats (PGS, DVD, DVB) replace the previous picture.
        if matches!(ev.content, SubContent::Bitmap(_)) {
            for old in g.iter_mut().rev().take(8) {
                if matches!(old.content, SubContent::Bitmap(_)) && old.end > ev.start && old.start < ev.start {
                    old.end = ev.start;
                }
            }
        }
        g.push(ev);
        if g.len() > 4000 {
            g.drain(..1000);
        }
    }

    /// Drops bitmap events that ended more than [`KEEP_PAST`] before media
    /// time `now` (absolute, like the events). Each holds a BGRA picture
    /// (about 45 KB for a DVD line, far more for HD subtitles) and a seek
    /// decodes them again anyway, so keeping a film's worth cost tens to
    /// hundreds of MB. Text events are kept: they're small.
    pub(super) fn prune_bitmaps(&self, now: f64) {
        let cutoff = now - KEEP_PAST;
        if cutoff.is_finite() {
            self.embedded.lock().retain(|e| !matches!(e.content, SubContent::Bitmap(_)) || e.end >= cutoff);
        }
    }

    /// Replaces the bitmap event that starts when `ev` does (a DVD menu
    /// redrawn with another button highlighted), or adds it.
    #[cfg_attr(not(has_dvdnav), allow(dead_code))]
    pub(super) fn replace_bitmap(&self, ev: SubEvent) {
        let mut g = self.embedded.lock();
        match g.iter_mut().rev().find(|e| matches!(e.content, SubContent::Bitmap(_)) && e.start == ev.start) {
            Some(old) => old.content = ev.content,
            None => g.push(ev),
        }
    }

    /// A "clear screen" packet (bitmap subtitle with no rectangles).
    pub(super) fn end_bitmaps_at(&self, t: f64) {
        let mut g = self.embedded.lock();
        for old in g.iter_mut().rev().take(8) {
            if old.end > t && old.start <= t {
                old.end = t;
            }
        }
    }

    /// Events visible at media time `t` (seconds from the start of the media).
    /// Embedded events use absolute stream timestamps (offset by
    /// `start_time`); external files are already relative to the start.
    pub fn active_at(&self, t: f64, start_time: f64, out: &mut Vec<SubEvent>) {
        let ext = *self.active_external.lock();
        if let Some(i) = ext {
            let mut tracks = self.external.lock();
            if let Some(tr) = tracks.get_mut(i) {
                // Sorted by start time: only events that already began, and
                // no earlier than the longest one could still be showing.
                let first = out.len();
                let idx = tr.events.partition_point(|e| e.start <= t);
                for j in (0..idx).rev() {
                    let e = &tr.events[j];
                    if e.start < t - tr.max_len {
                        break;
                    }
                    if t < e.end {
                        let content = match &e.content {
                            CueContent::Text(s) => SubContent::Text(s.clone()),
                            CueContent::Bitmaps(b) => SubContent::Bitmap(expanded_bitmaps(&mut tr.expanded, j, b)),
                        };
                        out.push(SubEvent { start: e.start, end: e.end, content });
                    }
                }
                out[first..].reverse();
                let evs = &tr.events;
                tr.expanded.retain(|&(j, _)| evs[j].end > t - KEEP_EXPANDED && evs[j].start < t + KEEP_EXPANDED);
            }
        } else {
            let abs = t + start_time;
            for e in self.embedded.lock().iter() {
                if e.start <= abs && abs < e.end {
                    let mut e = e.clone();
                    e.start -= start_time;
                    e.end -= start_time;
                    out.push(e);
                }
            }
        }
    }
}

/// Removes ASS override blocks and converts ASS line breaks.
pub fn ass_to_plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push('\u{a0}');
                }
                _ => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// ASS event line → text: skip the first 8 comma-separated fields.
fn ass_event_text(line: &str) -> String {
    let line = line.strip_prefix("Dialogue:").unwrap_or(line);
    let mut it = line.splitn(10, ',');
    // Decoder output: ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text
    let parts: Vec<&str> = it.by_ref().collect();
    let text = if parts.len() >= 9 { parts[8..].join(",") } else { line.to_string() };
    ass_to_plain(&text)
}

/// A bitmap rectangle of a decoded subtitle, checked so its pixels can be read.
struct IndexedRect<'a> {
    r: &'a ff::AVSubtitleRect,
    w: usize,
    h: usize,
    lut: [u32; 256],
}

impl<'a> IndexedRect<'a> {
    /// `r` must be a rectangle of a live `AVSubtitle`.
    unsafe fn new(r: &'a ff::AVSubtitleRect) -> Option<Self> {
        // Guard against corrupt streams: the reads below trust these.
        if r.w <= 0
            || r.h <= 0
            || r.data[0].is_null()
            || r.data[1].is_null()
            || r.linesize[0] < r.w
            || r.w as u64 * r.h as u64 > MAX_SUB_BITMAP_PIXELS
        {
            return None;
        }
        let pal = r.data[1] as *const u32;
        // The palette is AVPALETTE_COUNT (256) entries.
        let nb_colors = r.nb_colors.clamp(0, 256) as usize;
        let mut lut = [0u32; 256];
        for (i, c) in lut.iter_mut().enumerate().take(nb_colors) {
            *c = unsafe { *pal.add(i) };
        }
        Some(Self { r, w: r.w as usize, h: r.h as usize, lut })
    }

    /// Palette indices of row `y`.
    fn row(&self, y: usize) -> &[u8] {
        debug_assert!(y < self.h);
        // Checked in `new`: data[0] holds h rows of linesize ≥ w bytes.
        unsafe { std::slice::from_raw_parts(self.r.data[0].add(y * self.r.linesize[0] as usize), self.w) }
    }

    fn to_bgra(&self, canvas: (u32, u32)) -> SubBitmap {
        let mut bgra = vec![0u8; self.w * self.h * 4];
        for (y, out_row) in bgra.chunks_exact_mut(self.w * 4).enumerate() {
            for (px, &idx) in out_row.as_chunks_mut::<4>().0.iter_mut().zip(self.row(y)) {
                *px = self.lut[idx as usize].to_le_bytes();
            }
        }
        SubBitmap {
            id: BITMAP_IDS.fetch_add(1, Ordering::Relaxed),
            x: self.r.x,
            y: self.r.y,
            w: self.w as u32,
            h: self.h as u32,
            canvas_w: canvas.0,
            canvas_h: canvas.1,
            bgra,
        }
    }

    /// `last_palette` is the previous rectangle's, reused when equal.
    fn pack(&self, canvas: (u32, u32), last_palette: &mut Option<Arc<[u32; 256]>>) -> PackedBitmap {
        let mut idx = Vec::with_capacity(self.w * self.h);
        for y in 0..self.h {
            idx.extend_from_slice(self.row(y));
        }
        let mut pixels = Vec::new();
        packbits(&idx, &mut pixels);
        let palette = match last_palette {
            Some(p) if **p == self.lut => p.clone(),
            _ => last_palette.insert(Arc::new(self.lut)).clone(),
        };
        PackedBitmap {
            x: self.r.x,
            y: self.r.y,
            w: self.w as u32,
            h: self.h as u32,
            canvas_w: canvas.0,
            canvas_h: canvas.1,
            palette,
            pixels: pixels.into_boxed_slice(),
        }
    }
}

/// `bitmap` turns each bitmap rectangle into what the caller keeps.
unsafe fn convert_subtitle<B>(
    sub: &ff::AVSubtitle,
    start: f64,
    end: f64,
    bitmap: &mut impl FnMut(&IndexedRect) -> B,
) -> Option<Cue<B>> {
    let mut texts: Vec<String> = Vec::new();
    let mut bitmaps: Vec<B> = Vec::new();
    for i in 0..sub.num_rects as usize {
        let r = unsafe { &**sub.rects.add(i) };
        match r.type_ {
            ff::AVSubtitleType::SUBTITLE_BITMAP => {
                if let Some(rect) = unsafe { IndexedRect::new(r) } {
                    bitmaps.push(bitmap(&rect));
                }
            }
            ff::AVSubtitleType::SUBTITLE_ASS => {
                if let Some(s) = unsafe { av::from_cstr(r.ass) } {
                    let t = ass_event_text(&s);
                    if !t.is_empty() {
                        texts.push(t);
                    }
                }
            }
            ff::AVSubtitleType::SUBTITLE_TEXT => {
                if let Some(s) = unsafe { av::from_cstr(r.text) } {
                    let t = s.trim().to_string();
                    if !t.is_empty() {
                        texts.push(t);
                    }
                }
            }
            _ => {}
        }
    }
    if !bitmaps.is_empty() {
        Some(Cue { start, end, content: CueContent::Bitmaps(bitmaps) })
    } else if !texts.is_empty() {
        Some(Cue { start, end, content: CueContent::Text(texts.join("\n")) })
    } else {
        None
    }
}

/// Decodes one packet; returns the event and whether it was a bitmap "clear".
unsafe fn decode_packet<B>(
    ctx: *mut ff::AVCodecContext,
    pkt: &Packet,
    tb: ff::AVRational,
    bitmap: &mut impl FnMut(&IndexedRect) -> B,
) -> (Option<Cue<B>>, Option<f64>) {
    unsafe {
        let mut sub: ff::AVSubtitle = std::mem::zeroed();
        let mut got: c_int = 0;
        let r = ff::avcodec_decode_subtitle2(ctx, &mut sub, &mut got, pkt.0);
        if r < 0 || got == 0 {
            return (None, None);
        }
        let base =
            if sub.pts != av::NOPTS { sub.pts as f64 / ff::AV_TIME_BASE as f64 } else { av::ts_to_secs(pkt.pts(), tb) };
        let base = if base.is_finite() { base } else { 0.0 };
        let start = base + sub.start_display_time as f64 / 1000.0;
        let pkt_dur = if pkt.duration() > 0 { pkt.duration() as f64 * av::q2d(tb) } else { 0.0 };
        let end = if sub.end_display_time > 0 && sub.end_display_time != u32::MAX {
            base + sub.end_display_time as f64 / 1000.0
        } else if pkt_dur > 0.0 {
            base + pkt_dur
        } else {
            f64::INFINITY
        };
        let is_bitmap_codec = (*(*ctx).codec_descriptor).props & ff::AV_CODEC_PROP_BITMAP_SUB as c_int != 0;
        let res = if sub.num_rects == 0 && is_bitmap_codec {
            (None, Some(start))
        } else {
            (convert_subtitle(&sub, start, end, bitmap), None)
        };
        ff::avsubtitle_free(&mut sub);
        res
    }
}

fn canvas_of(ctx: *mut ff::AVCodecContext, sess: &SessionCtx) -> (u32, u32) {
    let (w, h) = unsafe { ((*ctx).width, (*ctx).height) };
    if w > 0 && h > 0 {
        return (w as u32, h as u32);
    }
    let v = sess.sub_canvas.load(Ordering::Relaxed);
    let (vw, vh) = ((v >> 32) as u32, v as u32);
    if vw > 0 { (vw, vh) } else { (720, 576) }
}

pub fn run_decoder(
    ctx: Arc<SessionCtx>,
    codec: OpenedCodec,
    q: Arc<PacketQueue>,
    tb: ff::AVRational,
    stop: Arc<AtomicBool>,
) {
    let mut serial = -1;
    while ctx.alive() && !stop.load(Ordering::Acquire) {
        let Some(qp) = q.get(&stop) else { break };
        if qp.serial != serial {
            serial = qp.serial;
            ctx.subs.clear_embedded();
            unsafe { ff::avcodec_flush_buffers(codec.ctx.0) };
        }
        if qp.pkt.is_empty() {
            continue;
        }
        let canvas = canvas_of(codec.ctx.0, &ctx);
        let (cue, clear) = unsafe { decode_packet(codec.ctx.0, &qp.pkt, tb, &mut |r: &IndexedRect| r.to_bgra(canvas)) };
        if let Some(t) = clear {
            ctx.subs.end_bitmaps_at(t);
        }
        if let Some(cue) = cue {
            ctx.subs.prune_bitmaps(ctx.position() + ctx.start_time.get());
            ctx.subs.add_event(cue.into_event());
        }
    }
}

/// Loads a whole subtitle file into memory, bitmaps packed (see
/// [`PackedBitmap`]).
fn load_file(path: &Path, cfg: &EngineConfig) -> anyhow::Result<Vec<Cue<PackedBitmap>>> {
    let ic = demux::open_input(&MediaSource::File(path.to_path_buf()), cfg, None)?;
    let idx =
        unsafe { ff::av_find_best_stream(ic.0, ff::AVMediaType::AVMEDIA_TYPE_SUBTITLE, -1, -1, ptr::null_mut(), 0) };
    if idx < 0 {
        anyhow::bail!("no subtitles in {}", path.display());
    }
    let st = ic.stream(idx as usize);
    let charset = cfg.sub_charset.clone().or_else(|| detect_charset(path));
    let codec = super::decoder::open_codec(st, 1, None, charset.as_deref())?;
    let tb = unsafe { (*st).time_base };
    let canvas = {
        let (w, h) = unsafe { ((*codec.ctx.0).width, (*codec.ctx.0).height) };
        if w > 0 { (w as u32, h as u32) } else { (720, 576) }
    };
    let mut events: Vec<Cue<PackedBitmap>> = Vec::new();
    let mut palette = None;
    let mut pack = |r: &IndexedRect| r.pack(canvas, &mut palette);
    let pkt = Packet::new();
    loop {
        let r = unsafe { ff::av_read_frame(ic.0, pkt.0) };
        if r < 0 {
            break;
        }
        if pkt.stream_index() == idx {
            let (ev, clear) = unsafe { decode_packet(codec.ctx.0, &pkt, tb, &mut pack) };
            if let Some(t) = clear {
                for e in events.iter_mut().rev().take(4) {
                    if e.end > t {
                        e.end = t;
                    }
                }
            }
            if let Some(ev) = ev {
                if matches!(ev.content, CueContent::Bitmaps(_)) {
                    for e in events.iter_mut().rev().take(4) {
                        if e.end > ev.start && matches!(e.content, CueContent::Bitmaps(_)) {
                            e.end = ev.start;
                        }
                    }
                }
                events.push(ev);
            }
        }
        unsafe { ff::av_packet_unref(pkt.0) };
    }
    events.sort_by(|a, b| a.start.total_cmp(&b.start));
    for e in events.iter_mut() {
        if !e.end.is_finite() {
            e.end = e.start + 5.0;
        }
    }
    Ok(events)
}

/// Rough charset sniffing for text subtitles: UTF-8/UTF-16 are recognised by
/// FFmpeg itself; for anything else fall back to Windows-1252.
fn detect_charset(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if !matches!(ext.as_str(), "srt" | "sub" | "txt" | "ssa" | "ass" | "smi") {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) || data.starts_with(&[0xFF, 0xFE]) || data.starts_with(&[0xFE, 0xFF]) {
        return None;
    }
    if std::str::from_utf8(&data).is_ok() {
        return None;
    }
    Some("CP1252".into())
}

pub fn load_external_async(ctx: Arc<SessionCtx>, path: PathBuf, select: bool) {
    std::thread::Builder::new()
        .name("sub-load".into())
        .spawn(move || {
            let cfg = ctx.controls.config.lock().clone();
            match load_file(&path, &cfg) {
                Ok(events) => {
                    if !ctx.alive() {
                        return;
                    }
                    let idx = {
                        let mut ext = ctx.subs.external.lock();
                        let max_len = events.iter().map(|e| e.end - e.start).fold(0.0, f64::max);
                        ext.push(ExternalTrack { events, max_len, expanded: Vec::new() });
                        ext.len() - 1
                    };
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    let lang = guess_lang_from_name(&path);
                    ctx.update_state(|st| {
                        if let Some(m) = st.media.as_mut() {
                            let m = Arc::make_mut(m);
                            m.tracks.push(Track {
                                id: TrackId::External(idx),
                                kind: TrackKind::Subtitle,
                                codec: path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default(),
                                language: lang,
                                title: name,
                                default: false,
                                forced: false,
                                hearing_impaired: false,
                                width: 0,
                                height: 0,
                                fps: 0.0,
                                is_cover_art: false,
                                channels: 0,
                                channel_layout: String::new(),
                                sample_rate: 0,
                                bitrate: 0,
                                external_path: Some(path.clone()),
                            });
                        }
                    });
                    if select {
                        ctx.subs.select_external(Some(idx));
                        ctx.switch_req.lock().push((TrackKind::Subtitle, None));
                        ctx.update_state(|st| st.selected.subtitle = Some(TrackId::External(idx)));
                        ctx.wake_demuxer();
                    }
                    ctx.emit(PlayerEvent::TracksChanged);
                }
                Err(e) => ctx.emit(PlayerEvent::Warning(format!("Could not load subtitles: {e}"))),
            }
        })
        .ok();
}

fn guess_lang_from_name(p: &Path) -> Option<String> {
    let stem = p.file_stem()?.to_string_lossy().to_string();
    let last = stem.rsplit('.').next()?;
    (last.len() == 2 || last.len() == 3)
        .then(|| last.to_string())
        .filter(|l| l.chars().all(|c| c.is_ascii_alphabetic()))
}

pub const SUB_EXTS: &[&str] = &["srt", "ass", "ssa", "vtt", "sub", "idx", "smi", "sup", "ttml", "lrc"];

/// Finds `movie.srt`, `movie.en.srt`, `Subs/movie.srt` and friends.
pub fn find_sidecar_subtitles(media: &Path) -> Vec<PathBuf> {
    let Some(dir) = media.parent() else { return vec![] };
    let Some(stem) = media.file_stem().map(|s| s.to_string_lossy().to_lowercase()) else { return vec![] };
    let mut found = Vec::new();
    let mut scan = |d: &Path, any_name: bool| {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                let Some(ext) = p.extension().map(|x| x.to_string_lossy().to_lowercase()) else { continue };
                if !SUB_EXTS.contains(&ext.as_str()) || p == media {
                    continue;
                }
                let name = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
                if any_name || name == stem || name.starts_with(&format!("{stem}.")) {
                    // `.sub` next to an `.idx` is the VobSub data file, not a track.
                    if ext == "sub" && p.with_extension("idx").exists() {
                        continue;
                    }
                    found.push(p);
                }
            }
        }
    };
    scan(dir, false);
    for sub in ["Subs", "subs", "Subtitles", "subtitles"] {
        let d = dir.join(sub);
        if d.is_dir() {
            scan(&d, false);
            let d2 = d.join(media.file_stem().unwrap_or_default());
            if d2.is_dir() {
                scan(&d2, true);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(start: f64, end: f64) -> SubEvent {
        let bm = SubBitmap { id: 0, x: 0, y: 0, w: 1, h: 1, canvas_w: 1, canvas_h: 1, bgra: vec![0; 4] };
        SubEvent { start, end, content: SubContent::Bitmap(Arc::new(vec![bm])) }
    }

    fn text(start: f64, end: f64) -> SubEvent {
        SubEvent { start, end, content: SubContent::Text("x".into()) }
    }

    #[test]
    fn old_bitmaps_are_pruned_but_text_and_recent_ones_stay() {
        let store = SubStore::new();
        store.add_event(bitmap(0.0, 2.0));
        store.add_event(text(1.0, 3.0));
        store.add_event(bitmap(100.0, 102.0));
        store.add_event(bitmap(150.0, f64::INFINITY));
        store.prune_bitmaps(162.0 + KEEP_PAST);
        let left: Vec<(f64, bool)> =
            store.embedded.lock().iter().map(|e| (e.start, matches!(e.content, SubContent::Bitmap(_)))).collect();
        // Both bitmaps that ended before 162 go; the one still showing and
        // the text event stay.
        assert_eq!(left, vec![(1.0, false), (150.0, true)]);
        // An unknown clock prunes nothing.
        store.prune_bitmaps(f64::NAN);
        assert_eq!(store.embedded.lock().len(), 2);
    }

    fn packed(idx: &[u8], w: u32) -> PackedBitmap {
        let mut pixels = Vec::new();
        packbits(idx, &mut pixels);
        let mut palette = [0u32; 256];
        for (i, c) in palette.iter_mut().enumerate() {
            *c = 0x0101_0101 * i as u32;
        }
        let h = idx.len() as u32 / w;
        PackedBitmap {
            x: 0,
            y: 0,
            w,
            h,
            canvas_w: 1,
            canvas_h: 1,
            palette: Arc::new(palette),
            pixels: pixels.into_boxed_slice(),
        }
    }

    #[test]
    fn packbits_round_trips() {
        let mut cases: Vec<Vec<u8>> = vec![vec![7], vec![1, 2], vec![3, 3], vec![0; 4096]];
        for n in [127, 128, 129, 130, 131, 257, 258] {
            cases.push(vec![5; n]);
            cases.push((0..n).map(|i| i as u8).collect());
        }
        // Mixed runs and literals, as anti-aliased text gives.
        let mut x = 1u32;
        cases.push(
            (0..10_000)
                .map(|i| {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    if (i / 50) % 2 == 0 { 0 } else { (x >> 24) as u8 & 3 }
                })
                .collect(),
        );
        for idx in cases {
            let b = packed(&idx, 1).expand();
            let back: Vec<u8> = b.bgra.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
            assert_eq!(back, idx);
        }
        // Long runs cost 2 bytes per 129 pixels.
        assert_eq!(packed(&[0; 129 * 10], 129).pixels.len(), 20);
    }

    #[test]
    fn external_bitmaps_expand_only_while_near() {
        let store = SubStore::new();
        let cue = |start: f64, end: f64| Cue { start, end, content: CueContent::Bitmaps(vec![packed(&[1, 2], 2)]) };
        let events =
            vec![cue(1.0, 2.0), Cue { start: 1.5, end: 3.0, content: CueContent::Text("t".into()) }, cue(10.0, 12.0)];
        store.external.lock().push(ExternalTrack { events, max_len: 2.0, expanded: Vec::new() });
        store.select_external(Some(0));
        let mut out = Vec::new();
        store.active_at(1.6, 0.0, &mut out);
        assert_eq!(out.len(), 2);
        let SubContent::Bitmap(a) = &out[0].content else { panic!() };
        assert_eq!(a[0].bgra, [1, 1, 1, 1, 2, 2, 2, 2]);
        assert!(matches!(&out[1].content, SubContent::Text(t) if t == "t"));
        // The same picture while it shows, so the UI keeps its texture.
        let mut again = Vec::new();
        store.active_at(1.9, 0.0, &mut again);
        let SubContent::Bitmap(b) = &again[0].content else { panic!() };
        assert!(Arc::ptr_eq(a, b));
        assert_eq!(store.external.lock()[0].expanded.len(), 1);
        // Far from it, only the picture on screen stays expanded.
        out.clear();
        store.active_at(11.0, 0.0, &mut out);
        assert_eq!(out.len(), 1);
        let ext = store.external.lock();
        assert_eq!(ext[0].expanded.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![2]);
    }
}
