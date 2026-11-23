//! Audio: decode → libavfilter (effects, tempo, format conversion) → sample
//! buffer → output callback (PipeWire natively on Linux, otherwise cpal).
//! The callback drives the master clock.

#[cfg(target_os = "linux")]
mod pw_out;

use super::av::{self, Frame, ff};
use super::decoder::{DecodeResult, Decoder};
use super::filters::Graph;
use super::sync::now;
use super::types::*;
use super::{Controls, SessionCtx};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::os::raw::c_int;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct OutFormat {
    pub rate: u32,
    pub channels: u16,
    pub device: String,
}

struct Chunk {
    data: Vec<f32>,
    pos: usize,
    pts: f64,
    speed: f64,
    serial: i32,
}

struct AbInner {
    chunks: VecDeque<Chunk>,
    samples: usize,
    serial: i32,
    /// Format of the last fill (interleaved channels, rate), for sizing fades.
    fmt: (usize, f64),
    /// The first moments of what was about to play when a seek flushed the
    /// buffer, faded out before the new position starts (no click).
    tail: Vec<f32>,
    tail_pos: usize,
    /// Output stopped abruptly (seek, stall): wait for a little audio, then
    /// fade it in instead of starting at full level.
    restart: bool,
    /// Frames of fade-in still to apply.
    fade_left: usize,
}

/// Length of the fade-out of interrupted audio and of the fade-in after it.
/// Long enough to remove the click, short enough not to be heard as a fade.
const DECLICK_SECS: f64 = 0.006;
/// Audio buffered before output resumes after a seek or stall, so it doesn't
/// start, run dry and stop again in the next callback.
const PREROLL_SECS: f64 = 0.03;

/// Decoded, device-ready samples waiting for the audio callback.
pub struct AudioBuffer {
    inner: Mutex<AbInner>,
    cv: Condvar,
    abort: AtomicBool,
}

pub struct Filled {
    /// (media pts of first written sample, serial, speed)
    pub first: Option<(f64, i32, f64)>,
    /// Media pts just after the last written sample.
    pub end_pts: Option<f64>,
    /// Frames written, faded-out tail included.
    pub frames: usize,
    /// Frames of the faded-out tail before the first new sample.
    pub lead: usize,
}

