//! MavPlayer's playback engine.
//!
//! Built directly on FFmpeg's libraries (demuxing, decoding, filtering,
//! scaling, resampling) plus `cpal` for audio output. Thread layout per
//! opened file ("session"):
//!
//! ```text
//!   demux ──► video packets ──► video decode ─► filter ─► frame queue ─► presenter ─► UI
//!         ├─► audio packets ──► audio decode ─► filter ─► sample buffer ─► audio device (master clock)
//!         └─► subtitle packets ─► subtitle decode ─► event store ─► UI overlay
//! ```
//!
//! Gapless playback: near the end of a file, the item queued with
//! [`Player::set_next`] is opened as a second, *standby* session that
//! buffers but shows and plays nothing. The audio output (which outlives
//! sessions, see `audio::OutputLink`) switches to it on the sample where
//! the current file's audio runs out and promotes it to current; without
//! audio, the demuxer promotes it at end of file.

mod audio;
pub mod av;
pub mod cdda;
mod decoder;
mod demux;
pub mod disc;
mod filters;
#[cfg(has_dvdnav)]
mod nav;
mod present;
pub mod probe;
#[cfg(has_dvdnav)]
mod spu;
mod subs;
mod sync;
pub mod thumbnails;
pub mod types;
mod upscale;
mod video;

pub use types::*;

/// Whether DVDs can be played with their menus (this build has libdvdnav).
pub fn dvd_menus_available() -> bool {
    cfg!(has_dvdnav)
}

/// Whether the GPU upscalers (libplacebo: EWA Lanczos and shaders) are
/// available in this FFmpeg build.
pub fn gpu_upscaling_available() -> bool {
    upscale::available()
}

use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Condvar, Mutex};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;

/// How long audio must have run dry before video takes over the clock.
const AUDIO_DRY_HANDOVER: f64 = 0.3;

/// An `f64` stored in an `AtomicU64`.
pub struct AtomicF64(AtomicU64);
impl AtomicF64 {
    pub fn new(v: f64) -> Self {
        Self(AtomicU64::new(v.to_bits()))
    }
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Acquire))
    }
    pub fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Release)
    }
}

/// Player-wide controls that persist across files.
pub struct Controls {
    pub paused: AtomicBool,
    pub volume: AtomicF64,
    pub muted: AtomicBool,
    /// Sound held silent while the progress bar is dragged (no stutter of
    /// snippets from each position passed).
    pub hushed: AtomicBool,
    /// The window isn't being drawn: decode and keep time, but don't
    /// convert pictures nobody sees.
    pub video_hidden: AtomicBool,
    pub speed: AtomicF64,
    pub audio_delay: AtomicF64,
    pub config: Mutex<EngineConfig>,
    pub audio_fx: Mutex<(u64, AudioFx)>,
    pub video_adjust: Mutex<(u64, VideoAdjust)>,
    /// Largest useful output size in physical pixels (w << 32 | h).
    pub target_size: AtomicU64,
    /// The UI has reported `target_size` (until then it is a 4K guess,
    /// which mustn't start GPU upscaling for a window that isn't open).
    pub target_known: AtomicBool,
    /// The audio output currently open, so a preloaded next item can carry
    /// on through it without a gap.
    pub(crate) output: Mutex<Weak<audio::OutputLink>>,
}

impl Controls {
    pub fn target_size(&self) -> (u32, u32) {
        let v = self.target_size.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }
    pub fn speed(&self) -> f64 {
        self.speed.get()
    }
}

/// State visible to the UI.
pub struct Shared {
    pub state: Mutex<PlaybackState>,
    pub frame_slot: Mutex<Option<PresentedFrame>>,
    pub wake: Box<dyn Fn() + Send + Sync>,
    pub events: Sender<PlayerEvent>,
    pub current_session: AtomicU64,
    next_id: AtomicU64,
    /// The item to play after the current one, for gapless transitions.
    next: Mutex<NextSlot>,
}

/// What follows the current session.
#[derive(Default)]
struct NextSlot {
    /// What the UI wants played next.
    want: Option<(MediaSource, LoadOptions)>,
    /// Its preloaded session, started once the current one nears its end.
    session: Option<Session>,
}

/// State changes and events a preloaded session collects until it takes over.
pub struct Standby {
    state: PlaybackState,
    events: Vec<PlayerEvent>,
}

