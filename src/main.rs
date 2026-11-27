//! MavPlayer — a fast, native media player built on FFmpeg and GPUI.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod engine;
#[cfg(target_os = "linux")]
mod font_prefetch;
mod fuzzy;
mod icons;
mod instance;
mod integration;
mod mem;
mod playlist;
mod settings;
mod ui;

use gpui::{App, Application};
use playlist::session::{CollectionRequest, Library};

/// The system allocator, keeping a few big blocks (video frames) for reuse.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[global_allocator]
static ALLOC: mem::Alloc = mem::Alloc;
use std::path::PathBuf;

const HELP: &str = "\
MavPlayer — play video, music, streams, DVDs and audio CDs

USAGE:
    mavplayer [OPTIONS] [FILE | FOLDER | URL | dvd://[TITLE][/DEVICE] | cdda://[TRACK][/DEVICE]] ...

    dvd:// starts at the disc's menu (title 0); dvd://TITLE plays one title.
    cdda:// plays every track of an audio CD; a .cue file plays a CD image.

OPTIONS:
    -f, --fullscreen          Start in fullscreen
    -n, --new-window          Open a new window even if MavPlayer is already open
        --collection=UUID     Open a specific collection in a separate window
        --read-only-collection=UUID
                              Open a read-only snapshot of a collection
        --temporary-collection
                              Open a temporary collection with crash recovery
        --list-sessions       List recoverable temporary session UUIDs
        --recover-session=UUID
                              Recover a temporary session in a separate window
        --list-collections    List collection UUIDs and names, then exit
        --create-collection=NAME
                              Create an empty collection, print its UUID, then exit
        --background          Start in the system tray without a window, so
                              the first file opened plays almost instantly
        --integrate           Add this AppImage to the desktop's application menu
        --remove-integration  Remove the application menu entry created by --integrate
    -v, --verbose             Print FFmpeg diagnostics to stderr
    -h, --help                Show this help
    -V, --version             Show the version
";

struct Args {
    sources: Vec<engine::MediaSource>,
    fullscreen: bool,
    new_window: bool,
    collection: CollectionRequest,
    /// Wait in the system tray without a window.
    background: bool,
    /// Taking over from a MavPlayer whose window just closed (internal).
    handover: bool,
    verbose: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        sources: vec![],
        fullscreen: false,
        new_window: false,
        collection: CollectionRequest::Default,
        background: false,
        handover: false,
        verbose: false,
    };
    let mut paths: Vec<PathBuf> = vec![];
    let mut only_files = false;
    for a in std::env::args().skip(1) {
        if !only_files && a.starts_with('-') && a.len() > 1 {
            match a.as_str() {
                "--" => only_files = true,
                "-h" | "--help" => {
                    print!("{HELP}");
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("mavplayer {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "-f" | "--fullscreen" => args.fullscreen = true,
                "-n" | "--new-window" => args.new_window = true,
                "--background" => args.background = true,
                "--handover" => args.handover = true,
                "-v" | "--verbose" => args.verbose = true,
                "--integrate" => exit_with(integration::integrate().map(|p| format!("Installed {}", p.display()))),
                "--remove-integration" | "--unintegrate" => {
                    exit_with(integration::unintegrate().map(|_| "Removed desktop integration".to_string()))
                }
                "--temporary-collection" => args.collection = CollectionRequest::Temporary,
                "--list-sessions" => exit_with(Library::list_sessions(&settings::config_dir()).map_err(Into::into)),
                _ if a.starts_with("--recover-session=") => {
                    let id = a["--recover-session=".len()..].parse().unwrap_or_else(|_| {
                        eprintln!("mavplayer: invalid session UUID");
                        std::process::exit(1);
                    });
                    args.collection = CollectionRequest::Recover(id);
                }
                "--list-collections" => exit_with(Library::list_collections(&settings::config_dir()).map_err(Into::into)),
                _ if a.starts_with("--create-collection=") => exit_with(
                    Library::create_collection(&settings::config_dir(), &a["--create-collection=".len()..])
                        .map(|id| id.to_string())
                        .map_err(Into::into),
                ),
                _ if a.starts_with("--collection=") || a.starts_with("--read-only-collection=") => {
                    let (option, value) = a.split_once('=').expect("collection option contains '='");
                    let id = value.parse().unwrap_or_else(|_| {
                        eprintln!("mavplayer: invalid collection UUID: {value}");
                        std::process::exit(1);
                    });
                    args.collection = if option == "--collection" {
                        CollectionRequest::Writable(id)
                    } else {
                        CollectionRequest::ReadOnly(id)
                    };
                }
                // Ignore unknown options passed by desktop launchers (e.g. -psn_… on macOS).
                other => eprintln!("mavplayer: ignoring unknown option {other}"),
            }
            continue;
        }
        if a.contains("://") {
            // Flush pending paths first so command-line order is preserved.
            args.sources.extend(playlist::expand_paths(&std::mem::take(&mut paths)));
            args.sources.push(playlist::source_for_arg(&a));
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    args.sources.extend(playlist::expand_paths(&paths));
    args
}

/// Minimal stderr logger (enabled by --verbose or MAVPLAYER_DEBUG).
struct StderrLog(std::time::Instant);

impl log::Log for StderrLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("mavplayer") && m.level() <= log::max_level()
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{:>8.3}s {:5}] {}", self.0.elapsed().as_secs_f64(), r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

static START: parking_lot::Mutex<Option<std::time::Instant>> = parking_lot::Mutex::new(None);

/// When this launch began: the process start, or the moment a MavPlayer
/// waiting in the tray started opening its window again.
pub fn launch_instant() -> Option<std::time::Instant> {
    *START.lock()
}

pub fn mark_launch() {
    *START.lock() = Some(std::time::Instant::now());
}

/// Milliseconds since launch, for the startup timings printed with `-v`.
pub fn ms_since_start() -> f64 {
    launch_instant().map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0)
}

fn exit_with(r: anyhow::Result<String>) -> ! {
    match r {
        Ok(msg) => {
            println!("{msg}");
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("mavplayer: {e:#}");
            std::process::exit(1)
        }
    }
}

/// Driver settings for the GPU work playback does, unless the user set them.
/// Call before any thread starts (it changes the environment).
fn gpu_env() {
    // CUDA gives each context eight hardware work queues by default, each
    // with its own command buffers in system memory. Decoding and resizing
    // use one stream, so one queue does: the context then holds 37 MB of
    // host memory instead of 84 MB (RTX 2070, driver 620), at no cost.
    if std::env::var_os("CUDA_DEVICE_MAX_CONNECTIONS").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("CUDA_DEVICE_MAX_CONNECTIONS", "1") };
    }
}