impl AudioBuffer {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AbInner {
                chunks: VecDeque::new(),
                samples: 0,
                serial: 0,
                fmt: (0, 0.0),
                tail: Vec::new(),
                tail_pos: 0,
                restart: false,
                fade_left: 0,
            }),
            cv: Condvar::new(),
            abort: AtomicBool::new(false),
        }
    }

    fn push(&self, c: Chunk, max_samples: usize, stop: &AtomicBool, live_serial: impl Fn() -> i32) -> bool {
        let mut g = self.inner.lock();
        while g.samples >= max_samples {
            if self.abort.load(Ordering::Acquire) || stop.load(Ordering::Acquire) || live_serial() != c.serial {
                return false;
            }
            self.cv.wait_for(&mut g, Duration::from_millis(20));
        }
        if c.serial != g.serial {
            return true; // stale, silently dropped
        }
        g.samples += c.data.len();
        g.chunks.push_back(c);
        true
    }

    pub fn flush(&self, serial: i32) {
        let mut g = self.inner.lock();
        // Keep what would have played next, to fade it out (unless a fade-out
        // from an earlier flush is still playing: continue that one).
        let (ch, rate) = g.fmt;
        let want = (rate * DECLICK_SECS) as usize * ch;
        if want > 0 && g.tail_pos >= g.tail.len() {
            let mut tail = std::mem::take(&mut g.tail);
            tail.clear();
            for c in g.chunks.iter().filter(|c| c.serial == g.serial) {
                let n = (c.data.len() - c.pos).min(want - tail.len());
                tail.extend_from_slice(&c.data[c.pos..c.pos + n]);
                if tail.len() >= want {
                    break;
                }
            }
            tail.truncate(tail.len() / ch * ch);
            g.tail = tail;
            g.tail_pos = 0;
        }
        g.chunks.clear();
        g.samples = 0;
        g.serial = serial;
        g.restart = true;
        g.fade_left = 0;
        drop(g);
        self.cv.notify_all();
    }

    /// Drops a pending fade-out tail: output was silent (paused, muted), so
    /// there's nothing to fade, and the tail is from before a seek.
    fn drop_tail(&self) {
        let mut g = self.inner.lock();
        g.tail.clear();
        g.tail_pos = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().samples == 0
    }

    pub fn abort(&self) {
        self.abort.store(true, Ordering::Release);
        self.cv.notify_all();
    }

    /// Fills `out` from the buffer, zero-padding what's missing. Interrupted
    /// audio is faded out and resumed audio faded in, so seeks and stalls
    /// don't click. `finished`: the decoder has delivered everything for
    /// this serial (no stall to wait out, and a running-dry end is the
    /// real end, which gapless playback continues seamlessly).
    fn fill(&self, out: &mut [f32], channels: usize, rate: f64, finished: bool) -> Filled {
        let ch = channels.max(1);
        let fade = ((rate * DECLICK_SECS) as usize).max(1);
        let mut res = Filled { first: None, end_pts: None, frames: 0, lead: 0 };
        let mut g = self.inner.lock();
        g.fmt = (ch, rate);
        let serial = g.serial;
        let mut written = 0;

        // 1. Fade out the remains of the audio a seek interrupted.
        if g.tail_pos < g.tail.len() {
            let total = (g.tail.len() / ch).max(1);
            let n = (g.tail.len() - g.tail_pos).min(out.len() / ch * ch);
            for i in 0..n {
                let frame = (g.tail_pos + i) / ch;
                let gain = 1.0 - (frame + 1) as f32 / total as f32;
                out[i] = g.tail[g.tail_pos + i] * gain;
            }
            g.tail_pos += n;
            written = n;
            if g.tail_pos >= g.tail.len() {
                g.tail.clear();
                g.tail_pos = 0;
            }
        }
        res.lead = written / ch;

        // 2. After an interruption, wait until enough is buffered.
        while g.chunks.front().is_some_and(|c| c.serial != serial) {
            let c = g.chunks.pop_front().unwrap();
            g.samples -= c.data.len() - c.pos;
        }
        let mut hold = false;
        if g.restart {
            if g.samples >= (rate * PREROLL_SECS) as usize * ch || (finished && g.samples > 0) {
                g.restart = false;
                g.fade_left = fade;
            } else {
                hold = true;
            }
        }

        // 3. The audio itself.
        let data_start = written;
        while !hold && written < out.len() {
            let Some(c) = g.chunks.front_mut() else { break };
            if c.serial != serial {
                let n = c.data.len() - c.pos;
                g.samples -= n;
                g.chunks.pop_front();
                continue;
            }
            if res.first.is_none() {
                let off = (c.pos / ch) as f64 / rate * c.speed;
                res.first = Some((c.pts + off, c.serial, c.speed));
            }
            let n = (c.data.len() - c.pos).min(out.len() - written);
            out[written..written + n].copy_from_slice(&c.data[c.pos..c.pos + n]);
            c.pos += n;
            written += n;
            res.end_pts = Some(c.pts + (c.pos / ch) as f64 / rate * c.speed);
            if c.pos >= c.data.len() {
                g.chunks.pop_front();
            }
            g.samples -= n;
        }
        // Fade in after an interruption.
        if g.fade_left > 0 {
            for frame in out[data_start..written].chunks_exact_mut(ch) {
                if g.fade_left == 0 {
                    break;
                }
                let gain = 1.0 - g.fade_left as f32 / fade as f32;
                frame.iter_mut().for_each(|s| *s *= gain);
                g.fade_left -= 1;
            }
        }
        // Ran dry mid-stream (a stall): fade out what was written and fade
        // in once audio returns.
        if !hold && written < out.len() && written > data_start && !finished {
            let frames = (written - data_start) / ch;
            let n = fade.min(frames);
            let end = data_start + frames * ch;
            for (i, frame) in out[end - n * ch..end].chunks_exact_mut(ch).enumerate() {
                let gain = 1.0 - (i + 1) as f32 / n as f32;
                frame.iter_mut().for_each(|s| *s *= gain);
            }
            g.restart = true;
            g.fade_left = 0;
        }
        drop(g);
        self.cv.notify_all();
        out[written..].fill(0.0);
        res.frames = written / ch;
        res
    }
}
fn find_device(host: &cpal::Host, name: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = name {
        if let Ok(mut devs) = host.output_devices()
            && let Some(d) = devs.find(|d| d.description().map(|x| x.name() == name).unwrap_or(false))
        {
            return Some(d);
        }
        log::warn!("audio device '{name}' not found, using default");
    }
    host.default_output_device()
}

pub fn list_devices() -> Vec<String> {
    #[cfg(target_os = "linux")]
    if let Some(v) = pw_out::list_devices() {
        return v;
    }
    let host = cpal::default_host();
    let mut v: Vec<String> = host
        .output_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|x| x.name().to_string())).collect())
        .unwrap_or_default();
    v.dedup();
    v
}

fn wanted_channels(mode: ChannelMode, media: i32) -> Option<u16> {
    match mode {
        ChannelMode::Auto => (media > 2).then_some(media.min(8) as u16),
        ChannelMode::Stereo => Some(2),
        ChannelMode::Mono => Some(1),
        ChannelMode::Surround51 => Some(6),
        ChannelMode::Surround71 => Some(8),
    }
}

/// A session the output plays from.
#[derive(Clone)]
struct Feed {
    ctx: Arc<SessionCtx>,
    stop: Arc<AtomicBool>,
    /// The `audio_pending` token to clear when this feed starts playing.
    token: u64,
}

impl Feed {
    fn active(&self) -> bool {
        self.ctx.alive() && !self.stop.load(Ordering::Acquire)
    }
}