#[derive(Clone, Copy, Debug)]
pub struct SeekRequest {
    pub target: f64,
    pub relative: bool,
    pub precise: bool,
    /// Direction of a relative seek: -1 backward, 1 forward, 0 absolute.
    /// Preserved when the request is converted to an absolute target.
    pub direction: i8,
    /// Playback position before the relative jump (seconds from media start).
    /// Used only for non-precise, directional seeks.
    pub origin: f64,
}

/// Everything shared by the threads of one opened file.
pub struct SessionCtx {
    pub id: u64,
    pub abort: AtomicBool,
    pub controls: Arc<Controls>,
    pub shared: Arc<Shared>,
    pub vq: Arc<sync::PacketQueue>,
    pub aq: Arc<sync::PacketQueue>,
    pub sq: Arc<sync::PacketQueue>,
    pub frames: sync::FrameQueue,
    pub abuf: audio::AudioBuffer,
    pub aclk: sync::Clock,
    pub vclk: sync::Clock,
    pub audio_active: AtomicBool,
    /// When the audio output last ran out of samples with none queued
    /// (`sync::now()` time; NaN while audio flows). See `audio_is_master`.
    pub audio_dry_since: AtomicF64,
    /// Non-zero while an audio track's output device is still being opened.
    /// Holds the token of the component doing it, so a stale one can't clear
    /// a newer one's flag. Video shows its first frame but waits meanwhile.
    pub audio_pending: AtomicU64,
    pub audio_tokens: AtomicU64,
    pub video_active: AtomicBool,
    /// The video track is cover art: one picture, decoded once.
    pub cover_art: AtomicBool,
    /// Asks the demuxer to decode the cover art again (for a new window size
    /// or new settings; see [`SessionCtx::redo_cover`]).
    pub cover_redo: AtomicBool,
    pub audio_finished: AtomicI32,
    pub video_finished: AtomicI32,
    pub eof: AtomicBool,
    pub seek_req: Mutex<Option<SeekRequest>>,
    pub demux_cv: Condvar,
    pub demux_lock: Mutex<()>,
    /// Exact-seek target: decoders drop frames before it (pts, serial).
    pub seek_target: Mutex<Option<(f64, i32)>>,
    pub seek_display: AtomicF64,
    pub seeking: AtomicBool,
    pub step: AtomicBool,
    pub show_one: AtomicBool,
    pub stepped: AtomicBool,
    pub start_time: AtomicF64,
    pub duration: AtomicF64,
    pub switch_req: Mutex<Vec<(TrackKind, Option<TrackId>)>>,
    pub subs: subs::SubStore,
    pub last_frame: Mutex<Option<present::LastFrame>>,
    /// A screenshot for the video thread to take from the next picture,
    /// downloaded whole: the one on screen was resized on the GPU.
    pub screenshot_req: Mutex<Option<PathBuf>>,
    pub sub_canvas: AtomicU64,
    /// `Some` while this session is preloaded as the next item: it opens,
    /// decodes and buffers, but shows and plays nothing, and its state and
    /// events reach the UI only when it takes over ([`SessionCtx::promote`]).
    standby: Mutex<Option<Standby>>,
    standby_flag: AtomicBool,
    /// The session this one was preloaded to follow (0 if none).
    pub follows: u64,
    /// A DVD played with its menus: the disc's navigator.
    #[cfg(has_dvdnav)]
    pub nav: Mutex<Option<Arc<nav::Nav>>>,
    /// Draws DVD subpictures (menu buttons) instead of FFmpeg's decoder.
    #[cfg(has_dvdnav)]
    pub spu_overlay: Mutex<Option<Arc<spu::Overlay>>>,
}

impl SessionCtx {
    /// Lets go of what an ended session holds (its demuxer has exited and
    /// stopped the decoders). Something may keep the context itself alive
    /// past that, such as an audio device open that never returns: it
    /// shouldn't keep queued packets, pictures (a CUDA frame keeps the GPU
    /// device open) or the disc's navigator with it.
    fn release(&self) {
        self.vq.flush();
        self.aq.flush();
        self.sq.flush();
        self.frames.clear();
        *self.last_frame.lock() = None;
        self.subs.clear_embedded();
        #[cfg(has_dvdnav)]
        {
            *self.nav.lock() = None;
            *self.spu_overlay.lock() = None;
        }
    }

    pub fn alive(&self) -> bool {
        !self.abort.load(Ordering::Acquire)
    }

