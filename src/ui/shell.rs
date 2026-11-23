//! Everything around the player window that lasts as long as the process:
//! the window itself, the tray icon, and media handed over by later launches
//! or the OS.
//!
//! With "Keep running in the system tray" on, closing the window doesn't end
//! MavPlayer: a process waits in the tray with no window and no playback. The
//! next thing opened (from a file manager, the command line or the tray icon)
//! gets a window from that warm process in a few tens of milliseconds: its
//! libraries, fonts, GPU context and hardware decoder are already loaded.
//!
//! GPUI ends its event loop when the last window closes on Linux and Windows,
//! so there the closing process starts a fresh `mavplayer --background` and
//! quits. That also keeps the waiting process lean: it never played anything
//! (about 100 MB of private memory on NVIDIA, mostly the GPU driver and
//! GPUI's font database). The hand-over (`finish_handover`):
//! 1. The new process shows its tray icon first thing. On Linux it takes over
//!    the old icon's D-Bus name, so Plasma never sees the icon go (see
//!    `tray.rs`). It reports "ready" on its stdout pipe.
//! 2. The old process frees the instance socket's name, closes the new one's
//!    stdin, removes its icon and exits.
//! 3. The new process, seeing its stdin close, listens on the socket straight
//!    away and brings up GPUI.
//!
//! On macOS the app simply stays running without windows.
//!
//! "Play in the background" is the other way to the tray: closing the window
//! while a file is open hides it instead (`PlayerView::hide_to_tray`), and
//! the process plays on with its window open but not shown. The tray icon's
//! menu then has playback controls (`TrayState`), and its Show entry brings
//! the same window back.

use super::player::{Boot, PlayerView};
use super::tray::{Tray, TrayEvent, TrayState};
use crate::engine::MediaSource;
use crate::settings::Settings;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use gpui::{
    App, AppContext, Bounds, Global, TitlebarOptions, WindowBounds, WindowDecorations, WindowHandle, WindowOptions, px,
    size,
};

/// Something for the shell to act on, from outside the window.
pub enum Incoming {
    /// Media (or just "show yourself") from a later launch; see `instance`.
    Remote(crate::instance::Request),
    /// Files or URLs the OS asks us to open (macOS Finder and dock).
    Urls(Vec<String>),
    Tray(TrayEvent),
    /// The dock icon was clicked (macOS).
    Reopen,
}

pub type Sender = UnboundedSender<Incoming>;

pub fn channel() -> (Sender, UnboundedReceiver<Incoming>) {
    futures::channel::mpsc::unbounded()
}

/// How this process starts.
#[allow(clippy::large_enum_variant)] // made once, at startup
pub enum Start {
    /// With a window (a normal launch).
    Window { boot: Boot, fullscreen: bool },
    /// In the tray, without a window (`--background`), with the tray icon
    /// if `main` already set it up.
    Background { settings: Settings, tray: Option<Tray> },
}

impl Start {
    pub fn settings(&self) -> &Settings {
        match self {
            Start::Window { boot, .. } => &boot.settings,
            Start::Background { settings, .. } => settings,
        }
    }
}

struct Shell {
    window: Option<WindowHandle<PlayerView>>,
    /// Later launches reach this process (it owns the instance socket).
    /// Only that one may wait in the tray: nothing could reach another.
    primary: bool,
    tx: Sender,
    tray: Option<Tray>,
    /// The settings ask for a tray icon.
    want_tray: bool,
    /// "Keep running in the system tray": once the window closes, a process
    /// waits in the tray (not just "Play in the background").
    wait_in_tray: bool,
    /// What the tray icon's menu shows (for an icon set up later, too).
    tray_state: TrayState,
    /// A tray icon is being set up.
    tray_starting: bool,
    /// Quitting for good (Quit chosen): don't wait in the tray.
    quitting: bool,
}

impl Global for Shell {}