struct Feeds {
    cur: Feed,
    /// A preloaded next item, played as soon as `cur` runs out.
    next: Option<Feed>,
    /// The output stopped; nothing may be queued on it any more.
    closed: bool,
}

/// What an open output device plays. It outlives sessions: when the next
/// playlist item was preloaded, the device carries straight on with it.
pub struct OutputLink {
    feeds: Mutex<Feeds>,
    controls: Arc<Controls>,
    /// Device and channel request it was opened for; a session wanting
    /// something else opens its own.
    key: (Option<String>, Option<u16>),
    fmt: Mutex<Option<OutFormat>>,
}

impl OutputLink {
    fn new(cur: Feed, cfg: &EngineConfig, media_channels: i32) -> Arc<Self> {
        Arc::new(Self {
            controls: cur.ctx.controls.clone(),
            feeds: Mutex::new(Feeds { cur, next: None, closed: false }),
            key: (cfg.audio_device.clone(), wanted_channels(cfg.channels, media_channels)),
            fmt: Mutex::new(None),
        })
    }

    fn snapshot(&self) -> (Feed, Option<Feed>) {
        let f = self.feeds.lock();
        (f.cur.clone(), f.next.clone())
    }

    fn current(&self) -> Arc<SessionCtx> {
        self.feeds.lock().cur.ctx.clone()
    }

    fn paused(&self) -> bool {
        self.controls.paused.load(Ordering::Acquire)
    }

    /// Called regularly by whichever thread drives the device: drops a dead
    /// next feed, moves on to a live one when the current session ended,
    /// and says whether there is anything left to play. Once it returns
    /// false the output is closed for good.
    fn keep_serving(&self) -> bool {
        let mut f = self.feeds.lock();
        if f.closed {
            return false;
        }
        if f.next.as_ref().is_some_and(|n| !n.active()) {
            f.next = None;
        }
        if f.cur.active() {
            return true;
        }
        if let Some(n) = f.next.take() {
            n.ctx.release_audio_pending(n.token);
            f.cur = n;
            return true;
        }
        f.closed = true;
        false
    }

    /// Switches to `next` if it is still the queued feed.
    fn switch_to(&self, next: &Feed) -> bool {
        let mut f = self.feeds.lock();
        if f.closed || f.next.as_ref().is_none_or(|n| n.ctx.id != next.ctx.id) {
            return false;
        }
        f.cur = f.next.take().unwrap();
        true
    }