    pub fn is_current(&self) -> bool {
        self.shared.current_session.load(Ordering::Acquire) == self.id
    }

    pub fn emit(&self, ev: PlayerEvent) {
        if !self.alive() {
            return;
        }
        if let Some(sb) = self.standby.lock().as_mut() {
            sb.events.push(ev);
            return;
        }
        if self.is_current() {
            let _ = self.shared.events.send(ev);
            (self.shared.wake)();
        }
    }

    pub fn update_state(&self, f: impl FnOnce(&mut PlaybackState)) {
        if let Some(sb) = self.standby.lock().as_mut() {
            f(&mut sb.state);
            return;
        }
        if self.is_current() {
            f(&mut self.shared.state.lock());
        }
    }

    /// True while preloaded and waiting to take over.
    pub fn is_standby(&self) -> bool {
        self.standby_flag.load(Ordering::Acquire)
    }

    /// Makes this preloaded session the one playing: publishes the state and
    /// events it collected and tells the UI it advanced. Returns false if it
    /// already took over or was shut down.
    pub fn promote(&self) -> bool {
        let mut sb = self.standby.lock();
        if !self.alive() {
            return false;
        }
        let Some(s) = sb.take() else { return false };
        self.standby_flag.store(false, Ordering::Release);
        let source = s.state.source.clone();
        {
            let mut st = self.shared.state.lock();
            *st = s.state;
            self.shared.current_session.store(self.id, Ordering::Release);
        }
        if let Some(src) = source {
            let _ = self.shared.events.send(PlayerEvent::Advanced(src));
        }
        for ev in s.events {
            let _ = self.shared.events.send(ev);
        }
        drop(sb);
        log::debug!("session {} took over from {}", self.id, self.follows);
        (self.shared.wake)();
        self.wake_demuxer();
        true
    }

    pub fn wake(&self) {
        if self.is_current() {
            (self.shared.wake)();
        }
    }

    pub fn wake_demuxer(&self) {
        let _g = self.demux_lock.lock();
        self.demux_cv.notify_all();
    }

    /// Cover art is decoded and filtered once, for the window size and
    /// settings of the moment. Decodes it again for the current ones, so a
    /// larger window gets it enlarged (by the upscaler chosen) from the
    /// original rather than the old picture stretched. False when the video
    /// isn't cover art.
    pub fn redo_cover(&self) -> bool {
        if !self.cover_art.load(Ordering::Acquire) {
            return false;
        }
        self.cover_redo.store(true, Ordering::Release);
        self.wake_demuxer();
        true
    }

    pub fn paused(&self) -> bool {
        self.controls.paused.load(Ordering::Acquire)
    }

    /// The clock everything is synchronised to: the audio device when audio
    /// is playing, otherwise the video clock.
    pub fn master_clock(&self) -> f64 {
        if self.audio_is_master() { self.aclk.get(self.aq.serial()) } else { self.vclk.get(self.vq.serial()) }
    }

    /// True while playback is held until the audio device is ready.
    pub fn audio_pending(&self) -> bool {
        self.audio_pending.load(Ordering::Acquire) != 0
    }

    /// Clears `audio_pending` if `token`'s audio component still holds it.
    pub fn release_audio_pending(&self, token: u64) {
        let _ = self.audio_pending.compare_exchange(token, 0, Ordering::AcqRel, Ordering::Relaxed);
    }

    /// Audio drives the clock, unless it has run dry while video goes on:
    /// a DVD menu whose music is shorter than its animation, or a file whose
    /// audio track ends first. The audio clock stands still then, and the
    /// pictures (and menu buttons) after it would wait for it.
    pub fn audio_is_master(&self) -> bool {
        if !self.audio_active.load(Ordering::Acquire) {
            return false;
        }
        let dry = self.audio_dry_since.get();
        !(dry.is_finite() && sync::now() - dry > AUDIO_DRY_HANDOVER && self.video_active.load(Ordering::Acquire))
    }

    /// Playback position in seconds from the start of the media.
    pub fn position(&self) -> f64 {
        if self.seeking.load(Ordering::Acquire) {
            return self.seek_display.get();
        }
        let start = self.start_time.get();
        let m = self.master_clock();
        if m.is_finite() {
            let d = self.duration.get();
            let p = (m - start).max(0.0);
            if d > 0.0 { p.min(d) } else { p }
        } else {
            self.seek_display.get()
        }
    }

