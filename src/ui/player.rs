//! The main window view: state, playback logic and action handlers.
//! Rendering lives in `render.rs`, dialogs in `dialogs.rs`, menus in `menus.rs`.

use super::actions::*;
use super::dialogs::Dialog;
use super::menu::OpenMenu;
use super::text_input::{InputEvent, TextInput};
use super::theme::Theme;
use super::tray::{TrayEvent, TrayState};
use crate::engine::disc::{self, DvdTitle};
use crate::engine::{
    self, Deinterlace, DvdCommand, DvdDomain, DvdMenu, HwDecode, LoadOptions, MediaSource, Player, PlayerEvent,
    Rotation, Status, SubEvent, TrackId, TrackKind, VideoAdjust,
};
use crate::icons;
use crate::playlist::{
    self, Playlist,
    session::{Library as PlaylistLibrary, Snapshot as PlaylistSnapshot},
};
use crate::settings::{RepeatMode, ResumeStore, Settings};
use futures::StreamExt;
use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, ImageId, PathPromptOptions, Pixels, Point,
    RenderImage, Subscription, Window, px, size,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Called with a slider's new value (0..1).
pub type SliderFn = Rc<dyn Fn(&mut PlayerView, f32, &mut Window, &mut Context<PlayerView>)>;

pub const SPEEDS: &[f64] = &[0.25, 0.5, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Aspect {
    Auto,
    Ratio(f64, &'static str),
    Stretch,
    Fill,
}

pub const ASPECTS: &[Aspect] = &[
    Aspect::Auto,
    Aspect::Ratio(16.0 / 9.0, "16:9"),
    Aspect::Ratio(4.0 / 3.0, "4:3"),
    Aspect::Ratio(2.35, "2.35:1"),
    Aspect::Ratio(2.39, "2.39:1"),
    Aspect::Ratio(1.85, "1.85:1"),
    Aspect::Ratio(16.0 / 10.0, "16:10"),
    Aspect::Ratio(5.0 / 4.0, "5:4"),
    Aspect::Ratio(1.0, "1:1"),
    Aspect::Fill,
    Aspect::Stretch,
];

impl Aspect {
    pub fn label(&self) -> &'static str {
        match self {
            Aspect::Auto => "Default",
            Aspect::Ratio(_, l) => l,
            Aspect::Stretch => "Stretch to window",
            Aspect::Fill => "Fill window (crop)",
        }
    }
}

/// The progress bar being dragged.
pub struct Scrub {
    /// Where the bar is held (0..1). While the pointer moves the bar is
    /// drawn here, so it follows the pointer exactly; once playback has
    /// landed here it follows playback again.
    pub frac: f32,
    /// The last position actually sought to, and whether that was exact.
    sought: Option<(f32, bool)>,
    last_seek: Instant,
    /// A delayed seek is scheduled (seeks are throttled while dragging).
    seek_scheduled: bool,
    last_move: Instant,
    /// An exact seek is scheduled for when the pointer comes to rest.
    settle_scheduled: bool,
    /// Playback was paused for the drag and resumes on release.
    resume: bool,
}

/// Renders a replaced texture is kept for before it is freed. GPUI's draw
/// ends by waiting for the GPU to finish the draw before it, so two renders
/// on, nothing can still be reading it. (Each extra one kept a whole frame,
/// 8 MB at 1080p and 33 MB at 4K, in video memory.)
const RETIRE_AFTER: u8 = 2;
/// Minimum time between seeks while the progress bar is dragged.
const SCRUB_INTERVAL: Duration = Duration::from_millis(60);
/// How long the pointer must rest on the bar before an exact seek there.
const SCRUB_SETTLE: Duration = Duration::from_millis(150);

/// An active slider drag.
pub struct Drag {
    pub bounds: Rc<Cell<Bounds<Pixels>>>,
    pub vertical: bool,
    pub on_change: SliderFn,
    pub on_release: Option<SliderFn>,
    pub last: f32,
}

/// Snapshot used while dragging a modal by its title bar.
#[derive(Clone, Copy)]
pub struct ModalDragOrigin {
    pub start: Point<Pixels>,
    pub initial: (f32, f32),
    pub limits: (f32, f32, f32, f32),
}

pub struct PlayerView {
    pub player: Player,
    pub settings: Settings,
    pub resume: ResumeStore,
    pub playlist: Playlist,
    pub(super) playlist_library: PlaylistLibrary,
    playlist_save_at: Instant,
    pub theme: Theme,
    pub focus: FocusHandle,
    pub(super) playlist_focus: FocusHandle,
    wake_pending: Arc<AtomicBool>,

    // Picture
    pub frame: Option<Arc<RenderImage>>,
    pub frame_size: (u32, u32),
    /// Textures to free a few frames from now (`retire_textures`). Only the
    /// ID is kept: holding the image would keep its pixels (8 MB for a 1080p
    /// frame) alive after GPUI has uploaded and let go of them.
    retired: VecDeque<(ImageId, u8)>,
    pub sub_images: HashMap<u64, (Arc<RenderImage>, u32)>,
    /// Experimental Cosmic Text subtitle textures, width, height and age.
    pub outlined_sub_images: HashMap<u64, (Arc<RenderImage>, u32, u32, u32)>,
    pub subs: Vec<SubEvent>,
    pub video_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub(super) control_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub(super) playlist_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub(super) playlist_row_bounds: Rc<RefCell<Vec<(u64, Bounds<Pixels>)>>>,
    pub(super) playlist_scrub_bounds: Rc<RefCell<Vec<(u64, Bounds<Pixels>)>>>,
    /// Last measured video-area geometry, including display scale.
    last_subtitle_geometry: Option<(f32, f32, f32, f32, f32)>,
    /// The menu bar row, left uncovered by an open menu-bar menu's backdrop
    /// so hovering another title switches menus.
    pub menubar_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Draws the picture; a new frame notifies only it (see `picture`).
    pub picture: Entity<super::picture::Picture>,
    /// Everything else, made on the first render.
    pub chrome: Option<Entity<super::picture::Chrome>>,
    /// The subtitles last shown, to redraw the chrome when they change.
    subs_signature: u64,
    /// Pause state just before the last single-click pause toggle, so the
    /// second click of a double-click can put it back.
    pub click_pause: Option<(bool, Instant)>,
    /// `load_counter` when the left button last went down on the video. A
    /// click that started before the current file loaded (on the start
    /// screen's Recent list, say) mustn't count as a click on the video.
    pub click_origin: u64,
    /// Start position of a left-button press on the video area.
    pub window_move_origin: Option<Point<Pixels>>,
    /// A window drag started; suppress the corresponding video click.
    pub window_move_started: bool,
    /// Volume and mute as last written to the settings file, and a change
    /// waiting to be written (saved once it stops changing).
    saved_volume: (f64, bool),
    volume_change: Option<((f64, bool), Instant)>,

    // Interface state
    pub last_activity: Instant,
    pub pointer_over_controls: bool,
    pub osd: Option<(String, Instant)>,
    pub menu: Option<OpenMenu>,
    pub media_keys: Option<super::media_keys::MediaKeys>,
    pub dialog: Option<Dialog>,
    /// Offset from the centered position of the current modal (logical pixels).
    pub modal_offset: (f32, f32),
    /// Pointer position and modal offset when title-bar dragging began.
    pub modal_drag_origin: Option<ModalDragOrigin>,
    /// Actual bounds of the modal, updated after layout.
    pub modal_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub drag: Option<Drag>,
    pub(super) timeline_edge_drag: bool,
    pub seek_hover: Option<f32>,
    pub(super) thumbnails: super::thumbnails::TimelineThumbnails,
    pub(super) playlist_previews: super::playlist_thumbnails::PlaylistPreviews,
    pub(super) cache_badges: super::cache_badges::CacheBadges,
    pub scrub: Option<Scrub>,
    /// The playlist beside the video in a window (saved in settings).
    pub show_playlist: bool,
    pub(super) playlist_autohide_open: bool,
    /// Runtime fullscreen visibility, reset to the fullscreen preference on entry.
    pub fs_playlist: bool,
    /// Fullscreen as of the last render.
    pub in_fullscreen: bool,
    pub playlist_selection: playlist::Selection,
    pub playlist_hovered: Option<u64>,
    pub playlist_filter_open: bool,
    playlist_filter_cache: RefCell<playlist::FilterCache>,
    pub ab_loop: (Option<f64>, Option<f64>),
    pub loop_file: bool,
    /// The playlist item queued in the engine to follow the current one.
    armed: Option<(usize, MediaSource)>,
    pub aspect: Aspect,
    pub zoom: f32,
    pub video_adjust: VideoAdjust,
    pub sub_delay: f64,
    /// The Keyboard Shortcuts page is waiting for the keystroke to add to this command.
    pub capturing_key: Option<&'static str>,
    /// What the last change on the Keyboard Shortcuts page did (shown there).
    pub shortcut_note: Option<String>,
    /// Scroll positions of the dialog areas that draw a scrollbar.
    pub modal_scroll: gpui::ScrollHandle,
    pub options_scroll: gpui::ScrollHandle,
    pub dvd_titles: Option<(String, Vec<DvdTitle>)>,
    /// What the Open Disc dialog found in the selected drive or image
    /// (`None` while it is being read).
    pub disc_scan: Option<(String, Option<disc::DiscContent>)>,
    pub reading_disc: bool,
    pub fitted_for: Option<u64>,
    /// A requested window size and how many more times to re-apply it if
    /// the compositor's configure overrode it.
    pub pending_fit: Option<(gpui::Size<Pixels>, u8)>,
    pub load_counter: u64,
    pub last_status: Status,
    pub was_fullscreen: bool,
    pub inputs: HashMap<&'static str, Entity<TextInput>>,
    pub audio_devices: Vec<String>,
    pub dropping: bool,
    /// Insertion point (0..=len) shown while something is dragged over the playlist.
    pub pl_drop: Option<usize>,
    pub pl_scroll: gpui::UniformListScrollHandle,
    /// The volume percentage is being edited as text.
    pub editing_volume: bool,
    volume_blur: Option<Subscription>,
    /// Frames received from the engine and renders since `ui_stats.2`, for
    /// the once-a-second `-v` report.
    pub ui_stats: (u32, u32, Instant),
    /// The window has been rendered once (for the startup timings).
    pub rendered: bool,
    /// The window surface is currently declared non-opaque (see
    /// [`PlayerView::avoid_direct_scanout`]).
    surface_translucent: bool,
    /// Border around `frame` beyond its `frame_size` picture (see
    /// `PresentedFrame::pad`).
    pub frame_pad: u32,
    /// The current frame has been drawn (has a GPU texture to free later).
    pub frame_painted: bool,
    last_render: Instant,
    /// Frames keep arriving but the window isn't drawn (minimised, on
    /// another desktop): the engine skips converting pictures for it.
    video_hidden: bool,
    /// The window is hidden in the tray ("Play in the background").
    pub hidden_to_tray: bool,
    /// When the window last lost the focus (see `in_front`).
    deactivated_at: Option<Instant>,
    _subs: Vec<Subscription>,
}

impl Focusable for PlayerView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Everything that can start before the window exists. `main` creates this
/// before GPUI sets up the GPU (which takes 100–200 ms on some drivers), so
/// the first file is already opening, decoding and finding its audio device
/// by the time the window appears. A MavPlayer waiting in the tray makes a
/// new one each time it opens its window again (see `shell`).
pub struct Boot {
    pub settings: Settings,
    player: Player,
    wake_pending: Arc<AtomicBool>,
    wake_rx: futures::channel::mpsc::UnboundedReceiver<()>,
    /// The desktop icon theme has been resolved (first window only).
    icons_rx: Option<futures::channel::mpsc::UnboundedReceiver<()>>,
    /// This process is the one later launches hand media to.
    primary: bool,
    resume: ResumeStore,
    sources: Vec<MediaSource>,
    playlist_library: PlaylistLibrary,
    start_index: Option<usize>,
    restored_position: Option<Option<f64>>,
    /// The selected startup item is already loading.
    started: bool,
}

impl Boot {
    pub fn start(
        settings: Settings,
        mut sources: Vec<MediaSource>,
        primary: bool,
        collection: crate::playlist::session::CollectionRequest,
    ) -> Self {
        icons::set_use_system_theme(settings.icon_theme == crate::settings::IconTheme::System);
        // Icons from the desktop theme are resolved off the UI thread, once per process.
        static ICONS: std::sync::Once = std::sync::Once::new();
        let mut icons_rx = None;
        ICONS.call_once(|| {
            let (itx, irx) = futures::channel::mpsc::unbounded::<()>();
            icons::init_system_icons(move || {
                let _ = itx.unbounded_send(());
            });
            icons_rx = Some(irx);
        });

        let wake_pending = Arc::new(AtomicBool::new(false));
        let (tx, wake_rx) = futures::channel::mpsc::unbounded::<()>();
        let wp = wake_pending.clone();
        // Wake-ups sent before the view exists queue up in the channel.
        let mut player = Player::new(settings.engine_config(), move || {
            if !wp.swap(true, Ordering::AcqRel) {
                let _ = tx.unbounded_send(());
            }
        });
        player.set_volume(settings.volume);
        player.set_muted(settings.muted);
        player.set_audio_fx(settings.audio_fx.clone());
        let resume = ResumeStore::load();
        let mut playlist_library = PlaylistLibrary::load(collection);
        let (start_index, restored_position) = if sources.is_empty() {
            let snapshot = playlist_library.snapshot();
            sources = snapshot.sources.clone();
            (snapshot.current, Some(snapshot.position))
        } else {
            playlist_library.start_launch_playlist();
            (Some(0), None)
        };
        // A whole DVD (title 0) needs its titles probed first; that runs in the view.
        let dvd_menus = engine::dvd_menus_available();
        let started = match start_index.and_then(|i| sources.get(i)) {
            Some(MediaSource::Dvd { title: 0, .. }) if !dvd_menus => false,
            Some(MediaSource::Cd { track: 0, .. }) | None => false,
            Some(src) => {
                let start_at = restored_position
                    .unwrap_or_else(|| if settings.resume_playback { resume.get(&src.key()) } else { None });
                // Paused until the window exists, so sound doesn't start
                // before there is anything to see.
                player.load(src.clone(), LoadOptions { start_at, start_paused: true, ..Default::default() });
                log::debug!("startup: first file loading at {:.0} ms", crate::ms_since_start());
                true
            }
        };
        Self {
            settings,
            player,
            wake_pending,
            wake_rx,
            icons_rx,
            primary,
            resume,
            sources,
            playlist_library,
            start_index,
            restored_position,
            started,
        }
    }
}

impl PlayerView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, boot: Boot) -> Self {
        let Boot {
            settings,
            player,
            wake_pending,
            wake_rx: mut rx,
            icons_rx,
            primary,
            resume,
            sources,
            playlist_library,
            start_index,
            restored_position,
            started,
        } = boot;
        log::debug!("startup: creating player view at {:.0} ms", crate::ms_since_start());
        // Engine → UI wake-ups. The engine sends one only after the previous
        // one was handled (`wake_pending`), so a wake-up that fails to reach
        // the view must be retried: dropping it would freeze the picture for
        // good while audio plays on.
        cx.spawn_in(window, async move |this, cx| {
            while rx.next().await.is_some() {
                loop {
                    if this.update_in(cx, |v, window, cx| v.on_engine_wake(window, cx)).is_ok() {
                        break;
                    }
                    if this.upgrade().is_none() {
                        return;
                    }
                    log::debug!("engine wake-up could not reach the window; retrying");
                    cx.background_executor().timer(Duration::from_millis(16)).await;
                }
            }
        })
        .detach();
        // Media keys and the desktop's player controls.
        let (mtx, mut media_rx) = futures::channel::mpsc::unbounded();
        let media_keys = super::media_keys::MediaKeys::new(window, primary, move |ev| {
            let _ = mtx.unbounded_send(ev);
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = media_rx.next().await {
                if this.update_in(cx, |v, window, cx| v.on_media_event(ev, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // Cache previews wake the UI as soon as the first JPEG is available.
        let (preview_tx, mut preview_rx) = futures::channel::mpsc::unbounded();
        cx.spawn_in(window, async move |this, cx| {
            while preview_rx.next().await.is_some() {
                while preview_rx.try_recv().is_ok() {}
                if this.update_in(cx, |v, _, cx| v.refresh_playlist_hover(cx)).is_err() && this.upgrade().is_none() {
                    break;
                }
            }
        })
        .detach();
        // Smooth playback position even when no mouse events arrive. Keep the
        // expensive housekeeping tick separate, and avoid redraws when hidden.
        // While nothing plays, check four times a second rather than 30: the
        // 200 ms tick redraws for anything else, and an idle player shouldn't
        // wake the CPU for nothing.
        cx.spawn_in(window, async move |this, cx| {
            let mut wait = Duration::from_millis(33);
            loop {
                cx.background_executor().timer(wait).await;
                match this.update_in(cx, |v, window, cx| {
                    let playing = !v.player.paused() && v.player.with_state(|s| s.status == Status::Playing);
                    if playing && (!window.is_fullscreen() || v.controls_visible()) {
                        cx.notify();
                    }
                    playing
                }) {
                    Ok(playing) => wait = Duration::from_millis(if playing { 33 } else { 250 }),
                    Err(_) if this.upgrade().is_none() => break,
                    Err(_) => {}
                }
            }
        })
        .detach();
        // Periodic UI refresh (time display, auto-hide, A-B loop, OSD expiry).
        // Like the wake-ups, it only stops once the view is gone.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                if this.update_in(cx, |v, window, cx| v.tick(window, cx)).is_err() && this.upgrade().is_none() {
                    break;
                }
            }
        })
        .detach();

        let mut subs = vec![];
        // The Keyboard Shortcuts page records the next keystroke before any binding
        // can act on it.
        let me = cx.entity().downgrade();
        let wid = window.window_handle().window_id();
        subs.push(cx.intercept_keystrokes(move |ev, window, cx| {
            if window.window_handle().window_id() != wid || super::keymap::is_modifier_only(&ev.keystroke) {
                return;
            }
            let Some(me) = me.upgrade() else { return };
            if me.read(cx).capturing_key.is_none() {
                return;
            }
            cx.stop_propagation();
            let ks = ev.keystroke.clone();
            me.update(cx, |v, cx| v.capture_keystroke(&ks, cx));
        }));
        subs.push(cx.observe_window_activation(window, |v, window, _| {
            if !window.is_window_active() {
                v.deactivated_at = Some(Instant::now());
            }
        }));
        subs.push(cx.observe_window_appearance(window, |v, window, cx| {
            v.theme = Theme::resolve(v.settings.theme, window);
            cx.notify();
        }));
        subs.push(cx.on_app_quit(|v, _| {
            v.save_state();
            async {}
        }));
        // Closing from the window frame (the compositor's close button,
        // Alt+F4) drops this view before the app quits: save first. With
        // "Play in the background", a window with a file open hides instead.
        let me = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            me.update(cx, |v, cx| {
                if v.hide_on_close(cx) {
                    v.hide_to_tray(window, cx);
                    return false;
                }
                v.save_state();
                true
            })
            .unwrap_or(true)
        });

        // Repaint once the desktop icon theme has been resolved.
        if let Some(mut irx) = icons_rx {
            cx.spawn_in(window, async move |this, cx| {
                if irx.next().await.is_some() {
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
            })
            .detach();
        }

        let theme = Theme::resolve(settings.theme, window);
        let focus = cx.focus_handle();
        let playlist_focus = cx.focus_handle();
        subs.push(cx.on_focus(&playlist_focus, window, |_, _, cx| cx.notify()));
        subs.push(cx.on_blur(&playlist_focus, window, |_, _, cx| cx.notify()));
        window.focus(&focus);
        let video_bounds = Rc::new(Cell::new(Bounds::default()));
        let picture = cx.new(|_| super::picture::Picture {
            frame: None,
            size: (0, 0),
            pad: 0,
            aspect: Aspect::Auto,
            zoom: 1.0,
            area: video_bounds.clone(),
        });
        let mut v = Self {
            playlist: Playlist::new(settings.repeat, settings.shuffle),
            playlist_library,
            playlist_save_at: Instant::now(),
            show_playlist: settings.show_playlist,
            playlist_autohide_open: false,
            fs_playlist: settings.fullscreen_playlist_initially_visible(),
            in_fullscreen: window.is_fullscreen(),
            player,
            resume,
            theme,
            focus,
            playlist_focus,
            wake_pending,
            frame: None,
            frame_size: (0, 0),
            retired: VecDeque::new(),
            sub_images: HashMap::new(),
            outlined_sub_images: HashMap::new(),
            subs: Vec::new(),
            video_bounds,
            control_bounds: Rc::new(Cell::new(Bounds::default())),
            playlist_bounds: Rc::new(Cell::new(Bounds::default())),
            playlist_row_bounds: Default::default(),
            playlist_scrub_bounds: Default::default(),
            last_subtitle_geometry: None,
            menubar_bounds: Default::default(),
            picture,
            chrome: None,
            subs_signature: 0,
            click_pause: None,
            click_origin: 0,
            window_move_origin: None,
            window_move_started: false,
            saved_volume: (settings.volume, settings.muted),
            volume_change: None,
            last_activity: Instant::now(),
            pointer_over_controls: false,
            osd: None,
            menu: None,
            media_keys,
            dialog: None,
            modal_offset: (0.0, 0.0),
            modal_drag_origin: None,
            modal_bounds: Rc::new(Cell::new(Bounds::default())),
            drag: None,
            timeline_edge_drag: false,
            seek_hover: None,
            thumbnails: Default::default(),
            playlist_previews: super::playlist_thumbnails::PlaylistPreviews::with_wake(preview_tx),
            cache_badges: Default::default(),
            scrub: None,
            playlist_selection: Default::default(),
            playlist_hovered: None,
            playlist_filter_open: false,
            playlist_filter_cache: Default::default(),
            ab_loop: (None, None),
            loop_file: false,
            armed: None,
            aspect: Aspect::Auto,
            zoom: 1.0,
            video_adjust: VideoAdjust::default(),
            sub_delay: 0.0,
            capturing_key: None,
            shortcut_note: None,
            modal_scroll: gpui::ScrollHandle::new(),
            options_scroll: gpui::ScrollHandle::new(),
            dvd_titles: None,
            disc_scan: None,
            reading_disc: false,
            fitted_for: None,
            pending_fit: None,
            load_counter: 0,
            last_status: Status::Idle,
            was_fullscreen: false,
            inputs: HashMap::new(),
            audio_devices: Vec::new(),
            dropping: false,
            pl_drop: None,
            pl_scroll: gpui::UniformListScrollHandle::new(),
            editing_volume: false,
            volume_blur: None,
            ui_stats: (0, 0, Instant::now()),
            rendered: false,
            surface_translucent: false,
            frame_pad: 0,
            frame_painted: false,
            last_render: Instant::now(),
            video_hidden: false,
            hidden_to_tray: false,
            deactivated_at: None,
            settings,
            _subs: subs,
        };
        window.set_window_title("MavPlayer");
        if !sources.is_empty() {
            v.playlist.add(sources);
            if restored_position.is_some() {
                for (item, id) in v.playlist.items.iter_mut().zip(&v.playlist_library.snapshot().ids) {
                    item.persistent_id = *id;
                }
                for (&i, title) in &v.playlist_library.snapshot().custom_titles {
                    if let Some(item) = v.playlist.items.get_mut(i) {
                        item.custom_title = Some(title.clone());
                    }
                }
            }
            v.probe_playlist(cx);
            if let Some(index) = start_index {
                let paused = restored_position.is_some() || v.settings.start_paused;
                v.start_item_at(index, !started, paused, restored_position, window, cx);
                if started && !paused {
                    v.player.set_paused(false);
                }
            }
        }
        if v.settings.start_fullscreen && !window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        #[cfg(target_os = "linux")]
        if crate::integration::is_appimage()
            && !crate::integration::is_integrated()
            && !crate::integration::prompt_marker().exists()
        {
            v.dialog = Some(Dialog::Integrate);
        }
        v
    }

    // ───────────────────────── engine plumbing ─────────────────────────

    fn on_engine_wake(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.wake_pending.store(false, Ordering::Release);
        // A new frame redraws the picture; the rest of the window (the
        // chrome, see `picture`) only when what it shows changed.
        let mut chrome = false;
        if let Some(f) = self.player.take_frame() {
            // The placeholder screen goes, or subtitles move with the size.
            chrome |= self.frame.is_none() || (f.width, f.height) != self.frame_size;
            if self.frame.is_none()
                && let Some(t0) = crate::launch_instant()
            {
                log::info!(
                    "first video frame {}x{} at {:.0} ms after launch",
                    f.width,
                    f.height,
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
            self.frame_size = (f.width, f.height);
            self.ui_stats.0 += 1;
            self.frame_pad = f.pad;
            if let Some(buf) = image::RgbaImage::from_raw(f.width + 2 * f.pad, f.height + 2 * f.pad, f.bgra) {
                let img = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(buf)]));
                // A frame that was never drawn has no texture: just drop it.
                // (Retiring those would pile up while the window isn't drawn.)
                let old = self.frame.replace(img);
                self.retire_frame(old);
            }
            self.picture.update(cx, |_, cx| cx.notify());
            let subs = self.subtitle_signature();
            if subs != self.subs_signature {
                self.subs_signature = subs;
                chrome = true;
            }
            // Pictures arrive but nothing is drawn: the compositor isn't
            // showing the window. Stop converting frames for it until it is.
            if !self.video_hidden && self.last_render.elapsed() > Duration::from_millis(500) {
                self.video_hidden = true;
                self.player.set_video_hidden(true);
                log::debug!("ui: window not being drawn; pausing picture conversion");
            }
        }
        let events = self.player.drain_events();
        chrome |= !events.is_empty();
        for ev in events {
            match ev {
                PlayerEvent::Loaded => self.on_loaded(window, cx),
                PlayerEvent::EndOfFile => self.on_end_of_file(window, cx),
                PlayerEvent::Advanced(src) => self.on_advanced(src, window, cx),
                PlayerEvent::Error(e) => {
                    if let Some(i) = self.playlist.current
                        && let Some(it) = self.playlist.items.get_mut(i)
                    {
                        it.failed = true;
                    }
                    self.osd(format!("⚠ {e}"), cx);
                    log::error!("{e}");
                    // Skip unplayable items, but don't loop forever over a broken list.
                    let all_failed = self.playlist.items.iter().all(|i| i.failed);
                    if !all_failed
                        && let Some(n) = self.playlist.next_index(true)
                        && Some(n) != self.playlist.current
                    {
                        self.continue_to(n, window, cx);
                    }
                }
                PlayerEvent::Warning(w) => self.osd(w, cx),
                PlayerEvent::TracksChanged => self.ensure_subtitle(),
                PlayerEvent::ScreenshotSaved(p) => self.osd(format!("Screenshot saved: {}", p.display()), cx),
            }
        }
        if chrome {
            self.update_thumbnails(self.in_fullscreen);
            cx.notify();
        }
    }

    fn on_loaded(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let st = self.player.state();
        let Some(media) = st.media.clone() else { return };
        if let Some(MediaSource::File(path)) = &st.source {
            self.playlist_previews.invalidate(path);
        }
        let title = self
            .playlist
            .current
            .and_then(|i| self.playlist.items.get(i))
            .and_then(|item| item.custom_title.clone())
            .or_else(|| media.title.clone().filter(|t| !t.trim().is_empty()))
            .unwrap_or_else(|| media.source_name.clone());
        window.set_window_title(&format!("{title} — MavPlayer"));
        if let Some(i) = self.playlist.current
            && let Some(it) = self.playlist.items.get_mut(i)
        {
            it.duration = Some(media.duration).filter(|d| *d > 0.0);
            it.has_subtitles |= media.tracks.iter().any(|t| t.kind == TrackKind::Subtitle);
            it.audio_stream = match st.selected.audio {
                Some(TrackId::Stream(i)) if media.seekable => Some(i),
                _ => None,
            };
            it.video_stream = match st.selected.video {
                Some(TrackId::Stream(i))
                    if media.seekable && media.tracks.iter().any(|t| t.id == TrackId::Stream(i) && !t.is_cover_art) =>
                {
                    Some(i)
                }
                _ => None,
            };
            if matches!(it.source, MediaSource::File(_)) || it.title.is_none() {
                it.title = media.title.clone();
                it.artist = media.artist.clone();
            }
            it.failed = false;
        }
        if let Some(src) = &st.source
            && let Some(pos) = self.resume.get(&src.key()).filter(|_| self.settings.resume_playback)
        {
            self.osd(format!("Resumed at {}", super::widgets::format_time(pos)), cx);
        }
        self.ensure_subtitle();
        self.fit_window_if_needed(window, cx);
    }

    fn on_end_of_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(src) = self.player.with_state(|s| s.source.clone()) {
            self.resume.forget(&src.key());
        }
        if self.loop_file {
            self.player.seek(0.0, false, true);
            self.player.set_paused(false);
            return;
        }
        if !self.settings.auto_play_next && self.playlist.repeat != RepeatMode::One {
            return;
        }
        if let Some(n) = self.playlist.next_index(false) {
            self.continue_to(n, window, cx);
        } else if self.is_fullscreen(window) {
            // Playlist finished: leave fullscreen like most players do.
            window.toggle_fullscreen();
        }
    }

    /// What should play when the current item ends, for gapless playback.
    fn desired_next(&self) -> Option<(usize, MediaSource)> {
        let cur = self.playlist.current?;
        let n = if self.loop_file {
            cur
        } else if !self.settings.auto_play_next && self.playlist.repeat != RepeatMode::One {
            return None;
        } else {
            self.playlist.next_index(false)?
        };
        let item = self.playlist.items.get(n)?;
        // Discs pick their title interactively; broken items are skipped.
        if item.failed || matches!(item.source, MediaSource::Dvd { title: 0, .. }) {
            return None;
        }
        Some((n, item.source.clone()))
    }

    /// Tells the engine what follows the current item whenever that changes
    /// (playlist edits, repeat, shuffle, loop).
    fn update_next(&mut self) {
        let want = self.desired_next();
        // Loads (including reloads) clear the engine's queue behind our back.
        if want == self.armed && self.player.queued_next() == want.as_ref().map(|(_, s)| s.clone()) {
            return;
        }
        let next = want.as_ref().map(|(n, src)| {
            let start_at = (Some(*n) != self.playlist.current && self.settings.resume_playback)
                .then(|| self.resume.get(&src.key()))
                .flatten();
            (src.clone(), LoadOptions { start_at, ..Default::default() })
        });
        self.player.set_next(next);
        self.armed = want;
    }

    /// The queued item took over from the one that just finished.
    fn on_advanced(&mut self, src: MediaSource, window: &mut Window, cx: &mut Context<Self>) {
        self.thumbnails.waveform.reset();
        let prev = self.playlist.current;
        if let Some(it) = prev.and_then(|i| self.playlist.items.get(i)) {
            self.resume.forget(&it.source.key());
        }
        let idx = match self.armed.take() {
            Some((i, s)) if s == src && self.playlist.items.get(i).is_some_and(|it| it.source == src) => Some(i),
            _ => self.playlist.items.iter().position(|it| it.source == src),
        };
        self.playlist.current = idx;
        if let Some(i) = idx
            && let Some(it) = self.playlist.items.get_mut(i)
        {
            let st = self.player.state();
            if let Some(media) = st.media.as_ref() {
                if matches!(it.source, MediaSource::File(_)) {
                    it.title = media.title.clone();
                    it.artist = media.artist.clone();
                }
                it.duration = Some(media.duration).filter(|d| *d > 0.0);
                it.has_subtitles |= media.tracks.iter().any(|t| t.kind == TrackKind::Subtitle);
                it.audio_stream = match st.selected.audio {
                    Some(TrackId::Stream(i)) if media.seekable => Some(i),
                    _ => None,
                };
                it.video_stream = match st.selected.video {
                    Some(TrackId::Stream(i))
                        if media.seekable
                            && media.tracks.iter().any(|t| t.id == TrackId::Stream(i) && !t.is_cover_art) =>
                    {
                        Some(i)
                    }
                    _ => None,
                };
            }
        }
        if let MediaSource::File(path) = &src {
            self.playlist_previews.invalidate(path);
        }
        if idx != prev {
            if let Some(i) = idx {
                self.playlist_selection.select_if_empty(&self.playlist.items, i);
            }
            self.ab_loop = (None, None);
            self.sub_delay = 0.0;
            self.load_counter += 1;
            self.settings.add_recent(&src);
            if let Some(it) = idx.and_then(|i| self.playlist.items.get(i)) {
                window.set_window_title(&format!("{} — MavPlayer", it.display_title()));
            }
        }
        // Keep the last picture for a seamless cut, unless nothing will replace it.
        if self.player.with_state(|s| s.video.is_none()) {
            let old = self.frame.take();
            self.retire_frame(old);
        }
        self.subs.clear();
        self.update_next();
        cx.notify();
    }

    pub(super) fn refresh_playlist_hover(&mut self, cx: &mut Context<Self>) {
        let visible = self.playlist_shown();
        let panel_item = self.menu.as_ref().and_then(|m| m.playlist_item);
        if self.playlist_previews.update_hover(&self.settings, visible, panel_item, &self.thumbnails) {
            cx.notify();
        }
    }

    pub fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_playlist_autohide(window, cx);
        let mut changed = self.update_thumbnails(window.is_fullscreen());
        let current = self.player.state();
        let playlist_shown = self.playlist_shown();
        changed |= self.cache_badges.update(playlist_shown);
        let panel_item = self.menu.as_ref().and_then(|m| m.playlist_item);
        // An occluding popup can prevent delivery of the row's mouse-exit event.
        if panel_item.is_some()
            && self.playlist_previews.hover.as_ref().is_some_and(|h| {
                !window.is_window_hovered() || !h.bounds.contains(&window.mouse_position())
            })
        {
            self.clear_playlist_panel_scrub(cx);
        }
        if let Some(item) = panel_item.and_then(|id| self.playlist.items.iter().find(|it| it.id == id))
            && let Some(key) = super::playlist_thumbnails::Key::new(item, &self.settings)
        {
            let duration = item.duration.unwrap_or_default();
            let target = key.representative_time(&self.settings, duration);
            let default = duration * crate::engine::thumbnails::representative_fraction(&key.path);
            if self.thumbnails.for_playlist(&key.path, key.stream, key.count, target).is_none()
                && self.playlist_previews.cached_representative(&key, target, default).is_none()
            {
                let _ = self.playlist_previews.inline_pair(&key, target, default);
            }
        }
        let (playlist_changed, retired) =
            self.playlist_previews.update(&self.settings, playlist_shown, panel_item, &self.thumbnails, &current);
        self.retired.extend(retired.into_iter().map(|id| (id, RETIRE_AFTER)));
        changed |= playlist_changed;
        if let Some(message) = self.playlist_previews.take_message() {
            self.osd(message, cx);
        }
        if self.playlist_previews.take_maintenance() {
            self.thumbnails.refresh_cache_stats();
            self.cache_badges.invalidate();
        }
        if let Some((key, duration)) = self.playlist_previews.take_completed() {
            for (index, it) in self.playlist.items.iter_mut().enumerate() {
                if matches!(&it.source, MediaSource::File(path) if path == &key.path) {
                    it.duration = Some(duration);
                    if self.playlist.current != Some(index)
                        || current.selected.video == Some(TrackId::Stream(key.stream))
                    {
                        it.video_stream = Some(key.stream);
                    }
                }
            }
            if matches!(&current.source, Some(MediaSource::File(path)) if path == &key.path)
                && current.selected.video == Some(TrackId::Stream(key.stream))
            {
                self.reset_thumbnails();
                self.update_thumbnails(window.is_fullscreen());
            }
            changed = true;
        }
        // `video_bounds` is updated during GPUI layout, after subtitle elements
        // have been built. A fullscreen/resize frame may therefore use the
        // *previous* video size. Request one more render when the measured
        // layout differs, including the final compositor-assigned size.
        let bounds = self.video_bounds.get();
        let geometry = (
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            window.scale_factor(),
        );
        if self.last_subtitle_geometry != Some(geometry) {
            self.last_subtitle_geometry = Some(geometry);
            changed = true;
        }
        self.save_volume_soon();
        if self.playlist_save_at.elapsed() >= Duration::from_secs(1) {
            self.playlist_save_at = Instant::now();
            if self.playlist_library.autosave() {
                self.save_playlist_session(false, false);
            }
        }
        if self.scrub.is_none() {
            self.player.set_hushed(false);
        }
        if self.ui_stats.2.elapsed() >= Duration::from_secs(1) {
            if log::log_enabled!(log::Level::Debug) && self.player.is_active() {
                let vs = window.viewport_size();
                log::debug!(
                    "ui: {} frames received, {} renders; {}x{}{}{}",
                    self.ui_stats.0,
                    self.ui_stats.1,
                    f32::from(vs.width),
                    f32::from(vs.height),
                    if window.is_fullscreen() { ", fullscreen" } else { "" },
                    if self.wake_pending.load(Ordering::Acquire) { ", wake-up pending" } else { "" },
                );
            }
            self.ui_stats = (0, 0, Instant::now());
        }
        self.update_next();
        if let Some((want, tries)) = self.pending_fit {
            let got = window.viewport_size();
            let off = (f32::from(got.width) - f32::from(want.width)).abs()
                + (f32::from(got.height) - f32::from(want.height)).abs();
            if off < 4.0 || tries == 0 || window.is_fullscreen() || window.is_maximized() {
                self.pending_fit = None;
            } else {
                window.resize(want);
                self.pending_fit = Some((want, tries - 1));
            }
        }
        if let Some(mut mk) = self.media_keys.take() {
            mk.update(self);
            self.media_keys = Some(mk);
        }
        self.update_tray(cx);
        let status = self.player.with_state(|s| s.status.clone());
        if status != self.last_status {
            self.last_status = status.clone();
            changed = true;
        }
        if matches!(status, Status::Playing | Status::Loading) || self.player.with_state(|s| s.buffering) {
            changed = true;
        }
        if let (Some(a), Some(b)) = self.ab_loop
            && b > a
            && self.player.position() >= b
        {
            self.player.seek(a, false, true);
        }
        if let Some((_, t)) = &self.osd
            && t.elapsed() > Duration::from_millis(2200)
        {
            self.osd = None;
            changed = true;
        }
        // Auto-hide transitions.
        let idle = self.last_activity.elapsed().as_secs_f64();
        let hide_after = self.settings.controls_autohide_secs.max(0.5);
        if idle > hide_after && idle < hide_after + 0.5 {
            changed = true;
        }
        if self.was_fullscreen != window.is_fullscreen() {
            self.was_fullscreen = window.is_fullscreen();
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    fn update_thumbnails(&mut self, fullscreen: bool) -> bool {
        let mut waveform_changed = false;
        if self.playlist_previews.has_generation() || self.thumbnails.waveform.has_generation() {
            let current = self.player.with_state(|st| match (&st.source, &st.status) {
                (Some(MediaSource::File(path)), Status::Loading | Status::Playing | Status::Paused | Status::Ended) => {
                    Some(path.clone())
                }
                _ => None,
            });
            // Do not hold the playback-state mutex while walking the playlist.
            let paths: std::collections::HashSet<&Path> = self
                .playlist
                .items
                .iter()
                .filter_map(|it| match &it.source {
                    MediaSource::File(path) => Some(path.as_path()),
                    _ => None,
                })
                .collect();
            let keep = |path: &Path| current.as_deref() == Some(path) || paths.contains(path);
            waveform_changed =
                self.playlist_previews.retain_generation(keep) | self.thumbnails.waveform.retain_generation(keep);
        }
        waveform_changed |= self.thumbnails.waveform.update(&self.player, &self.settings, fullscreen);
        if self.thumbnails.waveform.take_maintenance() {
            self.thumbnails.refresh_cache_stats();
            self.cache_badges.invalidate();
        }
        let playlist_busy = self.settings.timeline_thumbnails
            && self.player.with_state(|st| {
                let Some(MediaSource::File(path)) = &st.source else {
                    return false;
                };
                let stream = match st.selected.video {
                    Some(TrackId::Stream(stream)) => Some(stream),
                    _ => None,
                };
                self.playlist_previews.generating_for(path, stream)
            });
        let (changed, retired) = self.thumbnails.update(&self.player, &self.settings, fullscreen, playlist_busy);
        self.retired.extend(retired.into_iter().map(|id| (id, RETIRE_AFTER)));
        if let Some((path, generated)) = self.thumbnails.take_completed_path() {
            if generated {
                self.playlist_previews.invalidate(&path);
                self.cache_badges.invalidate();
            }
            let st = self.player.state();
            if let Some(index) = self.playlist.current
                && let Some(it) = self.playlist.items.get_mut(index)
                && matches!(&it.source, MediaSource::File(p) if p == &path)
                && let Some(TrackId::Stream(stream)) = st.selected.video
            {
                it.video_stream = Some(stream);
            }
        }
        changed || waveform_changed
    }

    pub(super) fn regenerate_playlist_thumbnails(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let stream = self.player.with_state(|st| {
            if matches!(&st.source, Some(MediaSource::File(p)) if p == &path)
                && let Some(TrackId::Stream(stream)) = st.selected.video
            {
                return Some(stream);
            }
            None
        });
        self.playlist_previews.regenerate(path, stream);
        cx.notify();
    }

    /// Queue missing or explicitly requested thumbnails for local playlist items.
    pub(super) fn generate_playlist_items(&mut self, ids: Option<&[u64]>, missing_only: bool, cx: &mut Context<Self>) {
        self.generate_playlist_thumbnail_jobs(ids, missing_only, false, cx);
    }

    pub(super) fn generate_playlist_representatives(&mut self, ids: Option<&[u64]>, cx: &mut Context<Self>) {
        self.generate_playlist_thumbnail_jobs(ids, true, true, cx);
    }

    fn generate_playlist_thumbnail_jobs(
        &mut self,
        ids: Option<&[u64]>,
        missing_only: bool,
        single: bool,
        cx: &mut Context<Self>,
    ) {
        if !single && !self.settings.timeline_thumbnails {
            return;
        }
        let jobs: Vec<_> = self
            .playlist
            .items
            .iter()
            .filter(|it| ids.is_none_or(|ids| ids.contains(&it.id)))
            .filter_map(|it| {
                let MediaSource::File(path) = &it.source else {
                    return None;
                };
                if it.probed && it.duration.is_some() && it.video_stream.is_none() {
                    return None;
                }
                Some((path.clone(), it.video_stream))
            })
            .collect();
        for (path, stream) in jobs {
            let selected_stream = self.player.with_state(|st| {
                if matches!(&st.source, Some(MediaSource::File(p)) if p == &path)
                    && let Some(TrackId::Stream(stream)) = st.selected.video
                {
                    return Some(stream);
                }
                stream
            });
            if single {
                self.playlist_previews.generate_representative(path, selected_stream);
            } else if missing_only {
                self.playlist_previews.generate_missing(path, selected_stream);
            } else {
                self.playlist_previews.regenerate(path, selected_stream);
            }
        }
        cx.notify();
    }

    /// Explicit waveform jobs use a separate FIFO and never change playback.
    pub(super) fn generate_playlist_waveforms(
        &mut self,
        ids: Option<&[u64]>,
        missing_only: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.timeline_waveform {
            return;
        }
        let jobs: Vec<_> = self
            .playlist
            .items
            .iter()
            .filter(|it| ids.is_none_or(|ids| ids.contains(&it.id)))
            .filter_map(|it| {
                let MediaSource::File(path) = &it.source else {
                    return None;
                };
                if it.probed && it.duration.is_some() && it.audio_stream.is_none() {
                    return None;
                }
                Some((path.clone(), it.audio_stream))
            })
            .collect();
        for (path, stream) in jobs {
            let stream = self.player.with_state(|st| {
                if matches!(&st.source, Some(MediaSource::File(p)) if p == &path)
                    && let Some(TrackId::Stream(i)) = st.selected.audio
                {
                    return Some(i);
                }
                stream
            });
            self.thumbnails.waveform.enqueue(path, stream, &self.settings, !missing_only);
        }
        self.update_thumbnails(self.in_fullscreen);
        cx.notify();
    }

    /// The display indices are also used by keyboard navigation and selection.
    pub(super) fn visible_playlist_indices(&self, cx: &App) -> Vec<usize> {
        let query = if self.playlist_filter_open {
            self.inputs.get("playlist_filter").map(|input| input.read(cx).text()).unwrap_or_default()
        } else {
            String::new()
        };
        self.playlist_filter_cache.borrow_mut().visible_indices(&self.playlist.items, &query)
    }

    pub(super) fn playlist_focus_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.playlist_shown() {
            return;
        }
        window.focus(&self.playlist_focus);
        let visible = self.visible_playlist_indices(cx);
        if !visible.iter().any(|&i| self.playlist_selection.contains(self.playlist.items[i].id))
            && let Some(&i) = visible.first()
        {
            self.playlist_selection.single(self.playlist.items[i].id);
            self.pl_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        }
    }

    fn playlist_focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.playlist_filter_open && self.playlist_shown() {
            self.playlist_filter_open = false;
            window.focus(&self.playlist_focus);
            cx.notify();
            return;
        }
        if !self.playlist_enabled() {
            self.toggle_playlist(window, cx);
        }
        self.playlist_autohide_open = true;
        self.playlist_filter_open = true;
        let input = self.input("playlist_filter", "Filter playlist…", window, cx);
        window.focus(&input.focus_handle(cx));
        cx.notify();
    }

    fn playlist_key_move(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let visible = self.visible_playlist_indices(cx);
        let count = visible.len();
        if count == 0 {
            return;
        }
        let lead = self.playlist_selection.lead();
        let pos = visible
            .iter()
            .position(|&i| Some(self.playlist.items[i].id) == lead)
            .or_else(|| visible.iter().position(|&i| self.playlist_selection.contains(self.playlist.items[i].id)));
        let target = match pos {
            Some(p) => p.saturating_add_signed(delta).min(count - 1),
            None if delta < 0 => count - 1,
            None => 0,
        };
        // Home/End use the same path even when nothing is selected.
        let target = if delta == isize::MIN {
            0
        } else if delta == isize::MAX {
            count - 1
        } else {
            target
        };
        self.playlist_selection.click_visible(&self.playlist.items, &visible, visible[target], false, extend);
        self.pl_scroll
            .scroll_to_item(target, if delta < 0 { gpui::ScrollStrategy::Top } else { gpui::ScrollStrategy::Bottom });
        self.playlist_previews.hover = None;
        self.refresh_playlist_hover(cx);
    }

    fn playlist_key_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let visible = self.visible_playlist_indices(cx);
        let ids: Vec<_> = visible
            .iter()
            .map(|&i| self.playlist.items[i].id)
            .filter(|&id| self.playlist_selection.contains(id))
            .collect();
        if ids.is_empty() {
            return;
        }
        let first =
            visible.iter().position(|&i| self.playlist_selection.contains(self.playlist.items[i].id)).unwrap_or(0);
        self.remove_playlist_items(&ids, window, cx);
        let visible = self.visible_playlist_indices(cx);
        if let Some(&i) = visible.get(first.min(visible.len().saturating_sub(1))) {
            self.playlist_selection.single(self.playlist.items[i].id);
            self.pl_scroll.scroll_to_item(first.min(visible.len() - 1), gpui::ScrollStrategy::Bottom);
        }
    }

    pub(super) fn playlist_key_play(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let visible = self.visible_playlist_indices(cx);
        let selected = |i: usize| self.playlist_selection.contains(self.playlist.items[i].id);
        let lead = self.playlist_selection.lead();
        let index = visible
            .iter()
            .copied()
            .find(|&i| selected(i) && Some(self.playlist.items[i].id) == lead)
            .or_else(|| visible.iter().copied().find(|&i| selected(i)));
        if let Some(i) = index {
            self.play_index(i, window, cx);
        }
    }

    fn playlist_key_select_all(&mut self, cx: &mut Context<Self>) {
        let visible = self.visible_playlist_indices(cx);
        if let (Some(&first), Some(&last)) = (visible.first(), visible.last()) {
            self.playlist_selection.single(self.playlist.items[first].id);
            self.playlist_selection.click_visible(&self.playlist.items, &visible, last, false, true);
        }
    }

    pub(super) fn remove_playlist_items(&mut self, ids: &[u64], window: &mut Window, cx: &mut Context<Self>) {
        let ids: std::collections::HashSet<u64> = ids.iter().copied().collect();
        if self.playlist.current.and_then(|i| self.playlist.items.get(i)).is_some_and(|it| ids.contains(&it.id)) {
            self.stop(window, cx);
        }
        self.playlist.remove_ids(&ids);
        self.playlist_selection.remove(&ids);
        if self.playlist_hovered.is_some_and(|id| ids.contains(&id)) {
            self.playlist_hovered = None;
        }
        if self.playlist_previews.hover.as_ref().is_some_and(|h| ids.contains(&h.id)) {
            self.playlist_previews.hover = None;
        }
        self.update_next();
        cx.notify();
    }

    pub(super) fn move_playlist_items(&mut self, ids: &[u64], up: bool, cx: &mut Context<Self>) {
        self.playlist.move_selected(ids, up);
        self.playlist_previews.hover = None;
        self.update_next();
        cx.notify();
    }

    pub(super) fn drop_playlist_items(&mut self, ids: &[u64], at: usize, cx: &mut Context<Self>) {
        self.playlist.drop_selected(ids, at);
        self.playlist_previews.hover = None;
        self.update_next();
        cx.notify();
    }

    pub(super) fn regenerate_thumbnails(&mut self, cx: &mut Context<Self>) {
        if let Some(MediaSource::File(path)) = self.player.with_state(|st| st.source.clone()) {
            self.regenerate_playlist_thumbnails(path, cx);
        }
    }

    fn reset_thumbnails(&mut self) {
        self.retired.extend(self.thumbnails.reset().into_iter().map(|id| (id, RETIRE_AFTER)));
        self.seek_hover = None;
    }

    /// Keeps KWin from scanning the fullscreen window out directly.
    ///
    /// A fullscreen surface that declares itself opaque lets the compositor
    /// put the app's buffers straight on the display instead of compositing
    /// them. With KWin 6 on the NVIDIA driver that path fails ("Failed to
    /// allocate a graphics buffer for a swapchain") and the screen keeps the
    /// last picture while the app goes on rendering. Declaring the surface
    /// non-opaque while fullscreen makes KWin composite it as usual. Every
    /// pixel is still drawn opaque, so nothing looks different.
    pub fn avoid_direct_scanout(&mut self, window: &mut Window) {
        let want = window.is_fullscreen() && cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some();
        if want != self.surface_translucent {
            self.surface_translucent = want;
            window.set_background_appearance(if want {
                gpui::WindowBackgroundAppearance::Transparent
            } else {
                gpui::WindowBackgroundAppearance::Opaque
            });
        }
    }

    /// Frees the texture of a picture no longer shown, a few frames later.
    /// A picture that was never drawn has none: it is just dropped.
    /// (Retiring those would pile up while the window isn't drawn.)
    fn retire_frame(&mut self, old: Option<Arc<RenderImage>>) {
        if let Some(old) = old
            && self.frame_painted
        {
            self.retired.push_back((old.id, RETIRE_AFTER));
        }
        self.frame_painted = false;
    }

    pub fn retire_textures(&mut self, window: &mut Window) {
        self.last_render = Instant::now();
        self.retired.extend(self.thumbnails.waveform.take_retired_images().into_iter().map(|id| (id, RETIRE_AFTER)));
        self.retired.extend(self.thumbnails.take_retired_barcode_images().into_iter().map(|id| (id, RETIRE_AFTER)));
        if self.video_hidden && !self.hidden_to_tray {
            self.video_hidden = false;
            self.player.set_video_hidden(false);
        }
        self.frame_painted |= self.frame.is_some();
        for e in self.retired.iter_mut() {
            e.1 = e.1.saturating_sub(1);
        }
        while self.retired.front().is_some_and(|e| e.1 == 0) {
            let (id, _) = self.retired.pop_front().unwrap();
            // `drop_image` only reads the ID (and the frame count: one).
            let mut stub = RenderImage::new(smallvec::smallvec![image::Frame::new(image::RgbaImage::new(1, 1))]);
            stub.id = id;
            let _ = window.drop_image(Arc::new(stub));
        }
    }

    /// Frees subtitle bitmaps not shown for a while (counted in chrome
    /// renders, which show them).
    pub fn age_sub_images(&mut self) {
        let stale: Vec<u64> = self
            .sub_images
            .iter_mut()
            .filter_map(|(id, (_, age))| {
                *age += 1;
                (*age > 30).then_some(*id)
            })
            .collect();
        for id in stale {
            if let Some((img, _)) = self.sub_images.remove(&id) {
                self.retired.push_back((img.id, RETIRE_AFTER));
            }
        }
        let stale_outlined: Vec<u64> = self
            .outlined_sub_images
            .iter_mut()
            .filter_map(|(key, (_, _, _, age))| {
                *age += 1;
                (*age > 30).then_some(*key)
            })
            .collect();
        for key in stale_outlined {
            if let Some((img, _, _, _)) = self.outlined_sub_images.remove(&key) {
                self.retired.push_back((img.id, RETIRE_AFTER));
            }
        }
    }

    // ───────────────────────── opening media ─────────────────────────

    pub fn osd(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        if self.settings.osd_messages {
            self.osd = Some((text.into(), Instant::now()));
            cx.notify();
        }
    }

    pub fn save_current_position(&mut self) {
        let st = self.player.state();
        if let (Some(src), Some(m)) = (st.source, st.media)
            && !m.is_live
            && m.seekable
            // A DVD with menus starts at its menu again.
            && st.dvd.is_none()
            && matches!(st.status, Status::Playing | Status::Paused)
        {
            self.resume.remember(src.key(), self.player.position(), m.duration);
        }
    }

    /// Writes a changed volume or mute to the settings file once it has
    /// stayed put for a moment, so it survives however the app ends.
    fn save_volume_soon(&mut self) {
        let now = (self.player.volume(), self.player.muted());
        if now == self.saved_volume {
            self.volume_change = None;
            return;
        }
        match self.volume_change {
            Some((v, at)) if v == now => {
                if at.elapsed() >= Duration::from_secs(1) {
                    self.settings.volume = now.0;
                    self.settings.muted = now.1;
                    self.settings.save();
                    self.saved_volume = now;
                    self.volume_change = None;
                }
            }
            _ => self.volume_change = Some((now, Instant::now())),
        }
    }

    fn playlist_snapshot(&self) -> PlaylistSnapshot {
        let sources: Vec<_> = self
            .playlist
            .items
            .iter()
            .map(|it| match &it.source {
                MediaSource::File(path) if path.is_relative() => {
                    MediaSource::File(std::path::absolute(path).unwrap_or_else(|_| path.clone()))
                }
                source => source.clone(),
            })
            .collect();
        let current = self.playlist.current.filter(|&i| i < sources.len());
        let previous = self.playlist_library.snapshot();
        let position = current.and_then(|i| {
            let st = self.player.state();
            if st.source.as_ref() == Some(&self.playlist.items[i].source) {
                if matches!(st.status, Status::Playing | Status::Paused)
                    && st.media.as_ref().is_some_and(|m| m.seekable && !m.is_live)
                    && st.dvd.is_none()
                {
                    return Some(self.player.position().max(0.0));
                }
                if st.status == Status::Ended {
                    return None;
                }
            }
            if previous.current.is_some_and(|j| {
                previous.ids.get(j) == Some(&self.playlist.items[i].persistent_id)
                    && previous.sources.get(j) == sources.get(i)
            }) {
                previous.position
            } else {
                None
            }
        });
        let custom_titles = self
            .playlist
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| item.custom_title.clone().map(|title| (i, title)))
            .collect();
        let ids = self.playlist.items.iter().map(|item| item.persistent_id).collect();
        PlaylistSnapshot { sources, ids, custom_titles, current, position }
    }

    fn save_playlist_session(&mut self, exact: bool, manual: bool) {
        let snapshot = self.playlist_snapshot();
        self.playlist_library.capture(snapshot, exact, manual);
        if let Err(err) = self.playlist_library.flush() {
            log::warn!("could not save playlist state: {err}");
        }
    }

    pub(super) fn set_playlist_item_title(
        &mut self, id: u64, title: &str, window: &mut Window, cx: &mut Context<Self>,
    ) {
        if let Some((index, item)) = self.playlist.items.iter_mut().enumerate().find(|(_, item)| item.id == id) {
            let title = title.trim();
            if item.custom_title.is_some() || title != item.display_title() {
                item.custom_title = (!title.is_empty()).then(|| title.to_owned());
            }
            if self.playlist.current == Some(index) {
                window.set_window_title(&format!("{} — MavPlayer", item.display_title()));
            }
            self.save_playlist_session(true, false);
        }
        self.close_dialog(window, cx);
    }

    pub(super) fn toggle_playlist_autosave(&mut self, cx: &mut Context<Self>) {
        let enabled = !self.playlist_library.autosave();
        self.playlist_library.set_autosave(enabled);
        self.save_playlist_session(true, false);
        cx.notify();
    }

    pub(super) fn save_playlist_workspace(&mut self, cx: &mut Context<Self>) {
        if !self.playlist_library.writable {
            self.osd("Playlist state is not writable in this window", cx);
            return;
        }
        let snapshot = self.playlist_snapshot();
        self.playlist_library.capture(snapshot, true, true);
        match self.playlist_library.flush() {
            Ok(()) => self.osd("Playlist state saved", cx),
            Err(err) => self.osd(format!("Could not save playlist state: {err}"), cx),
        }
    }

    pub(super) fn switch_playlist_workspace(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index == self.playlist_library.active || index >= self.playlist_library.slots.len() {
            return;
        }
        self.stop(window, cx);
        self.playlist_library.switch(index);
        self.restore_playlist_workspace(window, cx);
    }

    pub(super) fn delete_playlist_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.playlist_library.can_delete() {
            return;
        }
        self.close_dialog(window, cx);
        self.stop(window, cx);
        if self.playlist_library.delete_active() {
            self.restore_playlist_workspace(window, cx);
        }
    }

    pub(super) fn name_playlist_workspace(
        &mut self,
        name: String,
        rename: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.playlist_library.valid_name(&name, rename) {
            self.osd("Choose a unique playlist name", cx);
            return;
        }
        self.close_dialog(window, cx);
        if rename {
            self.playlist_library.rename(name);
            self.save_playlist_session(true, false);
        } else {
            self.stop(window, cx);
            self.playlist_library.create(name);
            self.restore_playlist_workspace(window, cx);
        }
        cx.notify();
    }

    pub(super) fn duplicate_playlist_workspace(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.playlist_library.writable {
            self.osd("Playlist state is not writable in this window", cx);
            return;
        }
        if !self.playlist_library.valid_name(&name, false) {
            self.osd("Choose a unique playlist name", cx);
            return;
        }
        let snapshot = self.playlist_snapshot();
        self.playlist_library.capture(snapshot.clone(), true, false);
        if !self.playlist_library.duplicate(name, snapshot) {
            return;
        }
        // The live queue is now the duplicate, without restarting playback.
        for (item, id) in self.playlist.items.iter_mut().zip(&self.playlist_library.snapshot().ids) {
            item.persistent_id = *id;
        }
        self.close_dialog(window, cx);
        match self.playlist_library.flush() {
            Ok(()) => self.osd("Playlist duplicated", cx),
            Err(err) => self.osd(format!("Could not save playlists: {err}"), cx),
        }
        cx.notify();
    }

    fn restore_playlist_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.playlist_library.snapshot().clone();
        self.playlist.clear(); // Keep the monotonic item IDs used by asynchronous probes.
        self.playlist_selection.clear();
        self.playlist_hovered = None;
        self.playlist_previews.hover = None;
        self.armed = None;
        self.playlist.add(snapshot.sources);
        for (item, id) in self.playlist.items.iter_mut().zip(snapshot.ids) {
            item.persistent_id = id;
        }
        for (i, title) in snapshot.custom_titles {
            if let Some(item) = self.playlist.items.get_mut(i) {
                item.custom_title = Some(title);
            }
        }
        self.probe_playlist(cx);
        if let Some(index) = snapshot.current {
            self.start_item_at(index, true, true, Some(snapshot.position), window, cx);
        }
        if let Err(err) = self.playlist_library.flush() {
            self.osd(format!("Could not save playlists: {err}"), cx);
        }
        cx.notify();
    }

    pub fn save_state(&mut self) {
        self.save_current_position();
        self.save_playlist_session(true, false);
        self.settings.volume = self.player.volume();
        self.settings.muted = self.player.muted();
        self.settings.repeat = self.playlist.repeat;
        self.settings.shuffle = self.playlist.shuffle();
        self.settings.show_playlist = self.show_playlist;
        self.settings.save();
        self.saved_volume = (self.settings.volume, self.settings.muted);
        self.resume.save();
    }

    /// Plays playlist item `idx` because the user chose it (paused instead
    /// if "Start files paused" is on).
    pub fn play_index(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.start_item(idx, true, self.settings.start_paused, window, cx);
    }

    /// Moves on to item `idx` by itself (the previous one ended or failed):
    /// always plays.
    fn continue_to(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.start_item(idx, true, false, window, cx);
    }

    /// Makes `idx` the current item. With `load` false the engine is
    /// already playing it (started early by [`Boot`]).
    fn start_item(&mut self, idx: usize, load: bool, paused: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.start_item_at(idx, load, paused, None, window, cx);
    }

    fn start_item_at(
        &mut self,
        idx: usize,
        load: bool,
        paused: bool,
        restored_position: Option<Option<f64>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.playlist.items.get(idx).cloned() else { return };
        self.thumbnails.waveform.reset();
        self.reset_thumbnails();
        self.save_current_position();
        self.playlist.current = Some(idx);
        self.playlist_selection.select_if_empty(&self.playlist.items, idx);
        self.ab_loop = (None, None);
        self.sub_delay = 0.0;
        let old = self.frame.take();
        self.retire_frame(old);
        self.subs.clear();
        if let MediaSource::Dvd { path, title: 0, .. } = &item.source
            && !self.dvd_menus_on()
        {
            self.open_dvd_auto(path.clone(), window, cx);
            return;
        }
        if let MediaSource::Cd { device, track: 0 } = &item.source {
            self.open_audio_cd(device.clone(), idx, paused, window, cx);
            return;
        }
        self.load_counter += 1;
        // A new load drops whatever was queued in the engine.
        self.armed = None;
        if load {
            let start_at = restored_position.unwrap_or_else(|| {
                if self.settings.resume_playback { self.resume.get(&item.source.key()) } else { None }
            });
            self.player.load(item.source.clone(), LoadOptions { start_at, start_paused: paused, ..Default::default() });
        }
        match &item.source {
            // One entry for the whole disc, not one per track.
            MediaSource::Cd { device, .. } => {
                self.settings.add_recent(&MediaSource::Cd { device: device.clone(), track: 0 })
            }
            src => self.settings.add_recent(src),
        }
        window.set_window_title(&format!("{} — MavPlayer", item.display_title()));
        // The title list (for the DVD Titles menu); a disc played with its
        // menus has its own, and reading it meanwhile would make the drive seek.
        if let MediaSource::Dvd { path, title, .. } = &item.source
            && (*title > 0 || !self.dvd_menus_on())
            && self.dvd_titles.as_ref().map(|(p, _)| p != path).unwrap_or(true)
        {
            self.probe_dvd_titles(path.clone(), None, window, cx);
        }
        cx.notify();
    }

    /// Reloads the current item at the current position (after settings
    /// that need a fresh decoder, like hardware decoding).
    pub fn reload_current(&mut self, cx: &mut Context<Self>) {
        let st = self.player.state();
        let Some(src) = st.source else { return };
        self.thumbnails.waveform.reset();
        self.reset_thumbnails();
        let pos = self.player.position();
        let paused = self.player.paused();
        // A DVD played with its menus resumes its title, not the first-play program.
        let dvd_title = st.dvd.as_ref().filter(|d| d.domain == DvdDomain::Title && d.title > 0).map(|d| d.title);
        self.player.set_config(self.engine_config());
        self.player
            .load(src, LoadOptions { start_at: Some(pos), dvd_title, start_paused: paused, ..Default::default() });
        cx.notify();
    }

    pub fn open_sources(&mut self, sources: Vec<MediaSource>, window: &mut Window, cx: &mut Context<Self>) {
        if sources.is_empty() {
            self.osd("No playable files found", cx);
            return;
        }
        let append = self.settings.open_mode == crate::settings::OpenMode::Append && !self.playlist.items.is_empty();
        if !append {
            self.save_current_position();
            self.playlist.clear();
        }
        let range = self.playlist.add(sources);
        self.probe_playlist(cx);
        self.play_index(range.start, window, cx);
    }

    /// Media sent by another launch while this window is open.
    pub fn open_remote(&mut self, req: crate::instance::Request, window: &mut Window, cx: &mut Context<Self>) {
        use crate::settings::RunningMode;
        self.show_window(window, cx);
        if req.sources.is_empty() {
            return;
        }
        match req.mode {
            RunningMode::Enqueue => self.enqueue(req.sources, window, cx),
            RunningMode::Replace => {
                self.save_current_position();
                self.playlist.clear();
                self.playlist_selection.clear();
                self.playlist_hovered = None;
                let range = self.playlist.add(req.sources);
                self.probe_playlist(cx);
                self.play_index(range.start, window, cx);
            }
            RunningMode::PlayNow | RunningMode::NewWindow => {
                let range = self.playlist.add(req.sources);
                self.probe_playlist(cx);
                self.play_index(range.start, window, cx);
            }
        }
    }

    /// Adds without interrupting playback.
    pub fn enqueue(&mut self, sources: Vec<MediaSource>, window: &mut Window, cx: &mut Context<Self>) {
        self.enqueue_at(self.playlist.items.len(), sources, window, cx);
    }

    /// Inserts into the playlist before index `at` without interrupting
    /// playback (starts playing if nothing is).
    pub fn enqueue_at(&mut self, at: usize, sources: Vec<MediaSource>, window: &mut Window, cx: &mut Context<Self>) {
        if sources.is_empty() {
            return;
        }
        let was_empty = self.playlist.items.is_empty();
        let n = sources.len();
        let range = self.playlist.insert(at, sources);
        self.probe_playlist(cx);
        if was_empty || !self.player.is_active() {
            self.play_index(range.start, window, cx);
        } else {
            self.osd(format!("Added {n} item(s) to the playlist"), cx);
        }
    }

    pub fn open_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        // A lone subtitle file dropped onto a playing video loads as subtitles.
        if paths.len() == 1 && self.player.is_active() {
            let ext = paths[0].extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            if engine_sub_ext(&ext) {
                self.player.add_subtitle_file(&paths[0]);
                self.remember_subtitles(true);
                self.osd(format!("Subtitles: {}", paths[0].file_name().unwrap_or_default().to_string_lossy()), cx);
                return;
            }
        }
        if let Some(d) = paths.first().and_then(|p| p.parent()) {
            self.settings.last_dir = Some(d.to_path_buf());
        }
        let sources = playlist::expand_paths(&paths);
        self.open_sources(sources, window, cx);
    }

    fn probe_playlist(&mut self, cx: &mut Context<Self>) {
        let todo: Vec<(u64, MediaSource)> = self
            .playlist
            .items
            .iter_mut()
            .filter(|i| !i.probed && matches!(i.source, MediaSource::File(_)))
            .map(|i| {
                i.probed = true;
                (i.id, i.source.clone())
            })
            .collect();
        if todo.is_empty() {
            return;
        }
        // A few workers (probing is mostly waiting on the disk), with results
        // shown as they arrive rather than after the whole list.
        let cfg = self.engine_config();
        // One probe per source in this batch, even when it occurs multiple times.
        let mut jobs: Vec<(Vec<u64>, MediaSource)> = Vec::new();
        let mut indices = HashMap::new();
        for (id, source) in todo {
            let index = *indices.entry(source.clone()).or_insert_with(|| {
                jobs.push((Vec::new(), source));
                jobs.len() - 1
            });
            jobs[index].0.push(id);
        }
        drop(indices);
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4));
        let executor = cx.background_executor().clone();
        cx.background_executor()
            .spawn(async move {
                // Publish all available local cache records before any source
                // checks. A slow or disconnected NAS must not delay browsing
                // cached entries elsewhere in the list.
                for (ids, source) in &jobs {
                    if let Some(result) = engine::probe::cached_playlist(source, &cfg) {
                        for &id in ids {
                            if tx.unbounded_send((id, Some(result.clone()))).is_err() {
                                return;
                            }
                        }
                    }
                }
                let queue = Arc::new(parking_lot::Mutex::new(VecDeque::from(jobs)));
                for _ in 0..workers {
                    let (queue, tx, cfg) = (queue.clone(), tx.clone(), cfg.clone());
                    executor
                        .spawn(async move {
                            loop {
                                let job = queue.lock().pop_front();
                                let Some((ids, source)) = job else { break };
                                let result = engine::probe::probe_playlist(&source, &cfg);
                                for id in ids {
                                    if tx.unbounded_send((id, result.clone())).is_err() {
                                        return; // the window closed
                                    }
                                }
                            }
                        })
                        .detach();
                }
            })
            .detach();
        cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(r) = rx.try_recv() {
                    batch.push(r);
                }
                let applied = this.update(cx, |v, cx| {
                    let state = v.player.state();
                    let loaded_id = v
                        .playlist
                        .current
                        .and_then(|i| v.playlist.items.get(i))
                        .filter(|it| state.media.is_some() && state.source.as_ref() == Some(&it.source))
                        .map(|it| it.id);
                    for (id, r) in batch {
                        if let (Some(it), Some(r)) = (v.playlist.items.iter_mut().find(|i| i.id == id), r) {
                            // The live engine is authoritative for the loaded item.
                            // Other items may replace provisional cached values.
                            if loaded_id != Some(id) {
                                it.duration = Some(r.duration).filter(|d| *d > 0.0);
                                it.video_stream = r.video_stream;
                                it.audio_stream = r.audio_stream;
                                it.has_subtitles = r.has_subtitles;
                                it.title = r.title;
                                it.artist = r.artist;
                            } else if r.source_validated {
                                it.has_subtitles = r.has_subtitles
                                    || state.media.as_ref().is_some_and(|media| {
                                        media.tracks.iter().any(|t| t.kind == TrackKind::Subtitle)
                                    });
                            } else {
                                it.has_subtitles |= r.has_subtitles;
                            }
                            if r.source_validated
                                && let MediaSource::File(path) = &it.source
                                && let Some(stream) = it.audio_stream
                                && it.duration.is_some_and(|d| d.is_finite() && d > 0.0)
                                && (if it.video_stream.is_some() {
                                    v.settings.waveform_auto_video
                                } else {
                                    v.settings.waveform_auto_audio
                                })
                            {
                                v.thumbnails.waveform.enqueue(path.clone(), Some(stream), &v.settings, false);
                            }
                            if r.source_validated
                                && let MediaSource::File(path) = &it.source
                                && let Some(stream) = it.video_stream
                                && it.duration.is_some_and(|d| d.is_finite() && d > 0.0)
                            {
                                if v.settings.timeline_thumbnails && v.settings.filmstrip_auto_video {
                                    v.playlist_previews.generate_missing(path.clone(), Some(stream));
                                } else if v.settings.playlist_thumbnail_auto_video
                                    && v.settings.playlist_thumbnails.inline()
                                {
                                    // Only the inline playlist modes show this picture.
                                    v.playlist_previews.generate_representative(path.clone(), Some(stream));
                                }
                            }
                        }
                    }
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn enqueue_paths_at(
        &mut self,
        at: usize,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(d) = paths.first().and_then(|p| p.parent()) {
            self.settings.last_dir = Some(d.to_path_buf());
        }
        let sources = playlist::expand_playlist_paths(&paths);
        if sources.is_empty() {
            if !paths.is_empty() {
                self.osd("No supported media files", cx);
            }
            return;
        }
        self.enqueue_at(at, sources, window, cx);
    }

    pub(super) fn prompt_playlist_add(&mut self, dirs_mode: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_playlist_add_paths(dirs_mode, false, window, cx);
    }

    pub(super) fn prompt_playlist_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_playlist_add_paths(false, true, window, cx);
    }

    fn prompt_playlist_add_paths(
        &mut self, dirs_mode: bool, playlists_only: bool, window: &mut Window, cx: &mut Context<Self>,
    ) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: !dirs_mode,
            directories: dirs_mode,
            multiple: true,
            prompt: Some(if playlists_only { "Import playlist from file" } else if dirs_mode { "Add folder" } else { "Add files" }.into()),
        });
        cx.spawn_in(window, async move |this, cx| match rx.await {
            Ok(Ok(Some(mut paths))) => {
                let _ = this.update_in(cx, |v, window, cx| {
                    if playlists_only {
                        paths.retain(|p| p.extension().is_some_and(|e| {
                            playlist::PLAYLIST_EXTS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str())
                        }));
                        if paths.is_empty() {
                            v.osd("No supported playlist files", cx);
                            return;
                        }
                    }
                    v.enqueue_paths_at(v.playlist.items.len(), paths, window, cx);
                });
            }
            Ok(Err(e)) => {
                let _ = this.update(cx, |v, cx| v.osd(format!("File dialog failed: {e}"), cx));
            }
            _ => {}
        })
        .detach();
    }

    pub fn prompt_open(&mut self, dirs_mode: bool, subtitle: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: !dirs_mode,
            directories: dirs_mode,
            multiple: !subtitle,
            prompt: Some(if subtitle { "Load Subtitles" } else { "Open" }.into()),
        });
        cx.spawn_in(window, async move |this, cx| match rx.await {
            Ok(Ok(Some(paths))) if !paths.is_empty() => {
                let _ = this.update_in(cx, |v, window, cx| {
                    if subtitle {
                        v.player.add_subtitle_file(&paths[0]);
                        v.remember_subtitles(true);
                    } else {
                        v.open_paths(paths, window, cx);
                    }
                });
            }
            Ok(Err(e)) => {
                let _ = this.update(cx, |v, cx| v.osd(format!("File dialog failed: {e}"), cx));
            }
            _ => {}
        })
        .detach();
    }

    /// Opens a disc: reads the title list first so the main feature plays.
    pub fn open_dvd_auto(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.reading_disc = true;
        self.player.stop();
        self.osd("Reading disc…", cx);
        self.probe_dvd_titles(path, Some(()), window, cx);
    }

    fn probe_dvd_titles(&mut self, path: String, then_play: Option<()>, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.engine_config();
        let p = path.clone();
        let task = cx.background_executor().spawn(async move { disc::probe_titles(&p, &cfg, 99) });
        cx.spawn_in(window, async move |this, cx| {
            let titles = task.await;
            let _ = this.update_in(cx, |v, window, cx| {
                let main = disc::main_title(&titles);
                v.dvd_titles = Some((path.clone(), titles));
                if then_play.is_some() {
                    v.reading_disc = false;
                    match main {
                        Some(t) => v.play_dvd_title(path, t, window, cx),
                        None => v.osd("Could not read the disc (no titles found)", cx),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A whole DVD (title 0) starts at its own menu, unless this build
    /// can't show menus: then it plays the main feature.
    pub fn dvd_menus_on(&self) -> bool {
        engine::dvd_menus_available()
    }

    /// A DVD playing with its menus shows buttons: arrow keys, Enter and
    /// the mouse go to them.
    pub fn dvd_buttons(&self) -> bool {
        self.player.with_state(|s| s.dvd.as_ref().is_some_and(|d| d.takes_input()))
    }

    /// Shows one of the disc's menus. A DVD playing title by title is
    /// reopened with its menus first.
    pub fn dvd_menu(&mut self, menu: DvdMenu, window: &mut Window, cx: &mut Context<Self>) {
        if self.player.with_state(|s| s.dvd.is_some()) {
            let ok = self.player.dvd_command(DvdCommand::Menu(menu))
                || (menu == DvdMenu::Root && self.player.dvd_command(DvdCommand::Menu(DvdMenu::Title)));
            if !ok {
                self.osd("The disc doesn't offer that menu here", cx);
            }
            return;
        }
        let Some(MediaSource::Dvd { path, .. }) = self.player.with_state(|s| s.source.clone()) else {
            self.osd("Disc menus are for DVDs", cx);
            return;
        };
        if !engine::dvd_menus_available() {
            self.osd("This build of MavPlayer can't show DVD menus", cx);
            return;
        }
        self.save_current_position();
        let src = MediaSource::Dvd { path, title: 0, angle: 1 };
        if let Some(i) = self.playlist.current {
            self.playlist.items[i].source = src.clone();
            self.playlist.items[i].title = None;
        }
        self.player.load(src, LoadOptions::default());
        self.load_counter += 1;
        cx.notify();
        let _ = window;
    }

    /// Sends a remote-control command to a DVD playing with its menus.
    pub fn dvd(&mut self, cmd: DvdCommand, cx: &mut Context<Self>) {
        if self.player.dvd_command(cmd) {
            cx.notify();
        }
    }

    /// A point in the window → the DVD picture's own pixels (for buttons).
    pub fn dvd_point(&self, p: gpui::Point<Pixels>) -> Option<(f32, f32)> {
        let rect = super::render::video_rect(self.video_bounds.get(), self.frame_size, self.aspect, self.zoom);
        let (cw, ch) = self.player.subtitle_canvas();
        let (w, h) = (f32::from(rect.size.width), f32::from(rect.size.height));
        if cw == 0 || ch == 0 || w <= 0.0 || h <= 0.0 {
            return None;
        }
        let (x, y) = (f32::from(p.x - rect.origin.x) / w, f32::from(p.y - rect.origin.y) / h);
        ((0.0..1.0).contains(&x) && (0.0..1.0).contains(&y)).then_some((x * cw as f32, y * ch as f32))
    }

    /// Reads an audio CD's track list and puts one playlist item per track
    /// in place of item `idx` (the whole disc), then plays the first.
    pub fn open_audio_cd(
        &mut self,
        device: String,
        idx: usize,
        paused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reading_disc = true;
        self.player.stop();
        self.osd("Reading CD…", cx);
        let d = device.clone();
        let task = cx.background_executor().spawn(async move { engine::cdda::read_toc(&d) });
        cx.spawn_in(window, async move |this, cx| {
            let toc = task.await;
            let _ = this.update_in(cx, |v, window, cx| {
                v.reading_disc = false;
                let toc = match toc {
                    Ok(t) => t,
                    Err(e) => {
                        if let Some(it) = v.playlist.items.get_mut(idx) {
                            it.failed = true;
                        }
                        v.osd(format!("Could not read the CD: {e}"), cx);
                        cx.notify();
                        return;
                    }
                };
                v.play_cd_tracks(device, &toc, Some(idx), 0, paused, window, cx);
            });
        })
        .detach();
    }

    /// Puts an audio CD's tracks in the playlist (in place of item `replace`,
    /// or replacing everything) and plays track number `first` (0-based).
    #[allow(clippy::too_many_arguments)]
    pub fn play_cd_tracks(
        &mut self,
        device: String,
        toc: &engine::cdda::CdToc,
        replace: Option<usize>,
        first: usize,
        paused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sources: Vec<MediaSource> =
            toc.tracks.iter().map(|t| MediaSource::Cd { device: device.clone(), track: t.number }).collect();
        if sources.is_empty() {
            self.osd("The CD has no audio tracks", cx);
            return;
        }
        let at = match replace.filter(|&i| i < self.playlist.items.len()) {
            Some(i) => {
                self.playlist.remove(i);
                i
            }
            None => {
                self.save_current_position();
                self.playlist.clear();
                0
            }
        };
        let range = self.playlist.insert(at, sources);
        for (it, t) in self.playlist.items[range.clone()].iter_mut().zip(&toc.tracks) {
            it.title = Some(t.title.clone().unwrap_or_else(|| format!("Track {}", t.number)));
            it.artist = t.artist.clone();
            it.duration = Some(t.duration());
            it.probed = true;
        }
        self.start_item(range.start + first.min(range.len() - 1), true, paused, window, cx);
    }

    pub fn play_dvd_title(&mut self, path: String, title: u32, window: &mut Window, cx: &mut Context<Self>) {
        let src = MediaSource::Dvd { path, title, angle: 1 };
        let idx = self.playlist.current.filter(|&i| matches!(self.playlist.items[i].source, MediaSource::Dvd { .. }));
        match idx {
            Some(i) => {
                self.playlist.items[i].source = src.clone();
                self.playlist.items[i].title = Some(format!("DVD – Title {title}"));
                let start_at = if self.settings.resume_playback { self.resume.get(&src.key()) } else { None };
                let start_paused = self.settings.start_paused;
                self.player.load(src.clone(), LoadOptions { start_at, start_paused, ..Default::default() });
                self.settings.add_recent(&src);
                self.load_counter += 1;
                window.set_window_title(&format!("{} — MavPlayer", self.playlist.items[i].display_title()));
            }
            None => {
                self.playlist.clear();
                self.playlist.add(vec![src]);
                self.play_index(0, window, cx);
            }
        }
        cx.notify();
    }

    fn fit_window_if_needed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Hidden in the tray: fit once shown again (`show_window`).
        if self.fitted_for == Some(self.load_counter) || self.hidden_to_tray {
            return;
        }
        self.fitted_for = Some(self.load_counter);
        if self.settings.fit_window_to_video {
            self.fit_window(true, window, cx);
        }
    }

    /// Sizes the window to the video (capped to the screen). Opening a file
    /// only grows it (`grow_only`): a small video leaves a larger window as it
    /// is. The Fit Window to Video command fits it exactly, shrinking too.
    pub fn fit_window(&mut self, grow_only: bool, window: &mut Window, cx: &mut Context<Self>) {
        if window.is_fullscreen() || window.is_maximized() {
            return;
        }
        let Some(v) = self.player.with_state(|s| s.video.clone()) else { return };
        if self.player.with_state(|s| s.media.as_ref().map(|m| !m.has_video()).unwrap_or(true)) {
            return;
        }
        let aspect = match self.aspect {
            Aspect::Ratio(r, _) => r,
            _ => v.display_aspect,
        };
        if v.height <= 0 || !aspect.is_finite() {
            return;
        }
        let scale = window.scale_factor() as f64;
        let mut vh = v.height as f64 / scale;
        let mut vw = vh * aspect;
        // Wayland only reports the window's output once it has been entered, and
        // never a "primary" one; fall back to the smallest known output.
        let screen = window
            .display(cx)
            .or_else(|| cx.primary_display())
            .map(|d| d.bounds().size)
            .or_else(|| {
                cx.displays()
                    .iter()
                    .map(|d| d.bounds().size)
                    .min_by(|a, b| a.height.partial_cmp(&b.height).unwrap_or(std::cmp::Ordering::Equal))
            })
            .unwrap_or(size(px(1920.), px(1080.)));
        let (sw, sh) = (f32::from(screen.width) as f64 * 0.85, f32::from(screen.height) as f64 * 0.85);
        let chrome_h =
            f32::from(window.viewport_size().height) as f64 - f32::from(self.video_bounds.get().size.height) as f64;
        let side = if self.show_playlist && !self.settings.playlist_autohide_for(false) { 320.0 } else { 0.0 };
        let k = ((sw - side) / vw).min((sh - chrome_h) / vh).min(1.0);
        vw *= k;
        vh *= k;
        // Never narrower than the bottom bar's controls need.
        let min_w = super::bar::min_window_width(&self.settings.bar_controls) as f64;
        let (mut w, mut h) = ((vw + side).max(480.0).max(min_w), (vh + chrome_h).max(320.0));
        if grow_only {
            let now = window.viewport_size();
            let (cw, ch) = (f32::from(now.width) as f64, f32::from(now.height) as f64);
            if w <= cw + 1.0 && h <= ch + 1.0 {
                return;
            }
            w = w.max(cw);
            h = h.max(ch);
        }
        let want = size(px(w as f32), px(h as f32));
        window.resize(want);
        self.pending_fit = Some((want, 4));
    }

    pub fn is_fullscreen(&self, window: &Window) -> bool {
        window.is_fullscreen()
    }

    pub(super) fn playlist_autohide(&self) -> bool {
        self.settings.playlist_autohide_for(self.in_fullscreen)
    }

    pub(super) fn set_playlist_autohide(&mut self, enabled: bool) {
        self.settings.set_playlist_autohide_for(self.in_fullscreen, enabled);
        self.playlist_autohide_open = false;
        self.playlist_previews.hover = None;
        self.playlist_hovered = None;
    }

    /// Whether the playlist is enabled, even when temporarily autohidden.
    pub(super) fn playlist_enabled(&self) -> bool {
        if self.in_fullscreen { self.fs_playlist } else { self.show_playlist }
    }

    pub fn playlist_shown(&self) -> bool {
        self.playlist_enabled() && (!self.playlist_autohide() || self.playlist_autohide_open)
    }

    pub(super) fn update_playlist_autohide(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.playlist_autohide() || !self.playlist_enabled() {
            return;
        }
        if self.menu.is_some()
            || self.dialog.is_some()
            || self.drag.is_some()
            || self.scrub.is_some()
            || cx.has_active_drag()
            || super::widgets::scrollbar_dragging()
        {
            return;
        }
        let hovered = window.is_window_hovered();
        if window.is_window_active()
            && self.playlist_filter_open
            && self.inputs.get("playlist_filter").is_some_and(|input| input.focus_handle(cx).is_focused(window))
        {
            return;
        }
        let viewport = window.viewport_size();
        let bounds = self.playlist_bounds.get();
        let pos = window.mouse_position();
        let open = hovered
            && if self.playlist_autohide_open && bounds.size.width > px(0.) {
                bounds.contains(&pos)
            } else {
                pos.x >= viewport.width - px(16.)
                    && pos.x <= viewport.width
                    && pos.y >= px(0.)
                    && pos.y <= viewport.height
            };
        if open != self.playlist_autohide_open {
            self.playlist_autohide_open = open;
            if !open {
                self.playlist_previews.hover = None;
                self.playlist_hovered = None;
            }
            cx.notify();
        }
    }

    pub fn toggle_playlist(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.playlist_previews.hover = None;
        self.track_fullscreen(window);
        if self.in_fullscreen {
            self.fs_playlist = !self.fs_playlist;
        } else {
            self.show_playlist = !self.show_playlist;
            self.settings.set_playlist_visibility(false, self.show_playlist);
        }
        self.playlist_autohide_open = self.playlist_enabled();
        cx.notify();
    }

    /// Called every render: reset the transient reveal state when switching modes.
    pub fn track_fullscreen(&mut self, window: &Window) {
        let fs = window.is_fullscreen();
        if fs != self.in_fullscreen {
            self.in_fullscreen = fs;
            self.playlist_autohide_open = false;
            if fs {
                self.fs_playlist = self.settings.fullscreen_playlist_initially_visible();
            }
        }
    }

    pub fn input(
        &mut self,
        key: &'static str,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextInput> {
        if let Some(e) = self.inputs.get(key) {
            return e.clone();
        }
        let ph = placeholder.to_string();
        let e = cx.new(|cx| TextInput::new(cx, ph));
        let sub = cx.subscribe_in(&e, window, move |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Submit => this.on_input_submit(key, window, cx),
            InputEvent::Cancel if key == "volume" => this.finish_volume_edit(false, window, cx),
            InputEvent::Cancel if key == "playlist_switch" => {
                this.close_menu(cx);
                window.focus(&this.playlist_focus);
            }
            InputEvent::Cancel if key == "playlist_filter" => {
                this.playlist_filter_open = false;
                window.focus(&this.playlist_focus);
                cx.notify();
            }
            InputEvent::Cancel => this.close_dialog(window, cx),
            InputEvent::Changed => this.on_input_changed(key, cx),
        });
        self._subs.push(sub);
        if key == "playlist_filter" {
            let focus = e.focus_handle(cx);
            self._subs.push(cx.on_focus(&focus, window, |_, _, cx| cx.notify()));
            self._subs.push(cx.on_blur(&focus, window, |_, _, cx| cx.notify()));
        }
        self.inputs.insert(key, e.clone());
        e
    }

    /// Swaps the volume percentage for a text field (double-click on it).
    pub fn start_volume_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input("volume", "%", window, cx);
        let t = self.theme;
        let pct = format!("{:.0}", self.player.volume() * 100.0);
        input.update(cx, |i, cx| {
            i.colors = (t.text, t.text_disabled, t.accent_soft, t.accent);
            i.set_text(pct, cx);
        });
        let fh = input.focus_handle(cx);
        window.focus(&fh);
        self.editing_volume = true;
        // Clicking elsewhere applies what was typed.
        self.volume_blur = Some(cx.on_blur(&fh, window, |this, window, cx| this.finish_volume_edit(true, window, cx)));
        cx.notify();
    }

    /// Ends volume editing; `apply` sets the typed percentage (Enter / blur),
    /// otherwise it is discarded (Escape).
    pub fn finish_volume_edit(&mut self, apply: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing_volume {
            return;
        }
        self.editing_volume = false;
        self.volume_blur = None;
        if apply {
            let text = self.inputs.get("volume").map(|i| i.read(cx).text()).unwrap_or_default();
            match parse_volume(&text) {
                Some(pct) => self.set_volume(pct / 100.0, cx),
                None => self.osd("Type a volume like 80 or 80%", cx),
            }
        }
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            self.end_drag(window, cx);
        }
        self.modal_drag_origin = None;
        self.close_menu(cx);

        if let Some(Dialog::Integrate) = self.dialog {
            let _ = std::fs::create_dir_all(crate::settings::config_dir());
            let _ = std::fs::write(crate::integration::prompt_marker(), b"asked");
        }
        self.capturing_key = None;
        if self.dialog.take().is_some() {
            self.settings.save();
        }
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn engine_config(&self) -> crate::engine::EngineConfig {
        self.settings.engine_config()
    }

    pub fn apply_settings(&mut self, cx: &mut Context<Self>) {
        self.update_thumbnails(self.in_fullscreen);
        self.player.set_config(self.engine_config());
        icons::set_use_system_theme(self.settings.icon_theme == crate::settings::IconTheme::System);
        self.settings.save();
        cx.notify();
    }

    // ───────────────────────── track helpers ─────────────────────────

    pub fn select_track(&mut self, kind: TrackKind, id: Option<TrackId>, cx: &mut Context<Self>) {
        self.player.select_track(kind, id);
        if kind == TrackKind::Audio
            && let Some(item) = self.playlist.current.and_then(|i| self.playlist.items.get_mut(i))
        {
            item.audio_stream = match id {
                Some(TrackId::Stream(i)) => Some(i),
                _ => item.audio_stream,
            };
            self.cache_badges.invalidate();
        }
        let label = self.track_label(kind, id);
        let what = match kind {
            TrackKind::Audio => "Audio",
            TrackKind::Subtitle => "Subtitles",
            TrackKind::Video => "Video",
        };
        if kind == TrackKind::Subtitle {
            // Picking a track turns subtitles on, "Off" turns them off.
            self.remember_subtitles(id.is_some());
        }
        self.osd(format!("{what}: {label}"), cx);
    }

    pub fn track_label(&self, kind: TrackKind, id: Option<TrackId>) -> String {
        let Some(id) = id else { return "Off".into() };
        self.player.with_state(|s| {
            s.media
                .as_ref()
                .and_then(|m| m.tracks_of(kind).enumerate().find(|(_, t)| t.id == id).map(|(n, t)| t.label(n + 1)))
                .unwrap_or_else(|| "?".into())
        })
    }

    /// Subtitles are on (the user's choice, kept across files and launches).
    /// A chosen track is hidden while they are off.
    /// Identifies the subtitles on screen now (what `video_area` shows).
    fn subtitle_signature(&mut self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut subs = std::mem::take(&mut self.subs);
        subs.clear();
        let dvd_nav = self.player.with_state(|s| s.dvd.is_some());
        if (self.subs_visible() || dvd_nav) && self.player.is_active() {
            self.player.subtitles_at(self.player.position() - self.sub_delay, &mut subs);
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for ev in &subs {
            (ev.start.to_bits(), ev.end.to_bits()).hash(&mut h);
            match &ev.content {
                crate::engine::SubContent::Text(t) => t.hash(&mut h),
                crate::engine::SubContent::Bitmap(b) => (Arc::as_ptr(b) as usize).hash(&mut h),
            }
        }
        self.subs = subs;
        h.finish()
    }

    pub fn subs_visible(&self) -> bool {
        self.settings.show_subtitles
    }

    /// Subtitles are on screen (a track is chosen and not hidden).
    pub fn subs_shown(&self) -> bool {
        self.subs_visible() && self.player.with_state(|s| s.selected.subtitle.is_some())
    }

    /// The subtitle track to show when the user asks for subtitles.
    fn best_subtitle(&self) -> Option<TrackId> {
        let langs = self.settings.engine_config().sub_langs;
        self.player.with_state(|s| s.media.as_ref().and_then(|m| engine::pick_subtitle(m, &langs)))
    }

    /// Records the user's subtitle on/off choice: saved right away, and the
    /// engine starts later files (and the one queued next) with it.
    fn remember_subtitles(&mut self, on: bool) {
        if self.settings.show_subtitles == on {
            return;
        }
        self.settings.show_subtitles = on;
        self.player.set_config(self.engine_config());
        self.settings.save();
    }

    /// With subtitles on, a file that started without a track (it was queued
    /// before they were turned on, or its subtitle file loaded later) gets
    /// the best one.
    pub fn ensure_subtitle(&mut self) {
        if !self.subs_visible() || self.player.with_state(|s| s.selected.subtitle.is_some()) {
            return;
        }
        if let Some(id) = self.best_subtitle() {
            self.player.select_track(TrackKind::Subtitle, Some(id));
        }
    }

    /// Turns subtitles on or off (Preferences): the same as the subtitle button.
    pub fn set_show_subtitles(&mut self, on: bool) {
        self.remember_subtitles(on);
        self.ensure_subtitle();
    }

    /// Show / hide subtitles. Showing picks the best track by itself when
    /// none is chosen yet, and later files and launches follow the choice.
    fn toggle_subtitles(&mut self, cx: &mut Context<Self>) {
        let cur = self.player.with_state(|s| s.selected.subtitle);
        if self.subs_visible() && cur.is_some() {
            self.remember_subtitles(false);
            // A DVD with menus always draws its pictures (buttons, forced
            // subtitles): deselect rather than hide.
            if self.player.with_state(|s| s.dvd.is_some()) {
                self.player.select_track(TrackKind::Subtitle, None);
            }
            self.osd("Subtitles off", cx);
            return;
        }
        let Some(id) = cur.or_else(|| self.best_subtitle()) else {
            // Nothing to show here, but later files come with subtitles.
            self.remember_subtitles(true);
            self.osd("No subtitles", cx);
            return;
        };
        if cur.is_none() {
            self.select_track(TrackKind::Subtitle, Some(id), cx);
        } else {
            self.remember_subtitles(true);
            let label = self.track_label(TrackKind::Subtitle, Some(id));
            self.osd(format!("Subtitles: {label}"), cx);
        }
    }

    /// Next (`step` 1) or previous (-1) track. Subtitles move between
    /// languages; when they are off, this turns them on with the best one.
    fn cycle_track(&mut self, kind: TrackKind, step: isize, cx: &mut Context<Self>) {
        let (ids, cur) = self.player.with_state(|s| {
            let ids: Vec<TrackId> = s
                .media
                .as_ref()
                .map(|m| m.tracks_of(kind).filter(|t| !t.is_cover_art).map(|t| t.id).collect())
                .unwrap_or_default();
            let cur = match kind {
                TrackKind::Audio => s.selected.audio,
                TrackKind::Subtitle => s.selected.subtitle,
                TrackKind::Video => s.selected.video,
            };
            (ids, cur)
        });
        if ids.is_empty() {
            self.osd(if kind == TrackKind::Audio { "No audio tracks" } else { "No subtitles" }, cx);
            return;
        }
        let pos = ids.iter().position(|i| Some(*i) == cur);
        let next = match pos {
            Some(_) if kind == TrackKind::Subtitle && !self.subs_visible() => cur,
            Some(p) => Some(ids[(p as isize + step).rem_euclid(ids.len() as isize) as usize]),
            None if kind == TrackKind::Subtitle => self.best_subtitle().or(Some(ids[0])),
            None => Some(ids[0]),
        };
        self.select_track(kind, next, cx);
    }

    pub fn set_volume(&mut self, v: f64, cx: &mut Context<Self>) {
        // Media-key clients (MPRIS) can send anything, NaN included.
        if !v.is_finite() {
            return;
        }
        let max = self.settings.max_volume as f64 / 100.0;
        let v = v.clamp(0.0, max);
        self.player.set_volume(v);
        self.player.set_muted(false);
        self.settings.volume = v;
        self.osd(format!("Volume {:.0}%", v * 100.0), cx);
    }

    pub fn set_speed(&mut self, s: f64, cx: &mut Context<Self>) {
        self.player.set_speed(s);
        self.osd(format!("Speed {}", super::widgets::format_speed(self.player.speed())), cx);
    }

    pub fn set_video_adjust(&mut self, adj: VideoAdjust, cx: &mut Context<Self>) {
        self.video_adjust = adj.clone();
        self.player.set_video_adjust(adj);
        cx.notify();
    }

    pub fn seek_relative(&mut self, secs: f64, precise: bool, cx: &mut Context<Self>) {
        if !self.player.is_active() {
            return;
        }
        self.player.seek(secs, true, precise || self.settings.exact_seeks);
        let target = (self.player.position()).max(0.0);
        let d = self.player.duration();
        self.osd(format!("{} / {}", super::widgets::format_time(target), super::widgets::format_time(d)), cx);
        cx.notify();
    }

    /// The position the progress bar and time show: under the pointer while
    /// the bar is dragged and the video hasn't caught up (keyframe seeks lag
    /// and land near it), otherwise the playback position, which keeps
    /// moving while the bar is held still.
    pub fn shown_position(&self) -> f64 {
        match &self.scrub {
            Some(s) if s.sought != Some((s.frac, true)) => s.frac as f64 * self.player.duration(),
            _ => self.player.position(),
        }
    }

    /// The progress bar was pressed at `f`: seeks there (exactly) and, if
    /// set to, pauses until it is released.
    pub fn scrub_start(&mut self, f: f32, cx: &mut Context<Self>) {
        let pause = self.settings.seek_drag == crate::settings::SeekDragMode::Pause && !self.player.paused();
        if pause {
            self.player.set_paused(true);
        }
        let now = Instant::now();
        self.scrub = Some(Scrub {
            frac: f,
            sought: None,
            last_seek: now,
            seek_scheduled: false,
            last_move: now,
            settle_scheduled: false,
            resume: pause,
        });
        self.seek_hover = Some(f);
        self.seek_fraction(f as f64, cx);
        if let Some(s) = self.scrub.as_mut() {
            s.sought = Some((f, true));
        }
    }

    /// The pointer moved while holding the bar. The bar follows at once;
    /// the video follows with fast keyframe seeks, at most one per
    /// [`SCRUB_INTERVAL`], and lands exactly under the pointer once it
    /// rests for [`SCRUB_SETTLE`].
    pub fn scrub_move(&mut self, f: f32, cx: &mut Context<Self>) {
        let Some(s) = self.scrub.as_mut() else { return };
        if s.frac == f {
            return;
        }
        s.frac = f;
        s.last_move = Instant::now();
        self.seek_hover = Some(f);
        // Snippets from every position passed would stutter: keep quiet
        // until the pointer rests.
        self.player.set_hushed(true);
        self.scrub_seek(cx);
        self.scrub_settle(cx);
        cx.notify();
    }

    fn scrub_settle(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.scrub.as_mut() else { return };
        if s.settle_scheduled || s.sought == Some((s.frac, true)) {
            return;
        }
        let wait = SCRUB_SETTLE.saturating_sub(s.last_move.elapsed());
        if wait.is_zero() {
            s.sought = Some((s.frac, true));
            s.last_seek = Instant::now();
            let f = s.frac as f64;
            self.seek_fraction(f, cx);
            self.player.set_hushed(false);
            return;
        }
        s.settle_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |v, cx| {
                if let Some(s) = v.scrub.as_mut() {
                    s.settle_scheduled = false;
                }
                v.scrub_settle(cx);
            });
        })
        .detach();
    }

    fn scrub_seek(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.scrub.as_mut() else { return };
        if s.sought.is_some_and(|(at, _)| at == s.frac) {
            return;
        }
        let wait = SCRUB_INTERVAL.saturating_sub(s.last_seek.elapsed());
        if wait.is_zero() {
            s.last_seek = Instant::now();
            s.sought = Some((s.frac, false));
            let target = self.player.duration() * s.frac as f64;
            self.player.seek(target, false, false);
        } else if !s.seek_scheduled {
            s.seek_scheduled = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |v, cx| {
                    if let Some(s) = v.scrub.as_mut() {
                        s.seek_scheduled = false;
                    }
                    v.scrub_seek(cx);
                });
            })
            .detach();
        }
    }

    /// The bar was released at `f`: lands exactly there (unless the press
    /// already did) and resumes playback if the drag paused it.
    pub fn scrub_end(&mut self, f: f32, cx: &mut Context<Self>) {
        let Some(s) = self.scrub.take() else { return };
        if s.sought != Some((f, true)) {
            self.seek_fraction(f as f64, cx);
        }
        self.player.set_hushed(false);
        if s.resume && self.player.paused() {
            self.player.set_paused(false);
        }
        self.seek_hover = None;
        cx.notify();
    }

    pub fn seek_fraction(&mut self, f: f64, cx: &mut Context<Self>) {
        let d = self.player.duration();
        if d > 0.0 {
            self.player.seek(d * f.clamp(0.0, 1.0), false, true);
            cx.notify();
        }
    }

    pub fn next_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.playlist.next_index(true) {
            self.play_index(n, window, cx);
        } else {
            self.osd("End of playlist", cx);
        }
    }

    pub fn prev_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Like most players: restart the current item unless near its start.
        if self.player.position() > 5.0 {
            self.player.seek(0.0, false, true);
            return;
        }
        if let Some(p) = self.playlist.prev_index() {
            self.play_index(p, window, cx);
        }
    }

    pub fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_playlist_session(true, false);
        self.thumbnails.waveform.reset();
        self.reset_thumbnails();
        self.save_current_position();
        self.player.stop();
        let old = self.frame.take();
        self.retire_frame(old);
        self.subs.clear();
        window.set_window_title("MavPlayer");
        cx.notify();
    }

    pub fn chapter_jump(&mut self, forward: bool, cx: &mut Context<Self>) {
        let chapters = self.player.with_state(|s| s.media.as_ref().map(|m| m.chapters.clone()).unwrap_or_default());
        if chapters.is_empty() {
            let step = self.settings.seek_step_large;
            self.seek_relative(if forward { step } else { -step }, false, cx);
            return;
        }
        let pos = self.player.position();
        let target = if forward {
            chapters.iter().find(|c| c.start > pos + 0.5)
        } else {
            chapters.iter().rev().find(|c| c.start < pos - 2.0).or(chapters.first())
        };
        if let Some(c) = target {
            let (start, title) = (c.start, c.title.clone());
            self.player.seek(start, false, true);
            self.osd(format!("Chapter: {title}"), cx);
        }
    }

    pub fn cycle_repeat(&mut self, cx: &mut Context<Self>) {
        self.playlist.repeat = match self.playlist.repeat {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        };
        self.settings.repeat = self.playlist.repeat;
        let s = match self.playlist.repeat {
            RepeatMode::Off => "Repeat: off",
            RepeatMode::All => "Repeat: all",
            RepeatMode::One => "Repeat: current item",
        };
        self.osd(s, cx);
    }

    pub fn screenshot(&mut self, cx: &mut Context<Self>) {
        if !self.player.has_frame() {
            self.osd("No picture to capture", cx);
            return;
        }
        let name = self
            .player
            .with_state(|s| s.source.as_ref().map(|x| x.display_name()))
            .unwrap_or_else(|| "mavplayer".into());
        let stem = Path::new(&name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(name);
        let t = self.player.position();
        let ms = (t * 1000.0) as u64;
        let file = format!(
            "{}_{:02}-{:02}-{:02}.{:03}.png",
            stem.replace(['/', '\\', ':'], "_"),
            ms / 3_600_000,
            (ms / 60_000) % 60,
            (ms / 1000) % 60,
            ms % 1000
        );
        self.player.screenshot(self.settings.screenshot_dir().join(file));
    }

    // ───────────────────────── tray ─────────────────────────

    /// "Play in the background" is on and there is a tray icon to come back from.
    pub fn can_hide_to_tray(&self, cx: &App) -> bool {
        self.settings.tray_playback && super::shell::tray_up(cx)
    }

    /// Closing the window hides it instead: a file is open (playing or paused).
    fn hide_on_close(&self, cx: &App) -> bool {
        self.can_hide_to_tray(cx)
            && self.player.is_active()
            && self.player.with_state(|s| matches!(s.status, Status::Playing | Status::Paused | Status::Loading))
    }

    /// Hides the window to the tray. Playback, media keys and the playlist
    /// carry on; the tray icon's menu controls them and brings it back.
    pub fn hide_to_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.hidden_to_tray {
            return;
        }
        if !self.can_hide_to_tray(cx) {
            let msg = if self.settings.tray_playback {
                "No system tray to hide in"
            } else {
                "Turn on “Play in the background” in Preferences → Playback first"
            };
            self.osd(msg, cx);
            return;
        }
        // Saved now, in case the session ends while nothing can be seen.
        self.save_state();
        self.close_menu(cx);
        if window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        window.set_window_visible(false);
        self.hidden_to_tray = true;
        // Nothing will be drawn: don't convert pictures meanwhile.
        self.video_hidden = true;
        self.player.set_video_hidden(true);
        log::debug!("ui: window hidden to the tray");
        self.update_tray(cx);
    }

    /// Brings the window back from the tray (if hidden) and to the front.
    pub fn show_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_window_with(None, window, cx);
    }

    /// `show_window`, with the activation token of the click that asked for
    /// it (from the tray host), without which a Wayland compositor may leave
    /// the window behind the one in front.
    fn show_window_with(&mut self, token: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.hidden_to_tray {
            self.hidden_to_tray = false;
            window.set_window_visible(true);
            // The next render turns picture conversion back on.
            log::debug!("ui: window shown from the tray");
            self.update_tray(cx);
            self.fit_window_if_needed(window, cx);
            cx.notify();
        }
        match token {
            Some(t) => window.activate_window_with_token(&t),
            None => window.activate_window(),
        }
    }

    /// The window is the one in front, as far as a click on the tray icon
    /// goes. A panel may take the keyboard focus as the icon is pressed
    /// (Plasma on Wayland does), before the click reaches us: a window that
    /// had it a moment ago counts too.
    fn in_front(&self, window: &Window) -> bool {
        window.is_window_active() || self.deactivated_at.is_some_and(|t| t.elapsed() < Duration::from_secs(1))
    }

    /// A click on the tray icon or its menu.
    pub fn on_tray(&mut self, ev: TrayEvent, window: &mut Window, cx: &mut Context<Self>) {
        use souvlaki::MediaControlEvent as Event;
        // Taken whatever the click does: a token works only once, and soon.
        let token = super::tray::take_activation_token();
        match ev {
            // The icon toggles a window that is in front.
            TrayEvent::Activate if !self.hidden_to_tray && self.in_front(window) && self.can_hide_to_tray(cx) => {
                self.hide_to_tray(window, cx)
            }
            TrayEvent::Activate | TrayEvent::Show => self.show_window_with(token, window, cx),
            TrayEvent::Hide => self.hide_to_tray(window, cx),
            TrayEvent::PlayPause => self.on_media_event(Event::Toggle, window, cx),
            TrayEvent::Previous => self.on_media_event(Event::Previous, window, cx),
            TrayEvent::Next => self.on_media_event(Event::Next, window, cx),
            // Handled by the shell.
            TrayEvent::Quit => {}
        }
        self.update_tray(cx);
        cx.notify();
    }

    /// Tells the tray icon's menu what is playing.
    fn update_tray(&self, cx: &mut App) {
        let (status, paused) = (self.player.with_state(|s| s.status.clone()), self.player.paused());
        let state = TrayState {
            controls: self.settings.tray_playback,
            playing: self.player.is_active() && !paused && matches!(status, Status::Playing | Status::Loading),
            has_media: !self.playlist.items.is_empty(),
            title: self.playlist.current.and_then(|i| self.playlist.items.get(i)).map(|it| it.display_title()),
            hidden: self.hidden_to_tray,
        };
        super::shell::set_tray_state(state, cx);
    }

    // ───────────────────────── action handlers ─────────────────────────

    pub fn register_actions<E: gpui::InteractiveElement>(&self, el: E, cx: &mut Context<Self>) -> E {
        macro_rules! on {
            ($el:expr, $($action:ident => |$this:ident, $w:ident, $cx:ident| $body:expr),* $(,)?) => {
                $el$(.on_action(cx.listener(move |$this: &mut Self, _: &$action, $w: &mut Window, $cx: &mut Context<Self>| { $body; $cx.notify(); })))*
            };
        }
        let step = self.settings.seek_step;
        let big = self.settings.seek_step_large;
        let vstep = self.settings.volume_step / 100.0;
        on!(el,
            TogglePause => |this, _w, _cx| this.player.toggle_pause(),
            DiscUp => |this, _w, cx| this.dvd(DvdCommand::Up, cx),
            DiscDown => |this, _w, cx| this.dvd(DvdCommand::Down, cx),
            DiscLeft => |this, _w, cx| this.dvd(DvdCommand::Left, cx),
            DiscRight => |this, _w, cx| this.dvd(DvdCommand::Right, cx),
            DiscActivate => |this, _w, cx| this.dvd(DvdCommand::Activate, cx),
            DiscMenu => |this, w, cx| this.dvd_menu(DvdMenu::Root, w, cx),
            DiscTitleMenu => |this, w, cx| this.dvd_menu(DvdMenu::Title, w, cx),
            Stop => |this, w, cx| this.stop(w, cx),
            PlayNext => |this, w, cx| this.next_item(w, cx),
            PlayPrevious => |this, w, cx| this.prev_item(w, cx),
            SeekForward => |this, _w, cx| this.seek_relative(step, false, cx),
            SeekBackward => |this, _w, cx| this.seek_relative(-step, false, cx),
            SeekForwardSmall => |this, _w, cx| this.seek_relative(1.0, true, cx),
            SeekBackwardSmall => |this, _w, cx| this.seek_relative(-1.0, true, cx),
            SeekForwardLarge => |this, _w, cx| this.seek_relative(big, false, cx),
            SeekBackwardLarge => |this, _w, cx| this.seek_relative(-big, false, cx),
            NextChapter => |this, _w, cx| this.chapter_jump(true, cx),
            PreviousChapter => |this, _w, cx| this.chapter_jump(false, cx),
            FrameStep => |this, _w, _cx| this.player.frame_step(true),
            FrameBackStep => |this, _w, _cx| this.player.frame_step(false),
            VolumeUp => |this, _w, cx| { let v = this.player.volume() + vstep; this.set_volume(v, cx) },
            VolumeDown => |this, _w, cx| { let v = this.player.volume() - vstep; this.set_volume(v, cx) },
            ToggleMute => |this, _w, cx| {
                let m = !this.player.muted();
                this.player.set_muted(m);
                this.settings.muted = m;
                this.osd(if m { "Muted" } else { "Unmuted" }, cx)
            },
            SpeedUp => |this, _w, cx| {
                let s = this.player.speed();
                let n = SPEEDS.iter().copied().find(|&x| x > s + 1e-3).unwrap_or(4.0);
                this.set_speed(n, cx)
            },
            SpeedDown => |this, _w, cx| {
                let s = this.player.speed();
                let n = SPEEDS.iter().rev().copied().find(|&x| x < s - 1e-3).unwrap_or(0.25);
                this.set_speed(n, cx)
            },
            SpeedReset => |this, _w, cx| this.set_speed(1.0, cx),
            CycleAudio => |this, _w, cx| this.cycle_track(TrackKind::Audio, 1, cx),
            CycleAudioBack => |this, _w, cx| this.cycle_track(TrackKind::Audio, -1, cx),
            CycleSubtitle => |this, _w, cx| this.cycle_track(TrackKind::Subtitle, 1, cx),
            CycleSubtitleBack => |this, _w, cx| this.cycle_track(TrackKind::Subtitle, -1, cx),
            ToggleSubtitles => |this, _w, cx| this.toggle_subtitles(cx),
            SubDelayIncrease => |this, _w, cx| { this.sub_delay += 0.1; let d = this.sub_delay; this.osd(format!("Subtitle delay {:+.0} ms", d * 1000.0), cx) },
            SubDelayDecrease => |this, _w, cx| { this.sub_delay -= 0.1; let d = this.sub_delay; this.osd(format!("Subtitle delay {:+.0} ms", d * 1000.0), cx) },
            AudioDelayIncrease => |this, _w, cx| { let d = this.player.audio_delay() + 0.1; this.player.set_audio_delay(d); this.osd(format!("Audio delay {:+.0} ms", d * 1000.0), cx) },
            AudioDelayDecrease => |this, _w, cx| { let d = this.player.audio_delay() - 0.1; this.player.set_audio_delay(d); this.osd(format!("Audio delay {:+.0} ms", d * 1000.0), cx) },
            SubScaleUp => |this, _w, cx| { this.settings.sub_scale = (this.settings.sub_scale + 0.1).min(3.0); let s = this.settings.sub_scale; this.osd(format!("Subtitle size {:.0}%", s * 100.0), cx) },
            SubScaleDown => |this, _w, cx| { this.settings.sub_scale = (this.settings.sub_scale - 0.1).max(0.3); let s = this.settings.sub_scale; this.osd(format!("Subtitle size {:.0}%", s * 100.0), cx) },
            ToggleFullscreen => |_this, w, _cx| w.toggle_fullscreen(),
            ExitFullscreen => |this, w, cx| {
                if this.menu.is_some() { this.close_menu(cx) }
                else if this.dialog.is_some() { this.close_dialog(w, cx) }
                else if w.is_fullscreen() { w.toggle_fullscreen() }
            },
            Screenshot => |this, _w, cx| this.screenshot(cx),
            ShowMediaInfo => |this, w, cx| {
                if this.dialog == Some(Dialog::MediaInfo) { this.close_dialog(w, cx) }
                else { this.show_dialog(Dialog::MediaInfo, w, cx) }
            },
            ToggleAbLoop => |this, _w, cx| {
                let pos = this.player.position();
                this.ab_loop = match this.ab_loop {
                    (None, _) => (Some(pos), None),
                    (Some(a), None) if pos > a + 0.1 => (Some(a), Some(pos)),
                    _ => (None, None),
                };
                let msg = match this.ab_loop {
                    (Some(a), None) => format!("Loop A: {}", super::widgets::format_time(a)),
                    (Some(a), Some(b)) => format!("Loop {} → {}", super::widgets::format_time(a), super::widgets::format_time(b)),
                    _ => "A-B loop cleared".into(),
                };
                this.osd(msg, cx)
            },
            ToggleLoopFile => |this, _w, cx| { this.loop_file = !this.loop_file; let l = this.loop_file; this.osd(if l { "Loop file: on" } else { "Loop file: off" }, cx) },
            CycleRepeat => |this, _w, cx| this.cycle_repeat(cx),
            ToggleShuffle => |this, _w, cx| {
                let s = !this.playlist.shuffle();
                this.playlist.set_shuffle(s);
                this.settings.shuffle = s;
                this.osd(if s { "Shuffle: on" } else { "Shuffle: off" }, cx)
            },
            OpenFile => |this, w, cx| this.prompt_open(false, false, w, cx),
            OpenFolder => |this, w, cx| this.prompt_open(true, false, w, cx),
            OpenUrl => |this, w, cx| this.show_dialog(Dialog::OpenUrl, w, cx),
            OpenDisc => |this, w, cx| this.show_dialog(Dialog::OpenDisc, w, cx),
            OpenSubtitle => |this, w, cx| {
                if this.player.is_active() { this.prompt_open(false, true, w, cx) } else { this.osd("Open a video first", cx) }
            },
            SavePlaylist => |this, _w, cx| this.save_playlist_workspace(cx),
            ExportPlaylist => |this, w, cx| this.export_playlist(w, cx),
            ShowPreferences => |this, w, cx| this.show_dialog(Dialog::Settings(0), w, cx),
            ShowEffects => |this, w, cx| this.show_dialog(Dialog::Effects(0), w, cx),
            TogglePlaylist => |this, w, cx| this.toggle_playlist(w, cx),
            PlaylistFocus => |this, w, cx| this.playlist_focus_list(w, cx),
            PlaylistFocusPlayer => |this, w, _cx| w.focus(&this.focus),
            PlaylistUp => |this, _w, cx| this.playlist_key_move(-1, false, cx),
            PlaylistDown => |this, _w, cx| this.playlist_key_move(1, false, cx),
            PlaylistExtendUp => |this, _w, cx| this.playlist_key_move(-1, true, cx),
            PlaylistExtendDown => |this, _w, cx| this.playlist_key_move(1, true, cx),
            PlaylistExtendHome => |this, _w, cx| this.playlist_key_move(isize::MIN, true, cx),
            PlaylistExtendEnd => |this, _w, cx| this.playlist_key_move(isize::MAX, true, cx),
            ComboUp => |this, _w, cx| this.combo_move(-1, cx),
            ComboDown => |this, _w, cx| this.combo_move(1, cx),
            ComboPageUp => |this, _w, cx| this.combo_move(-this.combo_page_step(), cx),
            ComboPageDown => |this, _w, cx| this.combo_move(this.combo_page_step(), cx),
            ComboHome => |this, _w, cx| this.combo_move(isize::MIN, cx),
            ComboEnd => |this, _w, cx| this.combo_move(isize::MAX, cx),
            PlaylistDelete => |this, w, cx| this.playlist_key_delete(w, cx),
            PlaylistPlay => |this, w, cx| this.playlist_key_play(w, cx),
            PlaylistSelectAll => |this, _w, cx| this.playlist_key_select_all(cx),
            PlaylistFilter => |this, w, cx| this.playlist_focus_filter(w, cx),
            ToggleMenubar => |this, _w, cx| {
                this.settings.show_menubar = !this.settings.show_menubar;
                this.settings.save();
                if !this.settings.show_menubar { let k = super::keymap::shortcut_for(&ToggleMenubar).map(|k| format!(" or press {k}")).unwrap_or_default();
                    this.osd(format!("Menu bar hidden — right-click{k} to bring it back"), cx)  }
            },
            JumpToTime => |this, w, cx| this.show_dialog(Dialog::JumpTo, w, cx),
            CycleAspect => |this, _w, cx| {
                let i = ASPECTS.iter().position(|a| *a == this.aspect).map(|i| (i + 1) % ASPECTS.len()).unwrap_or(0);
                this.aspect = ASPECTS[i];
                let l = this.aspect.label();
                this.osd(format!("Aspect ratio: {l}"), cx)
            },
            CycleDeinterlace => |this, _w, cx| {
                this.settings.deinterlace = match this.settings.deinterlace { Deinterlace::Auto => Deinterlace::On, Deinterlace::On => Deinterlace::Off, Deinterlace::Off => Deinterlace::Auto };
                this.apply_settings(cx);
                let s = format!("Deinterlacing: {:?}", this.settings.deinterlace);
                this.osd(s, cx)
            },
            ToggleHwdec => |this, _w, cx| {
                this.settings.hwdec = if this.settings.hwdec == HwDecode::Off { HwDecode::Always } else { HwDecode::Off };
                this.apply_settings(cx);
                this.reload_current(cx);
                let s = if this.settings.hwdec == HwDecode::Off { "Hardware decoding: off" } else { "Hardware decoding: on" };
                this.osd(s, cx)
            },
            RotateClockwise => |this, _w, cx| {
                let mut a = this.video_adjust.clone();
                a.rotation = match a.rotation { Rotation::None => Rotation::R90, Rotation::R90 => Rotation::R180, Rotation::R180 => Rotation::R270, Rotation::R270 => Rotation::None };
                let deg = match a.rotation { Rotation::None => 0, Rotation::R90 => 90, Rotation::R180 => 180, Rotation::R270 => 270 };
                this.set_video_adjust(a, cx);
                this.osd(format!("Rotation {deg}°"), cx)
            },
            ZoomIn => |this, _w, cx| { this.zoom = (this.zoom * 1.1).min(8.0); let z = this.zoom; this.osd(format!("Zoom {:.0}%", z * 100.0), cx) },
            ZoomOut => |this, _w, cx| { this.zoom = (this.zoom / 1.1).max(0.25); let z = this.zoom; this.osd(format!("Zoom {:.0}%", z * 100.0), cx) },
            ZoomReset => |this, _w, cx| { this.zoom = 1.0; this.aspect = Aspect::Auto; this.osd("Zoom reset", cx) },
            FitWindow => |this, w, cx| this.fit_window(false, w, cx),
            ShowShortcuts => |this, w, cx| this.show_dialog(Dialog::Settings(super::dialogs::SHORTCUTS_TAB), w, cx),
            ShowAbout => |this, w, cx| this.show_dialog(Dialog::About, w, cx),
            CloseOverlay => |this, w, cx| {
                if this.menu.is_some() && this.dialog.is_none() && !this.editing_volume {
                    this.close_menu(cx);
                } else {
                    this.close_menu(cx);
                    this.close_dialog(w, cx);
                }
            },
            Quit => |this, _w, cx| { this.save_state(); super::shell::quit(cx) },
            HideToTray => |this, w, cx| this.hide_to_tray(w, cx),
            Seek0 => |this, _w, cx| this.seek_fraction(0.0, cx),
            Seek1 => |this, _w, cx| this.seek_fraction(0.1, cx),
            Seek2 => |this, _w, cx| this.seek_fraction(0.2, cx),
            Seek3 => |this, _w, cx| this.seek_fraction(0.3, cx),
            Seek4 => |this, _w, cx| this.seek_fraction(0.4, cx),
            Seek5 => |this, _w, cx| this.seek_fraction(0.5, cx),
            Seek6 => |this, _w, cx| this.seek_fraction(0.6, cx),
            Seek7 => |this, _w, cx| this.seek_fraction(0.7, cx),
            Seek8 => |this, _w, cx| this.seek_fraction(0.8, cx),
            Seek9 => |this, _w, cx| this.seek_fraction(0.9, cx),
        )
    }

    pub fn export_playlist(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.playlist.items.is_empty() {
            self.osd("The playlist is empty", cx);
            return;
        }
        let dir = self.settings.last_dir.clone().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some("playlist.m3u"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let _ = this.update(cx, |v, cx| match v.playlist.save_m3u(&path) {
                    Ok(()) => v.osd(format!("Playlist exported: {}", path.display()), cx),
                    Err(e) => v.osd(format!("Could not export playlist: {e}"), cx),
                });
            }
        })
        .detach();
    }

    pub fn note_activity(&mut self, cx: &mut Context<Self>) {
        let was_hidden = !self.controls_visible();
        self.last_activity = Instant::now();
        if was_hidden {
            cx.notify();
        }
    }

    pub fn controls_visible(&self) -> bool {
        if self.in_fullscreen
            && self.menu.as_ref().is_some_and(|m| m.playlist_context)
            && self.player.is_active()
            && !self.player.paused()
        {
            return false;
        }
        self.last_activity.elapsed().as_secs_f64() < self.settings.controls_autohide_secs.max(0.5)
            || self.pointer_over_controls
            || self.menu.is_some()
            || self.dialog.is_some()
            || self.drag.is_some()
            || self.player.paused()
            || !self.player.is_active()
    }
}

fn engine_sub_ext(ext: &str) -> bool {
    matches!(ext, "srt" | "ass" | "ssa" | "vtt" | "sub" | "idx" | "smi" | "sup" | "ttml" | "lrc")
}

/// Parses a typed volume percentage: "80", "80%", " 72.5 % ".
pub fn parse_volume(text: &str) -> Option<f64> {
    let n: f64 = text.trim().trim_end_matches('%').trim().parse().ok()?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::parse_volume;

    #[test]
    fn volume_parsing() {
        assert_eq!(parse_volume("80"), Some(80.0));
        assert_eq!(parse_volume(" 72.5 % "), Some(72.5));
        assert_eq!(parse_volume("120%"), Some(120.0));
        assert_eq!(parse_volume("-5"), None);
        assert_eq!(parse_volume("loud"), None);
        assert_eq!(parse_volume(""), None);
    }
}