    /// Waits until the output no longer plays session `id` (its audio
    /// stopped). Returns true if the output closed rather than moving on.
    fn release(&self, id: u64) -> bool {
        loop {
            {
                let f = self.feeds.lock();
                if f.closed {
                    return true;
                }
                if f.cur.ctx.id != id && f.next.as_ref().is_none_or(|n| n.ctx.id != id) {
                    return false;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// For a preloaded session: queues its audio on the output that is playing
/// the session it follows, so it starts on the very sample where that one
/// ends. `None` if there is no such output or it was opened differently.
pub fn adopt_output(
    ctx: &Arc<SessionCtx>,
    stop: &Arc<AtomicBool>,
    cfg: &EngineConfig,
    media_channels: i32,
    token: u64,
) -> Option<(Arc<OutputLink>, OutFormat)> {
    if ctx.follows == 0 {
        return None;
    }
    let link = ctx.controls.output.lock().upgrade()?;
    if link.key != (cfg.audio_device.clone(), wanted_channels(cfg.channels, media_channels)) {
        return None;
    }
    let fmt = link.fmt.lock().clone()?;
    let mut f = link.feeds.lock();
    if f.closed || f.cur.ctx.id != ctx.follows || !f.cur.active() {
        return None;
    }
    f.next = Some(Feed { ctx: ctx.clone(), stop: stop.clone(), token });
    drop(f);
    Some((link, fmt))
}

/// Everything of `ctx`'s audio has been heard (and its video decoded), so a
/// successor can start.
fn drained(ctx: &SessionCtx) -> bool {
    ctx.eof.load(Ordering::Acquire)
        && ctx.audio_finished.load(Ordering::Acquire) == ctx.aq.serial()
        && ctx.abuf.is_empty()
        && (!ctx.video_active.load(Ordering::Acquire) || ctx.video_finished.load(Ordering::Acquire) == ctx.vq.serial())
}

struct CallbackState {
    link: Arc<OutputLink>,
    channels: usize,
    rate: f64,
    gain: f32,
    scratch: Vec<f32>,
    /// (session id, media pts after the last written sample, serial, speed)
    last_end: Option<(u64, f64, i32, f64)>,
}

impl CallbackState {
    fn run(&mut self, out: &mut [f32], latency: f64) {
        let (cur, next) = self.link.snapshot();
        let ctx = &cur.ctx;
        // Nothing was audible: a fade-out of pre-seek audio would only be heard now.
        if self.gain == 0.0 {
            ctx.abuf.drop_tail();
        }
        if !cur.active() || ctx.paused() || ctx.is_standby() {
            out.fill(0.0);
            if self.gain > 0.0 && cur.active() && !ctx.is_standby() {
                // Pausing: fade out over the next few milliseconds rather than cut off.
                let n = ((self.rate * DECLICK_SECS) as usize).max(1).min(out.len() / self.channels.max(1));
                let part = &mut out[..n * self.channels];
                ctx.abuf.fill(part, self.channels, self.rate, true);
                ramp(part, self.channels, self.gain, 0.0);
            }
            self.gain = 0.0;
            return;
        }
        let finished = |c: &SessionCtx| c.audio_finished.load(Ordering::Acquire) == c.aq.serial();
        let r = ctx.abuf.fill(out, self.channels, self.rate, finished(ctx));
        let t = now();
        let written = r.frames * self.channels;
        if written < out.len()
            && let Some(n) = next
            && n.active()
            && !n.ctx.abuf.is_empty()
            && drained(ctx)
            && self.link.switch_to(&n)
        {
            // Gapless: the next item starts right after the last sample of
            // this one, in this very buffer.
            n.ctx.release_audio_pending(n.token);
            let r2 = n.ctx.abuf.fill(&mut out[written..], self.channels, self.rate, finished(&n.ctx));
            let lead = (r.frames + r2.lead) as f64 / self.rate;
            if let Some((pts, serial, speed)) = r2.first {
                n.ctx.aclk.set_at(pts - (latency + lead) * speed, serial, t);
                if let Some(e) = r2.end_pts {
                    self.last_end = Some((n.ctx.id, e, serial, speed));
                }
            }
            n.ctx.audio_dry_since.set(f64::NAN);
            n.ctx.promote();
        } else if let Some((pts, serial, speed)) = r.first {
            ctx.audio_dry_since.set(f64::NAN);
            ctx.aclk.set_at(pts - (latency + r.lead as f64 / self.rate) * speed, serial, t);
            if let Some(e) = r.end_pts {
                self.last_end = Some((ctx.id, e, serial, speed));
            }
        } else {
            // Starved (buffering): hold the clock at the last audible sample.
            if let Some((id, e, serial, speed)) = self.last_end
                && id == ctx.id
                && serial == ctx.aq.serial()
            {
                ctx.aclk.set_at(e - latency * speed, serial, t);
            }
            // Nothing on its way either: the track has a gap or has ended,
            // and video may take over the clock (`audio_is_master`).
            if ctx.aq.len() == 0 && !ctx.audio_dry_since.get().is_finite() {
                ctx.audio_dry_since.set(t);
            }
        }
        // Volume: cubic curve like most players, ramped per buffer to avoid
        // clicks. Silent while the progress bar is being dragged.
        let v = ctx.controls.volume.get() as f32;
        let silent = ctx.controls.muted.load(Ordering::Relaxed) || ctx.controls.hushed.load(Ordering::Relaxed);
        let target = if silent { 0.0 } else { v * v * v };
        ramp(out, self.channels, self.gain, target);
        self.gain = target;
    }
}

/// Scales `out` by a gain moving linearly from `from` to `to` across it.
fn ramp(out: &mut [f32], channels: usize, from: f32, to: f32) {
    let ch = channels.max(1);
    let frames = (out.len() / ch).max(1) as f32;
    for (i, frame) in out.chunks_exact_mut(ch).enumerate() {
        let g = from + (to - from) * (i as f32 + 1.0) / frames;
        for s in frame {
            *s = (*s * g).clamp(-1.0, 1.0);
        }
    }
}

/// How long playback waits for a device before continuing without sound.
const DEVICE_OPEN_TIMEOUT: Duration = Duration::from_secs(3);

/// Device opens that timed out and still wait for the driver. A new open
/// would most likely hang the same way (ALSA holds a global lock while its
/// PipeWire plugin waits for a server that never answers), and each would
/// leave one more thread behind and hold playback 3 s: while one is stuck,
/// play silently at once instead.
static STUCK_OPENS: AtomicIsize = AtomicIsize::new(0);

/// Opens the audio device on its own thread (cpal streams may not be `Send`).
pub fn open_output(
    ctx: Arc<SessionCtx>,
    stop: Arc<AtomicBool>,
    cfg: &EngineConfig,
    media_channels: i32,
) -> anyhow::Result<(JoinHandle<()>, OutFormat, Arc<OutputLink>)> {
    let (tx, rx) = crossbeam_channel::bounded::<OutFormat>(1);
    let device_name = cfg.audio_device.clone();
    let mode = cfg.channels;
    let link = OutputLink::new(Feed { ctx: ctx.clone(), stop, token: 0 }, cfg, media_channels);
    if STUCK_OPENS.load(Ordering::Acquire) > 0 {
        log::warn!("an earlier audio device open still hangs; playing without sound");
        ctx.emit(PlayerEvent::Warning("The audio device isn't responding — playing without sound".into()));
        let fmt = silent_format(mode, media_channels);
        let (f2, l2) = (fmt.clone(), link.clone());
        let sink = std::thread::Builder::new().name("audio-null".into()).spawn(move || null_sink(&l2, f2))?;
        *link.fmt.lock() = Some(fmt.clone());
        *ctx.controls.output.lock() = Arc::downgrade(&link);
        return Ok((sink, fmt, link));
    }
    // Whoever sets this first owns the output: the device thread when it
    // opens in time, or the fallback below when it doesn't.
    let claimed = Arc::new(AtomicBool::new(false));
    let (l, cl) = (link.clone(), claimed.clone());
    let handle = std::thread::Builder::new().name("audio-out".into()).spawn(move || {
        let link = l;
        // Called once the open returns: whether it came too late (the
        // fallback took over and counted this open as stuck).
        let late = || {
            let late = cl.swap(true, Ordering::AcqRel);
            if late {
                STUCK_OPENS.fetch_sub(1, Ordering::AcqRel);
            }
            late
        };
        #[cfg(target_os = "linux")]
        {
            let ready = |fmt: &OutFormat| {
                if late() {
                    log::warn!("audio device opened after the timeout; not using it for this file");
                    return false;
                }
                let _ = tx.send(fmt.clone());
                true
            };
            match pw_out::run(&link, device_name.as_deref(), mode, media_channels, ready) {
                Ok(pw_out::Outcome::Stopped) => return,
                Ok(pw_out::Outcome::Failed(fmt, why)) => {
                    log::warn!("{why}; continuing silently");
                    link.current().emit(PlayerEvent::Warning(
                        "The audio device stopped responding; playing without sound".into(),
                    ));
                    null_sink(&link, fmt);
                    return;
                }
                // No PipeWire server: fall through to ALSA.
                Err(e) => log::info!("PipeWire unavailable ({e:#}); using ALSA"),
            }
        }
        let started = start_device(&link, device_name.as_deref(), mode, media_channels);
        if late() {
            // Took too long; playback already continued without this device.
            log::warn!("audio device opened after the timeout; not using it for this file");
            return;
        }
        match started {
            Ok((stream, fmt, callbacks)) => {
                let _ = tx.send(fmt.clone());
                // Watchdog: a device that opens but never asks for samples
                // (stuck sound server, suspended sink) would freeze the master
                // clock. Fall back to the silent sink if that happens.
                let mut seen = callbacks.load(Ordering::Relaxed);
                let mut last_progress = std::time::Instant::now();
                while link.keep_serving() {
                    std::thread::sleep(Duration::from_millis(30));
                    let c = callbacks.load(Ordering::Relaxed);
                    if c != seen || link.paused() {
                        seen = c;
                        last_progress = std::time::Instant::now();
                    } else if last_progress.elapsed() > Duration::from_secs(2) {
                        log::warn!("audio device stopped requesting samples; continuing silently");
                        link.current().emit(PlayerEvent::Warning(
                            "The audio device stopped responding; playing without sound".into(),
                        ));
                        drop(stream);
                        null_sink(&link, fmt);
                        return;
                    }
                }
                drop(stream);
            }
            Err(e) => {
                // No usable device: keep playing in real time without sound.
                log::warn!("audio output unavailable: {e:#}");
                link.current().emit(PlayerEvent::Warning("No audio device available — playing without sound".into()));
                let fmt = silent_format(mode, media_channels);
                let _ = tx.send(fmt.clone());
                null_sink(&link, fmt);
            }
        }
    })?;
    let opened = match rx.recv_timeout(DEVICE_OPEN_TIMEOUT) {
        Ok(fmt) => (handle, fmt),
        Err(_) if !claimed.swap(true, Ordering::AcqRel) => {
            // The device is hanging. Leave its thread behind (it exits on its
            // own if the device ever answers) and play silently instead.
            STUCK_OPENS.fetch_add(1, Ordering::AcqRel);
            log::warn!("audio device did not open within {DEVICE_OPEN_TIMEOUT:?}; playing without sound");
            ctx.emit(PlayerEvent::Warning("The audio device isn't responding — playing without sound".into()));
            let fmt = silent_format(mode, media_channels);
            let (f2, l2) = (fmt.clone(), link.clone());
            let sink = std::thread::Builder::new().name("audio-null".into()).spawn(move || null_sink(&l2, f2))?;
            (sink, fmt)
        }
        // The device won the race just as we timed out; its format is on the way.
        Err(_) => match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(fmt) => (handle, fmt),
            Err(_) => anyhow::bail!("audio device did not respond"),
        },
    };
    *link.fmt.lock() = Some(opened.1.clone());
    *ctx.controls.output.lock() = Arc::downgrade(&link);
    Ok((opened.0, opened.1, link))
}

/// After a session's audio decoder stopped: waits until the output has let
/// go of it, joining the device thread if this was the last user.
pub fn finish_output(link: &OutputLink, id: u64, thread: Option<JoinHandle<()>>) {
    if link.release(id)
        && let Some(t) = thread
    {
        let _ = t.join();
    }
}

fn silent_format(mode: ChannelMode, media_channels: i32) -> OutFormat {
    OutFormat {
        rate: 48000,
        channels: wanted_channels(mode, media_channels).unwrap_or(2),
        device: "None (silent)".into(),
    }
}

fn callback_state(link: &Arc<OutputLink>, fmt: &OutFormat) -> CallbackState {
    CallbackState {
        link: link.clone(),
        channels: fmt.channels as usize,
        rate: fmt.rate as f64,
        gain: 0.0,
        scratch: Vec::new(),
        last_end: None,
    }
}

/// Consumes audio in real time and discards it, so the audio clock keeps
/// driving playback when there is no working output device.
fn null_sink(link: &Arc<OutputLink>, fmt: OutFormat) {
    const PERIOD: Duration = Duration::from_millis(20);
    let mut state = callback_state(link, &fmt);
    let mut buf = vec![0f32; (fmt.rate as usize / 50).max(1) * fmt.channels as usize];
    let mut next = std::time::Instant::now();
    while link.keep_serving() {
        state.run(&mut buf, 0.0);
        next += PERIOD;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > Duration::from_millis(200) {
            next = now;
        }
    }
}

type Started = (cpal::Stream, OutFormat, Arc<std::sync::atomic::AtomicU64>);

fn start_device(
    link: &Arc<OutputLink>,
    device_name: Option<&str>,
    mode: ChannelMode,
    media_channels: i32,
) -> anyhow::Result<Started> {
    // Debug builds only: simulate a slow or hanging sound server.
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("MAVPLAYER_TEST_AUDIO_DELAY_MS").ok().and_then(|v| v.parse().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
    }
    let host = cpal::default_host();
    let device = find_device(&host, device_name).ok_or_else(|| anyhow::anyhow!("no audio output device"))?;
    let dev_label = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "default".into());
    let def = device.default_output_config()?;
    let rate = def.sample_rate();
    let mut channels = def.channels();
    if let Some(want) = wanted_channels(mode, media_channels) {
        let ok = device
            .supported_output_configs()
            .map(|mut it| {
                it.any(|c| c.channels() == want && c.min_sample_rate() <= rate && c.max_sample_rate() >= rate)
            })
            .unwrap_or(false);
        if ok || want <= 2 {
            channels = want;
        }
    }
    let fmt = OutFormat { rate, channels, device: dev_label };
    let config = cpal::StreamConfig { channels, sample_rate: rate, buffer_size: cpal::BufferSize::Default };
    let mut state = callback_state(link, &fmt);
    let callbacks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let err_link = link.clone();
    let on_err = move |e: cpal::Error| {
        log::warn!("audio stream error: {e}");
        err_link.current().emit(PlayerEvent::Warning(format!("Audio device error: {e}")));
    };
    let latency_of = |info: &cpal::OutputCallbackInfo| {
        let ts = info.timestamp();
        ts.playback.duration_since(ts.callback).as_secs_f64().min(0.5)
    };
    let cb = callbacks.clone();
    let stream = match def.sample_format() {
        cpal::SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config,
            move |out, info| {
                cb.fetch_add(1, Ordering::Relaxed);
                let lat = latency_of(info);
                let mut scratch = std::mem::take(&mut state.scratch);
                scratch.resize(out.len(), 0.0);
                state.run(&mut scratch, lat);
                for (o, s) in out.iter_mut().zip(scratch.iter()) {
                    *o = (s * 32767.0) as i16;
                }
                state.scratch = scratch;
            },
            on_err,
            None,
        )?,
        _ => device.build_output_stream::<f32, _, _>(
            config,
            move |out, info| {
                cb.fetch_add(1, Ordering::Relaxed);
                let lat = latency_of(info);
                state.run(out, lat);
            },
            on_err,
            None,
        )?,
    };
    stream.play()?;
    Ok((stream, fmt, callbacks))
}

fn atempo_chain(speed: f64) -> Vec<String> {
    if (speed - 1.0).abs() < 1e-3 {
        return vec![];
    }
    let mut v = Vec::new();
    let mut s = speed;
    while s < 0.5 {
        v.push("atempo=0.5".to_string());
        s /= 0.5;
    }
    v.push(format!("atempo={s:.4}"));
    v
}

fn fx_chain(fx: &AudioFx, in_channels: i32) -> Vec<String> {
    let mut v = Vec::new();
    let stereo = in_channels == 2;
    if fx.swap_channels && stereo {
        v.push("channelmap=map=1|0".into());
    }
    if fx.karaoke && stereo {
        v.push("pan=stereo|c0=c0-c1|c1=c1-c0".into());
    }
    if fx.stereo_widen && stereo {
        v.push("extrastereo=m=1.8".into());
    }
    if fx.eq_enabled {
        if fx.preamp_db.abs() > 0.05 {
            v.push(format!("volume={:.2}dB", fx.preamp_db));
        }
        for (f, g) in EQ_BANDS.iter().zip(fx.eq_db.iter()) {
            if g.abs() > 0.05 {
                v.push(format!("equalizer=f={f}:t=o:w=1:g={g:.2}"));
            }
        }
    }
    if fx.night_mode {
        v.push("acompressor=threshold=-24dB:ratio=6:attack=10:release=200:makeup=8dB".into());
    }
    if fx.normalize {
        v.push("dynaudnorm=f=200:g=11:p=0.9:m=8".into());
    }
    v
}

fn layout_string(l: &ff::AVChannelLayout) -> String {
    if l.order == ff::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC || l.nb_channels <= 0 {
        let mut d: ff::AVChannelLayout = unsafe { std::mem::zeroed() };
        unsafe { ff::av_channel_layout_default(&mut d, l.nb_channels.max(1)) };
        let s = av::channel_layout_name(&d);
        unsafe { ff::av_channel_layout_uninit(&mut d) };
        s
    } else {
        av::channel_layout_name(l)
    }
}

fn out_layout_string(channels: u16) -> String {
    let mut d: ff::AVChannelLayout = unsafe { std::mem::zeroed() };
    unsafe { ff::av_channel_layout_default(&mut d, channels as c_int) };
    let s = av::channel_layout_name(&d);
    unsafe { ff::av_channel_layout_uninit(&mut d) };
    s
}

#[derive(PartialEq)]
struct Key {
    rate: i32,
    fmt: c_int,
    layout: String,
    fx_gen: u64,
    speed: u64,
    /// Seeks start a new graph: filters with memory (tempo, normaliser,
    /// compressor) would otherwise play pre-seek audio under the new
    /// position, and atempo would keep counting time from the old one.
    serial: i32,
}

pub fn run_decoder(ctx: Arc<SessionCtx>, mut dec: Decoder, tb: ff::AVRational, out: OutFormat, stop: Arc<AtomicBool>) {
    let mut frame = Frame::new();
    let mut filtered = Frame::new();
    let mut graph: Option<(Key, Graph, f64)> = None;
    let mut next_pts = f64::NAN;
    let out_layout = out_layout_string(out.channels);
    let max_samples = (out.rate as usize * out.channels as usize) / 4; // ~250 ms
    let aq = ctx.aq.clone();

    while ctx.alive() && !stop.load(Ordering::Acquire) {
        let res = dec.decode(&mut frame);
        let serial = dec.pkt_serial;
        match res {
            DecodeResult::Aborted => break,
            DecodeResult::Eof => {
                // Drain samples held by the filters (tempo, normaliser).
                if let Some((_, g, start)) = graph.as_mut() {
                    g.push_eof();
                    let start = *start;
                    let speed = ctx.controls.speed();
                    drain(&ctx, g, &mut filtered, start, speed, serial, out.channels, max_samples, &stop, &aq);
                }
                graph = None;
                ctx.audio_finished.store(dec.finished, Ordering::Release);
                ctx.wake_demuxer();
                continue;
            }
            DecodeResult::Frame => {}
            DecodeResult::Restart => continue, // only video decoders use the GPU
        }
        let rate = frame.raw().sample_rate.max(1);
        let n = frame.nb_samples();
        let mut pts = av::ts_to_secs(frame.raw().pts, tb);
        if !pts.is_finite() {
            pts = next_pts;
        }
        let dur = n as f64 / rate as f64;
        next_pts = if pts.is_finite() { pts + dur } else { f64::NAN };

        if let Some((target, s)) = *ctx.seek_target.lock()
            && s == serial
            && pts.is_finite()
            && pts + dur < target
        {
            frame.unref();
            continue;
        }

        let speed = ctx.controls.speed();
        let (fx_gen, fx) = ctx.controls.audio_fx.lock().clone();
        let layout = layout_string(&frame.raw().ch_layout);
        let key = Key { rate, fmt: frame.format(), layout: layout.clone(), fx_gen, speed: speed.to_bits(), serial };
        if graph.as_ref().map(|(k, _, _)| k != &key).unwrap_or(true) {
            // Flush the old graph's tail so speed/effect changes don't drop
            // audio. After a seek its tail is from the old position: drop it.
            if let Some((_, g, start)) = graph.as_mut().filter(|(k, _, _)| k.serial == serial) {
                g.push_eof();
                let (start, old_speed) = (*start, f64::from_bits(graph.as_ref().unwrap().0.speed));
                let g = &mut graph.as_mut().unwrap().1;
                drain(&ctx, g, &mut filtered, start, old_speed, serial, out.channels, max_samples, &stop, &aq);
            }
            let mut chain = fx_chain(&fx, frame.raw().ch_layout.nb_channels);
            chain.extend(atempo_chain(speed));
            chain.push(format!("aformat=sample_fmts=flt:sample_rates={}:channel_layouts={}", out.rate, out_layout));
            let args = format!(
                "time_base=1/{rate}:sample_rate={rate}:sample_fmt={}:channel_layout={}",
                av::sample_fmt_name(frame.format()),
                layout
            );
            match Graph::new("abuffer", &args, "abuffersink", &chain.join(","), 1) {
                Ok(g) => graph = Some((key, g, if pts.is_finite() { pts } else { 0.0 })),
                Err(e) => {
                    log::warn!("audio filters failed ({e}); falling back to plain conversion");
                    let chain =
                        format!("aformat=sample_fmts=flt:sample_rates={}:channel_layouts={}", out.rate, out_layout);
                    match Graph::new("abuffer", &args, "abuffersink", &chain, 1) {
                        Ok(g) => graph = Some((key, g, if pts.is_finite() { pts } else { 0.0 })),
                        Err(e) => {
                            log::error!("audio conversion failed: {e}");
                            frame.unref();
                            continue;
                        }
                    }
                }
            }
        }
        let (_, g, start) = graph.as_mut().unwrap();
        let start = *start;
        unsafe {
            (*frame.0).pts = if pts.is_finite() { (pts * rate as f64).round() as i64 } else { av::NOPTS };
        }
        if let Err(e) = g.push(&mut frame) {
            log::debug!("{e}");
        }
        frame.unref();
        drain(&ctx, g, &mut filtered, start, speed, serial, out.channels, max_samples, &stop, &aq);
    }
}

#[allow(clippy::too_many_arguments)]
fn drain(
    ctx: &SessionCtx,
    g: &mut Graph,
    filtered: &mut Frame,
    start: f64,
    speed: f64,
    serial: i32,
    channels: u16,
    max_samples: usize,
    stop: &AtomicBool,
    aq: &super::sync::PacketQueue,
) {
    let otb = g.out_time_base();
    while g.pull(filtered) {
        // The graph ends in `aformat` for packed float in the output layout;
        // anything else would make the read below run past the buffer.
        let raw = filtered.raw();
        if raw.ch_layout.nb_channels != channels as c_int
            || raw.format != ff::AVSampleFormat::AV_SAMPLE_FMT_FLT as c_int
            || raw.data[0].is_null()
            || raw.nb_samples <= 0
        {
            filtered.unref();
            continue;
        }
        let n = filtered.nb_samples() as usize * channels as usize;
        let out_pts = av::ts_to_secs(filtered.raw().pts, otb);
        // atempo stamps output on its own timeline; map back to media time.
        let media_pts = if out_pts.is_finite() { start + (out_pts - start) * speed } else { start };
        let data = unsafe { std::slice::from_raw_parts((*filtered.0).data[0] as *const f32, n) }.to_vec();
        filtered.unref();
        let chunk = Chunk { data, pos: 0, pts: media_pts, speed, serial };
        if !ctx.abuf.push(chunk, max_samples, stop, || aq.serial()) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48000.0;

    fn push(b: &AudioBuffer, value: f32, frames: usize, serial: i32) {
        let c = Chunk { data: vec![value; frames * 2], pos: 0, pts: 0.0, speed: 1.0, serial };
        assert!(b.push(c, usize::MAX, &AtomicBool::new(false), || serial));
    }

    /// Plays `n` callbacks of 256 frames and returns the left channel.
    fn play(b: &AudioBuffer, n: usize, finished: bool, out: &mut Vec<f32>) {
        for _ in 0..n {
            let mut buf = vec![0.0f32; 512];
            b.fill(&mut buf, 2, RATE, finished);
            out.extend(buf.iter().step_by(2));
        }
    }

    fn largest_step(s: &[f32]) -> f32 {
        s.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn seeks_fade_instead_of_clicking() {
        let b = AudioBuffer::new();
        push(&b, 0.8, 4800, 0);
        let mut out = Vec::new();
        play(&b, 4, false, &mut out);
        assert_eq!(*out.last().unwrap(), 0.8);
        b.flush(1);
        push(&b, -0.8, 4800, 1);
        play(&b, 12, false, &mut out);
        // Full level on both sides, but no jump anywhere: a 1.6 step became a ramp.
        assert_eq!(*out.last().unwrap(), -0.8);
        assert!(largest_step(&out) < 0.01, "step {}", largest_step(&out));
    }

    #[test]
    fn stalls_fade_out_and_back_in() {
        let b = AudioBuffer::new();
        b.flush(0); // as after a seek: starts with a fade-in
        push(&b, 0.8, 2000, 0);
        let mut out = Vec::new();
        play(&b, 10, false, &mut out); // runs dry in the 8th callback
        assert!(out[out.len() - 256..].iter().all(|&s| s == 0.0));
        // Not enough for the pre-roll yet: stays silent rather than starting and stopping.
        push(&b, 0.8, 100, 0);
        play(&b, 2, false, &mut out);
        assert!(out[out.len() - 256..].iter().all(|&s| s == 0.0));
        push(&b, 0.8, 4800, 0);
        play(&b, 10, false, &mut out);
        assert_eq!(*out.last().unwrap(), 0.8);
        assert!(largest_step(&out) < 0.01, "step {}", largest_step(&out));
    }

    #[test]
    fn the_real_end_is_not_faded() {
        // Gapless playback carries straight on from the last sample.
        let b = AudioBuffer::new();
        push(&b, 0.8, 300, 0);
        let mut buf = vec![0.0f32; 1024];
        b.fill(&mut buf, 2, RATE, true);
        assert_eq!(buf[599], 0.8);
        assert_eq!(buf[600], 0.0);
    }
}