    pub fn request_seek(&self, req: SeekRequest) {
        let base = if self.seeking.load(Ordering::Acquire) { self.seek_display.get() } else { self.position() };
        let target = if req.relative { (base + req.target).max(0.0) } else { req.target.max(0.0) };
        self.seek_display.set(target);
        // Preserve both the direction and the starting position when
        // converting a relative seek into a coalesced absolute request.
        let abs = SeekRequest {
            target,
            relative: false,
            precise: req.precise,
            direction: req.direction,
            origin: if req.relative { base } else { target },
        };
        *self.seek_req.lock() = Some(abs);
        self.seeking.store(true, Ordering::Release);
        self.wake_demuxer();
    }
}

#[derive(Default, Clone)]
pub struct LoadOptions {
    pub start_at: Option<f64>,
    /// A DVD played with its menus starts in this title (at `start_at`)
    /// instead of at its first-play program: a reload keeps its place.
    #[cfg_attr(not(has_dvdnav), allow(dead_code))]
    pub dvd_title: Option<u32>,
    pub external_subs: Vec<PathBuf>,
    pub start_paused: bool,
}

struct Session {
    ctx: Arc<SessionCtx>,
    demux: Option<JoinHandle<()>>,
}

impl Session {
    fn shutdown(mut self) {
        let ctx = self.ctx.clone();
        ctx.abort.store(true, Ordering::Release);
        ctx.vq.abort();
        ctx.aq.abort();
        ctx.sq.abort();
        ctx.frames.abort();
        ctx.abuf.abort();
        ctx.wake_demuxer();
        // Join in the background so switching files never blocks the UI.
        if let Some(h) = self.demux.take() {
            std::thread::Builder::new()
                .name("session-reaper".into())
                .spawn(move || {
                    let _ = h.join();
                    ctx.release();
                })
                .ok();
        }
    }
}

/// The handle the UI uses to control playback.
pub struct Player {
    controls: Arc<Controls>,
    shared: Arc<Shared>,
    session: Option<Session>,
    events_rx: Receiver<PlayerEvent>,
}

/// How long before the end of the current item the next one is preloaded.
const PRELOAD_SECS: f64 = 10.0;

fn new_session(controls: &Arc<Controls>, shared: &Arc<Shared>, source: &MediaSource, follows: u64) -> Arc<SessionCtx> {
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let standby = (follows != 0).then(|| Standby {
        state: PlaybackState { status: Status::Loading, source: Some(source.clone()), ..Default::default() },
        events: Vec::new(),
    });
    let ctx = SessionCtx {
        id,
        abort: AtomicBool::new(false),
        controls: controls.clone(),
        shared: shared.clone(),
        vq: Arc::new(sync::PacketQueue::new()),
        aq: Arc::new(sync::PacketQueue::new()),
        sq: Arc::new(sync::PacketQueue::new()),
        frames: sync::FrameQueue::new(4),
        abuf: audio::AudioBuffer::new(),
        aclk: sync::Clock::new(),
        vclk: sync::Clock::new(),
        audio_active: AtomicBool::new(false),
        audio_dry_since: AtomicF64::new(f64::NAN),
        audio_pending: AtomicU64::new(0),
        audio_tokens: AtomicU64::new(0),
        video_active: AtomicBool::new(false),
        cover_art: AtomicBool::new(false),
        cover_redo: AtomicBool::new(false),
        audio_finished: AtomicI32::new(-1),
        video_finished: AtomicI32::new(-1),
        eof: AtomicBool::new(false),
        seek_req: Mutex::new(None),
        demux_cv: Condvar::new(),
        demux_lock: Mutex::new(()),
        seek_target: Mutex::new(None),
        seek_display: AtomicF64::new(0.0),
        seeking: AtomicBool::new(false),
        step: AtomicBool::new(false),
        show_one: AtomicBool::new(false),
        stepped: AtomicBool::new(false),
        start_time: AtomicF64::new(0.0),
        duration: AtomicF64::new(0.0),
        switch_req: Mutex::new(Vec::new()),
        subs: subs::SubStore::new(),
        last_frame: Mutex::new(None),
        screenshot_req: Mutex::new(None),
        sub_canvas: AtomicU64::new(0),
        standby_flag: AtomicBool::new(standby.is_some()),
        standby: Mutex::new(standby),
        follows,
        #[cfg(has_dvdnav)]
        nav: Mutex::new(None),
        #[cfg(has_dvdnav)]
        spu_overlay: Mutex::new(None),
    };
    let speed = controls.speed();
    ctx.aclk.set_speed(speed);
    ctx.vclk.set_speed(speed);
    Arc::new(ctx)
}