/// Opens the first window (or the tray icon) and handles `rx`. Called from `app.run`.
pub fn run(cx: &mut App, start: Start, primary: bool, tx: Sender, mut rx: UnboundedReceiver<Incoming>) {
    let settings = start.settings().clone();
    cx.set_global(Shell {
        window: None,
        primary,
        tx,
        tray: None,
        want_tray: false,
        wait_in_tray: settings.keep_in_tray,
        tray_state: TrayState::default(),
        tray_starting: false,
        quitting: false,
    });
    match start {
        Start::Window { boot, fullscreen } => {
            if !open_window(boot, fullscreen, cx) {
                cx.quit();
                return;
            }
            if settings.wants_tray() {
                set_tray(&settings, false, cx);
            }
        }
        Start::Background { tray, .. } => {
            crate::instance::set_background(true);
            match tray {
                Some(tray) => {
                    let shell = cx.global_mut::<Shell>();
                    shell.tray = Some(tray);
                    shell.want_tray = true;
                    shell.wait_in_tray = true;
                }
                // Without a tray icon there would be no way to reach or quit this process.
                None => start_tray(false, true, cx),
            }
            log::debug!("startup: ready in the background at {:.0} ms", crate::ms_since_start());
        }
    }
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            last_window_closed(cx);
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Some(msg) = rx.next().await {
            if cx.update(|cx| handle(msg, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Quits MavPlayer for good, tray included (the Quit command).
pub fn quit(cx: &mut App) {
    if cx.has_global::<Shell>() {
        cx.global_mut::<Shell>().quitting = true;
    }
    crate::instance::stop_accepting();
    cx.quit();
}

/// Opens the player window for `boot`. Returns false if it couldn't.
fn open_window(boot: Boot, fullscreen: bool, cx: &mut App) -> bool {
    let fullscreen = fullscreen || boot.settings.start_fullscreen;
    let (w, h) = boot.settings.window_size.unwrap_or((960.0, 600.0));
    // Wide enough for every control chosen for the bottom bar. (GPUI can't
    // change it later; the bar leaves controls out if it must.)
    let min_w = super::bar::min_window_width(&boot.settings.bar_controls);
    let bounds = Bounds::centered(None, size(px(w.max(480.0).max(min_w)), px(h.max(320.0))), cx);
    let options = WindowOptions {
        window_bounds: Some(if fullscreen { WindowBounds::Fullscreen(bounds) } else { WindowBounds::Windowed(bounds) }),
        titlebar: Some(TitlebarOptions {
            title: Some("MavPlayer".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        app_id: Some(crate::integration::APP_ID.to_string()),
        window_min_size: Some(size(px(min_w), px(280.))),
        window_decorations: Some(WindowDecorations::Server),
        focus: true,
        show: true,
        ..Default::default()
    };
    log::debug!("startup: opening window at {:.0} ms", crate::ms_since_start());
    match cx.open_window(options, move |window, cx| cx.new(|cx| PlayerView::new(window, cx, boot))) {
        Ok(handle) => {
            cx.global_mut::<Shell>().window = Some(handle);
            crate::instance::set_background(false);
            cx.activate(true);
            log::debug!("startup: window open at {:.0} ms", crate::ms_since_start());
            true
        }
        Err(e) => {
            eprintln!("mavplayer: could not open a window: {e:#}");
            false
        }
    }
}

/// Opens the window from the background, playing `sources`.
fn reopen(sources: Vec<MediaSource>, fullscreen: bool, cx: &mut App) {
    crate::mark_launch();
    log::debug!("opening the window from the tray with {} item(s)", sources.len());
    let primary = cx.global::<Shell>().primary;
    let boot = Boot::start(Settings::load(), sources, primary);
    if !open_window(boot, fullscreen, cx) {
        quit(cx);
    }
}

/// The open player window, if any.
fn window(cx: &App) -> Option<WindowHandle<PlayerView>> {
    let w = cx.global::<Shell>().window?;
    cx.windows().iter().any(|h| h.window_id() == w.window_id()).then_some(w)
}

fn handle(msg: Incoming, cx: &mut App) {
    let open = window(cx);
    match msg {
        Incoming::Remote(req) => match open {
            Some(w) => {
                let _ = w.update(cx, |v, window, cx| v.open_remote(req, window, cx));
            }
            None => reopen(req.sources, req.fullscreen, cx),
        },
        Incoming::Urls(urls) => {
            let sources: Vec<MediaSource> = urls.iter().map(|u| crate::playlist::source_for_arg(u)).collect();
            match open {
                Some(w) => {
                    let _ = w.update(cx, |v, window, cx| {
                        v.show_window(window, cx);
                        v.open_sources(sources, window, cx)
                    });
                }
                None => reopen(sources, false, cx),
            }
        }
        Incoming::Tray(TrayEvent::Quit) => {
            if let Some(w) = open {
                let _ = w.update(cx, |v, _, _| v.save_state());
            }
            quit(cx);
        }
        // Waiting in the tray without a window: the menu only offers Open
        // (and Quit), so anything else opens the window too.
        Incoming::Tray(TrayEvent::Show | TrayEvent::Activate) | Incoming::Reopen if open.is_none() => {
            reopen(Vec::new(), false, cx)
        }
        Incoming::Tray(ev) => {
            if let Some(w) = open {
                let _ = w.update(cx, |v, window, cx| v.on_tray(ev, window, cx));
            }
        }
        Incoming::Reopen => {
            if let Some(w) = open {
                let _ = w.update(cx, |v, window, cx| v.show_window(window, cx));
            }
        }
    }
}

/// Waits in the tray when its icon is up; quits otherwise.
fn last_window_closed(cx: &mut App) {
    let shell = cx.global_mut::<Shell>();
    shell.window = None;
    let stay = shell.tray.is_some() && shell.wait_in_tray && shell.primary && !shell.quitting;
    if !stay {
        quit(cx);
        return;
    }
    #[cfg(target_os = "macos")]
    {
        crate::instance::set_background(true);
        log::debug!("window closed; waiting in the menu bar");
    }
    #[cfg(not(target_os = "macos"))]
    {
        // GPUI is ending its event loop: hand over to a fresh process.
        crate::instance::stop_accepting();
        match spawn_background() {
            Ok(child) => {
                log::debug!("window closed; a new MavPlayer takes over in the system tray");
                // Keep the icon up past the end of GPUI (which drops the
                // shell) until the new process shows its own.
                let tray = cx.global_mut::<Shell>().tray.take();
                HANDOVER.with(|h| *h.borrow_mut() = Some((tray, child)));
            }
            Err(e) => log::warn!("could not keep running in the system tray: {e:#}"),
        }
        cx.quit();
    }
}

#[cfg(not(target_os = "macos"))]
thread_local! {
    /// This process's tray icon and the process taking over from it.
    static HANDOVER: std::cell::RefCell<Option<(Option<Tray>, std::process::Child)>> =
        const { std::cell::RefCell::new(None) };
}

/// Line the process taking over writes once its tray icon is up.
const READY: &str = "mavplayer-tray-ready";

/// After GPUI has ended (from `main`): if a new process is taking over in
/// the tray, waits until its icon is up (at most a few seconds), then lets it
/// have the instance socket and removes this one's icon.
pub fn finish_handover() {
    #[cfg(not(target_os = "macos"))]
    if let Some((tray, mut child)) = HANDOVER.with(|h| h.borrow_mut().take()) {
        use std::io::BufRead;
        let t0 = std::time::Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                let ready = std::io::BufReader::new(out).lines().map_while(Result::ok).any(|l| l.trim() == READY);
                let _ = tx.send(ready);
            });
        }
        let ready = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or(false);
        log::debug!(
            "the new MavPlayer {} after {:.0} ms",
            if ready { "shows its tray icon" } else { "didn't report a tray icon" },
            t0.elapsed().as_secs_f64() * 1000.0
        );
        crate::instance::release();
        // Its cue to listen on the socket.
        drop(child.stdin.take());
        drop(tray);
        // `main` then exits at once (on Linux without the exit handlers).
    }
}

/// Tells the process handing over to this one that our tray icon is up
/// (`--handover`), then waits (briefly) for it to free the instance socket,
/// which it signals by closing our stdin.
pub fn report_ready() {
    use std::io::{Read, Write};
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{READY}");
    let _ = out.flush();
    drop(out);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = std::io::stdin().lock().read_to_end(&mut Vec::new());
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
    // The pipe's reader has exited: don't let a later write fail.
    #[cfg(target_os = "linux")]
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::dup2(null, 1);
            libc::close(null);
        }
    }
}

/// Starts `mavplayer --background --handover`, detached from this process
/// (and its terminal), with its standard output piped back for `READY` and
/// its standard input kept open until it may take the instance socket.
#[cfg(not(target_os = "macos"))]
fn spawn_background() -> anyhow::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    let mut cmd = if std::env::var_os("FLATPAK_ID").is_some() {
        // A process left in this sandbox would end with it: start a new one.
        let mut c = Command::new("flatpak-spawn");
        c.arg(&exe);
        c
    } else if let Some(appimage) = crate::integration::appimage_path() {
        // This AppImage's mount goes away when this process ends.
        Command::new(appimage)
    } else {
        Command::new(&exe)
    };
    cmd.args(["--background", "--handover"]).stdin(Stdio::piped()).stdout(Stdio::piped());
    if log::log_enabled!(log::Level::Debug) {
        cmd.arg("-v");
    } else {
        cmd.stderr(Stdio::null());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: closing the terminal this one ran in mustn't end it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    Ok(cmd.spawn()?)
}

/// The tray icon, sending its clicks to `tx`.
pub fn make_tray(tx: Sender) -> anyhow::Result<Tray> {
    Tray::new(move |ev| {
        let _ = tx.unbounded_send(Incoming::Tray(ev));
    })
}

/// Shows or removes the tray icon, following the "Keep running in the system
/// tray" and "Play in the background" settings. With `announce`, a desktop
/// without a tray is reported in the window (when one has just been turned on).
pub fn set_tray(settings: &Settings, announce: bool, cx: &mut App) {
    let on = settings.wants_tray();
    let shell = cx.global_mut::<Shell>();
    shell.want_tray = on;
    shell.wait_in_tray = settings.keep_in_tray;
    if !on {
        shell.tray = None;
        return;
    }
    if !shell.primary {
        if announce {
            notify("Another MavPlayer window is the one that stays in the tray", cx);
        }
        return;
    }
    start_tray(announce, false, cx);
}

/// Sets up the tray icon unless it is already there. With `required`, a
/// failure quits (a background process can't be reached without it).
fn start_tray(announce: bool, required: bool, cx: &mut App) {
    let shell = cx.global_mut::<Shell>();
    shell.want_tray = true;
    if shell.tray.is_some() || shell.tray_starting {
        return;
    }
    shell.tray_starting = true;
    let tx = shell.tx.clone();
    let make = move || make_tray(tx);
    cx.spawn(async move |cx| {
        // Registering over D-Bus can take a moment: keep it off the UI thread.
        #[cfg(target_os = "linux")]
        let tray = cx.background_executor().spawn(async move { make() }).await;
        // Windows and macOS need it on the main thread; let the window
        // appear first so it doesn't delay the first picture.
        #[cfg(not(target_os = "linux"))]
        let tray = {
            if !required {
                cx.background_executor().timer(std::time::Duration::from_millis(300)).await;
            }
            make()
        };
        let _ = cx.update(|cx| {
            let shell = cx.global_mut::<Shell>();
            shell.tray_starting = false;
            match tray {
                Ok(tray) if shell.want_tray => {
                    if shell.tray_state != TrayState::default() {
                        tray.set_state(shell.tray_state.clone());
                    }
                    shell.tray = Some(tray);
                }
                Ok(_) => {}
                Err(e) => {
                    log::warn!("no system tray: {e:#}");
                    if required {
                        quit(cx);
                    } else if announce {
                        notify("No system tray found: closing the window will quit", cx);
                    }
                }
            }
        });
    })
    .detach();
}

/// This process shows a tray icon, so a window hidden to the tray can be
/// brought back.
pub fn tray_up(cx: &App) -> bool {
    cx.try_global::<Shell>().is_some_and(|s| s.tray.is_some() && s.primary)
}

/// Updates the tray icon's menu and tooltip, if `state` is new.
pub fn set_tray_state(state: TrayState, cx: &mut App) {
    let Some(shell) = cx.try_global::<Shell>() else { return };
    if shell.tray_state == state {
        return;
    }
    let shell = cx.global_mut::<Shell>();
    if let Some(tray) = &shell.tray {
        tray.set_state(state.clone());
    }
    shell.tray_state = state;
}

fn notify(msg: &'static str, cx: &mut App) {
    if let Some(w) = window(cx) {
        let _ = w.update(cx, |v, _, cx| v.osd(msg, cx));
    }
}