fn main() {
    mem::tune();
    gpu_env();
    let start = std::time::Instant::now();
    *START.lock() = Some(start);
    let args = parse_args();
    let verbose = args.verbose || std::env::var_os("MAVPLAYER_DEBUG").is_some();
    if verbose && log::set_boxed_logger(Box::new(StderrLog(start))).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    // Files on the command line win over --background.
    let separate_collection = args.collection.is_explicit();
    let background = (args.background || args.handover) && args.sources.is_empty() && !separate_collection;
    let handover = background && args.handover;

    let settings = settings::Settings::load();
    // Hand the files to an open window instead, if the settings say so. A
    // MavPlayer waiting in the tray takes any launch, files or not: opening
    // its window is much quicker than starting up.
    let hand_over =
        !args.sources.is_empty() && !args.new_window && settings.when_running != settings::RunningMode::NewWindow;
    if !background
        && !separate_collection
        && instance::forward(settings.when_running, &args.sources, args.fullscreen, !hand_over)
    {
        log::debug!("startup: handed {} item(s) to the running MavPlayer", args.sources.len());
        return;
    }
    engine::init(verbose);
    mem::start();

    // Requests from later launches, the OS and the tray icon, for the UI thread.
    let (tx, rx) = ui::shell::channel();
    // Taking over from a MavPlayer whose window just closed: show the tray
    // icon before anything else. That one removes its own only once ours is up.
    let tray = handover.then(|| ui::shell::make_tray(tx.clone()));
    if let Some(Err(e)) = &tray {
        log::warn!("no system tray: {e:#}");
    }
    let tray = tray.and_then(Result::ok);
    if handover {
        ui::shell::report_ready();
        log::debug!("startup: tray icon up at {:.0} ms", ms_since_start());
    }
    let primary = !separate_collection && {
        let tx = tx.clone();
        // That MavPlayer exits once our icon is up: wait for its socket.
        let wait = std::time::Duration::from_secs(if handover { 5 } else { 0 });
        instance::listen(wait, move |req| {
            let _ = tx.unbounded_send(ui::shell::Incoming::Remote(req));
        })
    };
    if background && !primary {
        log::debug!("startup: another MavPlayer is already running; nothing to do in the background");
        return;
    }
    // A collection owner is not necessarily the global settings owner. Only
    // the IPC primary may write global settings/resume data, and it must also
    // hold the OS lock (including across tray handovers).
    let global_writes = primary.then(settings::claim_global_writes).flatten();
    let settings = if global_writes.is_some() { settings::Settings::load() } else { settings };
    // GPUI's font scan in Application::new() is slow on a cold cache: warm it in parallel first.
    #[cfg(target_os = "linux")]
    let fonts = font_prefetch::start();
    let boot = if background {
        // No file yet, and no GPU devices for one: the hardware decoder alone
        // would more than double the memory this process sits in the tray with.
        // The first file opens them.
        ui::shell::Start::Background { settings, tray }
    } else {
        // Start opening the first file now: it decodes while GPUI brings up the GPU.
        ui::shell::Start::Window {
            boot: ui::player::Boot::start(settings, args.sources, primary, args.collection),
            fullscreen: args.fullscreen,
        }
    };
    integration::ensure_window_icon();
    #[cfg(target_os = "linux")]
    if let Some(fonts) = fonts {
        let _ = fonts.join();
    }
    let app = Application::new().with_assets(icons::Assets);
    log::debug!("startup: GPU and platform ready at {:.0} ms", ms_since_start());
    // Files opened from Finder / the dock while running (and URL schemes).
    {
        let tx = tx.clone();
        app.on_open_urls(move |urls| {
            let _ = tx.unbounded_send(ui::shell::Incoming::Urls(urls));
        });
    }
    // The dock icon clicked while MavPlayer waits in the background (macOS).
    {
        let tx = tx.clone();
        app.on_reopen(move |_| {
            let _ = tx.unbounded_send(ui::shell::Incoming::Reopen);
        });
    }
    app.run(move |cx: &mut App| {
        ui::keymap::apply(cx, &boot.settings().key_bindings);
        #[cfg(target_os = "macos")]
        ui::actions::set_native_menus(cx);
        // Quits when the last window closes (on every platform: a player has
        // no use windowless), unless it is to wait in the system tray.
        ui::shell::run(cx, boot, primary, tx, rx);
    });
    // All UI save callbacks have finished. Release global writes before the
    // tray successor takes the IPC socket and reloads settings.
    settings::release_global_writes(global_writes);
    // Hands the tray over to the process that waits in it, if one was started.
    ui::shell::finish_handover();
    // Leave without running the exit handlers. Engine threads may still be
    // freeing the last file's decoders, and CUDA's handler tears the driver
    // down under them: NVDEC's cleanup then crashes. Settings and resume
    // positions were saved when the window closed.
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        unsafe { libc::_exit(0) }
    }
}