fn start_session(ctx: Arc<SessionCtx>, source: MediaSource, opts: LoadOptions) -> Session {
    ctx.seek_display.set(opts.start_at.unwrap_or(0.0));
    let c = ctx.clone();
    let demux = std::thread::Builder::new()
        .name("demux".into())
        .spawn(move || demux::run(c, source, opts))
        .expect("spawn demux thread");
    Session { ctx, demux: Some(demux) }
}

/// Called by the current session's demuxer once it has read everything:
/// preloads the queued next item when the end is near.
fn preload_next(ctx: &SessionCtx) {
    if !ctx.is_current() {
        return;
    }
    let d = ctx.duration.get();
    if d > 0.0 && d - ctx.position() > PRELOAD_SECS {
        return;
    }
    let mut slot = ctx.shared.next.lock();
    if slot.session.is_some() {
        return;
    }
    let Some((source, opts)) = slot.want.clone() else { return };
    log::debug!("preloading the next item: {}", source.display_name());
    let next = new_session(&ctx.controls, &ctx.shared, &source, ctx.id);
    slot.session = Some(start_session(next, source, opts));
}

/// Called when `ctx` has finished playing: hands over to its preloaded
/// successor, if there is one. (With audio the output usually does this
/// already, sample-accurately, a moment earlier.)
fn advance(ctx: &SessionCtx) -> bool {
    let slot = ctx.shared.next.lock();
    match &slot.session {
        Some(s) if s.ctx.follows == ctx.id => s.ctx.promote(),
        _ => false,
    }
}

impl Player {
    pub fn new(config: EngineConfig, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Player {
            controls: Arc::new(Controls {
                paused: AtomicBool::new(false),
                volume: AtomicF64::new(1.0),
                muted: AtomicBool::new(false),
                hushed: AtomicBool::new(false),
                video_hidden: AtomicBool::new(false),
                speed: AtomicF64::new(1.0),
                audio_delay: AtomicF64::new(0.0),
                config: Mutex::new(config),
                audio_fx: Mutex::new((0, AudioFx::default())),
                video_adjust: Mutex::new((0, VideoAdjust::default())),
                target_size: AtomicU64::new((3840u64 << 32) | 2160),
                target_known: AtomicBool::new(false),
                output: Mutex::new(Weak::new()),
            }),
            shared: Arc::new(Shared {
                state: Mutex::new(PlaybackState::default()),
                frame_slot: Mutex::new(None),
                wake: Box::new(wake),
                events: tx,
                current_session: AtomicU64::new(0),
                next_id: AtomicU64::new(1),
                next: Mutex::new(NextSlot::default()),
            }),
            session: None,
            events_rx: rx,
        }
    }

    /// Starts playing `source`, replacing whatever was playing.
    pub fn load(&mut self, source: MediaSource, opts: LoadOptions) {
        self.stop_session();
        let ctx = new_session(&self.controls, &self.shared, &source, 0);
        self.shared.current_session.store(ctx.id, Ordering::Release);
        self.controls.paused.store(opts.start_paused, Ordering::Release);
        {
            let mut st = self.shared.state.lock();
            *st = PlaybackState { status: Status::Loading, source: Some(source.clone()), ..Default::default() };
        }
        *self.shared.frame_slot.lock() = None;
        self.session = Some(start_session(ctx, source, opts));
        (self.shared.wake)();
    }

    /// Queues what to play when the current item ends (`None`: nothing).
    /// It is preloaded shortly before the end and takes over without a gap;
    /// [`PlayerEvent::Advanced`] reports the switch.
    pub fn set_next(&mut self, next: Option<(MediaSource, LoadOptions)>) {
        self.adopt();
        let mut slot = self.shared.next.lock();
        let same = match (&slot.want, &next) {
            (Some((a, ao)), Some((b, bo))) => a == b && ao.start_at == bo.start_at,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        slot.want = next;
        if let Some(s) = slot.session.take() {
            s.shutdown();
        }
    }

    /// What [`Player::set_next`] last queued (cleared by a load and by the switch).
    pub fn queued_next(&self) -> Option<MediaSource> {
        self.shared.next.lock().want.as_ref().map(|(s, _)| s.clone())
    }

    /// Takes ownership of the preloaded session once it has taken over.
    fn adopt(&mut self) {
        let cur = self.shared.current_session.load(Ordering::Acquire);
        if self.session.as_ref().is_none_or(|s| s.ctx.id == cur) {
            return;
        }
        let mut slot = self.shared.next.lock();
        if slot.session.as_ref().is_some_and(|s| s.ctx.id == cur) {
            let next = slot.session.take();
            // The UI queues whatever follows the new item.
            slot.want = None;
            drop(slot);
            if let Some(old) = std::mem::replace(&mut self.session, next) {
                old.shutdown();
            }
        }
    }

    /// The current session and the preloaded one.
    fn sessions(&self) -> Vec<Arc<SessionCtx>> {
        let mut v: Vec<_> = self.session.iter().map(|s| s.ctx.clone()).collect();
        v.extend(self.shared.next.lock().session.as_ref().map(|s| s.ctx.clone()));
        v
    }

    fn stop_session(&mut self) {
        if let Some(s) = self.session.take() {
            s.shutdown();
        }
        let next = std::mem::take(&mut *self.shared.next.lock());
        if let Some(s) = next.session {
            s.shutdown();
        }
    }

    pub fn stop(&mut self) {
        self.stop_session();
        self.shared.current_session.store(0, Ordering::Release);
        *self.shared.state.lock() = PlaybackState::default();
        *self.shared.frame_slot.lock() = None;
        (self.shared.wake)();
    }

    pub fn is_active(&self) -> bool {
        self.session.is_some()
    }

    pub fn state(&self) -> PlaybackState {
        self.shared.state.lock().clone()
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&PlaybackState) -> R) -> R {
        f(&self.shared.state.lock())
    }

    pub fn drain_events(&mut self) -> Vec<PlayerEvent> {
        self.adopt();
        self.events_rx.try_iter().collect()
    }

    pub fn take_frame(&self) -> Option<PresentedFrame> {
        self.shared.frame_slot.lock().take()
    }

    pub fn position(&self) -> f64 {
        self.session.as_ref().map(|s| s.ctx.position()).unwrap_or(0.0)
    }

    pub fn duration(&self) -> f64 {
        self.session.as_ref().map(|s| s.ctx.duration.get()).unwrap_or(0.0)
    }

    pub fn paused(&self) -> bool {
        self.controls.paused.load(Ordering::Acquire)
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.adopt();
        let was = self.controls.paused.swap(paused, Ordering::AcqRel);
        if let Some(n) = &self.shared.next.lock().session {
            n.ctx.aclk.set_paused(paused);
            n.ctx.vclk.set_paused(paused);
        }
        if let Some(s) = &self.session {
            let ctx = &s.ctx;
            ctx.aclk.set_paused(paused);
            ctx.vclk.set_paused(paused);
            if was && !paused && ctx.stepped.swap(false, Ordering::AcqRel) {
                // Frame stepping desynchronised audio; realign precisely.
                let pos = ctx.position();
                ctx.request_seek(SeekRequest {
                    target: pos,
                    relative: false,
                    precise: true,
                    direction: 0,
                    origin: 0.0,
                });
            }
            ctx.update_state(|st| {
                if matches!(st.status, Status::Playing | Status::Paused) {
                    st.status = if paused { Status::Paused } else { Status::Playing };
                }
            });
            ctx.wake_demuxer();
        }
        (self.shared.wake)();
    }

    pub fn toggle_pause(&mut self) {
        let ended = self.with_state(|s| s.status == Status::Ended);
        if ended {
            self.seek(0.0, false, true);
            self.set_paused(false);
            return;
        }
        let p = !self.paused();
        self.set_paused(p);
    }

    /// Seeks to `target` seconds (or by `target` seconds if `relative`).
    pub fn seek(&mut self, target: f64, relative: bool, precise: bool) {
        if !target.is_finite() {
            return;
        }
        self.adopt();
        if let Some(s) = &self.session {
            let direction = if relative {
                if target > 0.0 {
                    1
                } else if target < 0.0 {
                    -1
                } else {
                    0
                }
            } else {
                0
            };
            s.ctx.request_seek(SeekRequest { target, relative, precise, direction, origin: 0.0 });
            s.ctx.update_state(|st| {
                if st.status == Status::Ended {
                    st.status = if s.ctx.paused() { Status::Paused } else { Status::Playing };
                }
            });
        }
    }

    pub fn frame_step(&mut self, forward: bool) {
        self.adopt();
        if self.session.is_none() {
            return;
        }
        if !self.paused() {
            self.set_paused(true);
        }
        let s = self.session.as_ref().unwrap();
        if forward {
            s.ctx.step.store(true, Ordering::Release);
            s.ctx.stepped.store(true, Ordering::Release);
        } else {
            let fps = self.with_state(|st| st.video.as_ref().map(|v| v.fps).unwrap_or(25.0));
            let fd = if fps > 1.0 { 1.0 / fps } else { 0.04 };
            let pos = s.ctx.position();
            s.ctx.request_seek(SeekRequest {
                target: (pos - fd * 1.5).max(0.0),
                relative: false,
                precise: true,
                direction: 0,
                origin: 0.0,
            });
            s.ctx.stepped.store(true, Ordering::Release);
        }
        let _ = s;
    }

    pub fn volume(&self) -> f64 {
        self.controls.volume.get()
    }
    /// Volume as a linear 0.0..=1.5 scale (1.0 = 100%).
    pub fn set_volume(&self, v: f64) {
        // `clamp` passes NaN through, which would silence (or corrupt) every sample.
        if v.is_finite() {
            self.controls.volume.set(v.clamp(0.0, 1.5));
        }
    }
    /// Silences output (with a fade) without changing the mute setting.
    pub fn set_hushed(&self, h: bool) {
        self.controls.hushed.store(h, Ordering::Relaxed);
    }
    /// Whether the video is currently out of sight (see [`Controls::video_hidden`]).
    pub fn set_video_hidden(&self, h: bool) {
        self.controls.video_hidden.store(h, Ordering::Relaxed);
    }
    pub fn muted(&self) -> bool {
        self.controls.muted.load(Ordering::Relaxed)
    }
    pub fn set_muted(&self, m: bool) {
        self.controls.muted.store(m, Ordering::Relaxed);
    }

    pub fn speed(&self) -> f64 {
        self.controls.speed()
    }
    pub fn set_speed(&self, speed: f64) {
        if !speed.is_finite() {
            return;
        }
        let speed = speed.clamp(0.25, 4.0);
        self.controls.speed.set(speed);
        for ctx in self.sessions() {
            ctx.aclk.set_speed(speed);
            ctx.vclk.set_speed(speed);
        }
    }

    pub fn audio_delay(&self) -> f64 {
        self.controls.audio_delay.get()
    }
    pub fn set_audio_delay(&self, d: f64) {
        if d.is_finite() {
            self.controls.audio_delay.set(d);
        }
    }

    pub fn set_audio_fx(&self, fx: AudioFx) {
        let mut g = self.controls.audio_fx.lock();
        if g.1 != fx {
            g.0 += 1;
            g.1 = fx;
        }
    }

    pub fn set_video_adjust(&self, adj: VideoAdjust) {
        let mut g = self.controls.video_adjust.lock();
        if g.1 != adj {
            g.0 += 1;
            g.1 = adj;
        }
        drop(g);
        // Re-show the current picture with the new filters when paused (and
        // cover art always: it isn't decoded again otherwise).
        if let Some(s) = &self.session
            && !s.ctx.redo_cover()
            && self.paused()
        {
            let pos = s.ctx.position();
            s.ctx.request_seek(SeekRequest { target: pos, relative: false, precise: true, direction: 0, origin: 0.0 });
        }
    }

    pub fn set_config(&self, cfg: EngineConfig) {
        // GPU upscaling just turned on for the video playing.
        if self.session.as_ref().is_some_and(|s| s.ctx.video_active.load(Ordering::Acquire)) {
            upscale::prewarm(&cfg);
        }
        // (The presenter notices changes to its own scaling by itself.)
        let shaders = upscale::ShaderSetKey::of(&cfg);
        let changed = {
            let mut c = self.controls.config.lock();
            let changed = upscale::ShaderSetKey::of(&c) != shaders;
            *c = cfg;
            changed
        };
        if changed && let Some(s) = &self.session {
            s.ctx.redo_cover();
        }
    }

    /// Tells the engine how large the video is drawn, in physical pixels, so
    /// it can avoid converting more pixels than will be shown. A function the
    /// UI's layout code calls with each size it lays the video out at.
    pub fn target_size_setter(&self) -> impl Fn(u32, u32) + 'static {
        let c = self.controls.clone();
        move |w, h| {
            c.target_size.store(((w.max(16) as u64) << 32) | h.max(16) as u64, Ordering::Relaxed);
            c.target_known.store(true, Ordering::Relaxed);
        }
    }

    pub fn select_track(&self, kind: TrackKind, id: Option<TrackId>) {
        if let Some(s) = &self.session {
            if kind == TrackKind::Subtitle {
                if let Some(TrackId::External(i)) = id {
                    s.ctx.subs.select_external(Some(i));
                    s.ctx.update_state(|st| st.selected.subtitle = id);
                    // Stop routing any embedded subtitle stream.
                    s.ctx.switch_req.lock().push((kind, None));
                    s.ctx.wake_demuxer();
                    (self.shared.wake)();
                    return;
                }
                s.ctx.subs.select_external(None);
            }
            s.ctx.switch_req.lock().push((kind, id));
            s.ctx.update_state(|st| match kind {
                TrackKind::Video => st.selected.video = id,
                TrackKind::Audio => st.selected.audio = id,
                TrackKind::Subtitle => st.selected.subtitle = id,
            });
            s.ctx.wake_demuxer();
            (self.shared.wake)();
        }
    }

    /// Loads an external subtitle file and selects it.
    pub fn add_subtitle_file(&self, path: &Path) {
        if let Some(s) = &self.session {
            subs::load_external_async(s.ctx.clone(), path.to_path_buf(), true);
        }
    }

    /// Subtitle events visible at `t` seconds (media time).
    pub fn subtitles_at(&self, t: f64, out: &mut Vec<SubEvent>) {
        out.clear();
        if let Some(s) = &self.session {
            s.ctx.subs.active_at(t, s.ctx.start_time.get(), out);
        }
    }

    /// Size of the canvas bitmap subtitles are positioned on.
    pub fn subtitle_canvas(&self) -> (u32, u32) {
        self.session
            .as_ref()
            .map(|s| {
                let v = s.ctx.sub_canvas.load(Ordering::Relaxed);
                ((v >> 32) as u32, v as u32)
            })
            .unwrap_or((0, 0))
    }

    /// Saves the current picture at its native resolution.
    pub fn screenshot(&self, path: PathBuf) {
        let Some(s) = &self.session else { return };
        let resized = s.ctx.last_frame.lock().as_ref().is_some_and(|l| l.source.is_some());
        if !resized {
            present::screenshot_async(s.ctx.clone(), path);
            return;
        }
        // The picture on screen is window-sized: have the video thread save
        // the next one whole. Without playback, decode this one again.
        *s.ctx.screenshot_req.lock() = Some(path);
        if self.paused() || s.ctx.eof.load(Ordering::Acquire) {
            let pos = s.ctx.position();
            s.ctx.request_seek(SeekRequest { target: pos, relative: false, precise: true, direction: 0, origin: 0.0 });
        }
    }

    /// Remote-control input for a DVD played with its menus. Returns false
    /// when nothing took it (not such a DVD, or the disc refused it).
    pub fn dvd_command(&self, cmd: DvdCommand) -> bool {
        #[cfg(has_dvdnav)]
        if let Some(s) = &self.session
            && let Some(nav) = s.ctx.nav.lock().clone()
        {
            let ok = nav.command(cmd);
            s.ctx.wake_demuxer();
            return ok;
        }
        let _ = cmd;
        false
    }

    pub fn has_frame(&self) -> bool {
        self.session.as_ref().map(|s| s.ctx.last_frame.lock().is_some()).unwrap_or(false)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop_session();
    }
}

/// One-time library initialisation.
pub fn init(verbose: bool) {
    av::init_logging(verbose);
    unsafe {
        av::ff::avformat_network_init();
    }
}

/// Closes GPU devices (hardware decoding, GPU upscaling) that nothing has
/// used for `idle`. Call it every second or so.
pub fn release_idle_devices(idle: std::time::Duration) {
    decoder::release_idle_hw(idle);
    upscale::release_idle(idle);
    disc::release_idle();
}

/// Lists audio output devices by name.
pub fn audio_devices() -> Vec<String> {
    audio::list_devices()
}
