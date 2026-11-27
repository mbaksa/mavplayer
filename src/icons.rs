//! Icons.
//!
//! Every icon is referred to by its freedesktop name. On Linux the desktop's
//! own icon theme (KDE, GNOME, ...) is used so MavPlayer matches the rest of
//! the system; Papirus symbolic icons, embedded at build time from the
//! Papirus git submodule, are the fallback and the default elsewhere.

use gpui::{AssetSource, SharedString};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

include!(concat!(env!("OUT_DIR"), "/bundled_icons.rs"));

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Play,
    Pause,
    Stop,
    Previous,
    Next,
    SeekBack,
    SeekForward,
    VolumeHigh,
    VolumeMedium,
    VolumeLow,
    VolumeMuted,
    Fullscreen,
    ExitFullscreen,
    Playlist,
    Search,
    RepeatAll,
    RepeatOne,
    Shuffle,
    RepeatOff,
    Settings,
    Subtitles,
    WindowClose,
    WindowMinimize,
    WindowMaximize,
    WindowRestore,
    OpenFile,
    OpenFolder,
    Add,
    Remove,
    Clear,
    Disc,
    Url,
    Info,
    Audio,
    Video,
    Up,
    Down,
    Screenshot,
    ChevronRight,
    ChevronLeft,
    Check,
    DropDown,
    Menu,
    More,
    Save,
    Export,
    Delete,
    ZoomIn,
    ZoomOut,
    Reset,
    About,
    Speakers,
    Recent,
    Speed,
    AppGeneric,
    Eject,
}

impl Icon {
    pub fn freedesktop_name(self) -> &'static str {
        use Icon::*;
        match self {
            Play => "media-playback-start",
            Pause => "media-playback-pause",
            Stop => "media-playback-stop",
            Previous => "media-skip-backward",
            Next => "media-skip-forward",
            SeekBack => "media-seek-backward",
            SeekForward => "media-seek-forward",
            VolumeHigh => "audio-volume-high",
            VolumeMedium => "audio-volume-medium",
            VolumeLow => "audio-volume-low",
            VolumeMuted => "audio-volume-muted",
            Fullscreen => "view-fullscreen",
            ExitFullscreen => "view-restore",
            Playlist => "view-list",
            Search => "edit-find",
            RepeatAll => "media-playlist-repeat",
            RepeatOne => "media-playlist-repeat-song",
            Shuffle => "media-playlist-shuffle",
            RepeatOff => "media-playlist-consecutive",
            Settings => "preferences-system",
            Subtitles => "media-view-subtitles",
            WindowClose => "window-close",
            WindowMinimize => "window-minimize",
            WindowMaximize => "window-maximize",
            WindowRestore => "window-restore",
            OpenFile => "document-open",
            OpenFolder => "folder-open",
            Add => "list-add",
            Remove => "list-remove",
            Clear => "edit-clear-all",
            Disc => "media-optical-dvd",
            Url => "globe",
            Info => "dialog-information",
            Audio => "audio-x-generic",
            Video => "video-x-generic",
            Up => "go-up",
            Down => "go-down",
            Screenshot => "camera-photo",
            ChevronRight => "pan-end",
            ChevronLeft => "pan-start",
            Check => "object-select",
            DropDown => "pan-down",
            Menu => "open-menu",
            More => "view-more",
            Save => "document-save",
            Export => "document-export",
            Delete => "edit-delete",
            ZoomIn => "zoom-in",
            ZoomOut => "zoom-out",
            Reset => "view-refresh",
            About => "help-about",
            Speakers => "audio-speakers",
            Recent => "document-open-recent",
            Speed => "speedometer",
            AppGeneric => "multimedia-player",
            Eject => "media-eject",
        }
    }

    /// Asset path for `gpui::svg().path(..)`.
    /// Called for every icon on every render, so both answers are cached.
    pub fn path(self) -> SharedString {
        let name = self.freedesktop_name();
        if use_system_theme()
            && let Some(map) = SYSTEM.get()
            && let Some(p) = map.read().ok().and_then(|m| m.get(name).cloned())
        {
            return p;
        }
        static BUNDLED: OnceLock<HashMap<&'static str, SharedString>> = OnceLock::new();
        BUNDLED
            .get_or_init(|| {
                ALL.iter()
                    .map(|i| (i.freedesktop_name(), format!("icons/{}.svg", i.freedesktop_name()).into()))
                    .collect()
            })
            .get(name)
            .cloned()
            .unwrap_or_else(|| format!("icons/{name}.svg").into())
    }
}

static USE_SYSTEM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// Icon name → asset path (`sys:<file>`) in the desktop's theme.
static SYSTEM: OnceLock<RwLock<HashMap<&'static str, SharedString>>> = OnceLock::new();

fn use_system_theme() -> bool {
    USE_SYSTEM.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_use_system_theme(v: bool) {
    USE_SYSTEM.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Resolves the desktop icon theme in the background (Linux only).
/// `done` is called once the lookup has finished.
pub fn init_system_icons(done: impl FnOnce() + Send + 'static) {
    SYSTEM.get_or_init(|| RwLock::new(HashMap::new()));
    #[cfg(target_os = "linux")]
    std::thread::Builder::new()
        .name("icon-theme".into())
        .spawn(move || {
            let found = linux::resolve_all();
            if !found.is_empty() {
                if let Some(m) = SYSTEM.get()
                    && let Ok(mut w) = m.write()
                {
                    *w = found.into_iter().map(|(k, p)| (k, format!("sys:{}", p.display()).into())).collect();
                }
                done();
            }
        })
        .ok();
    #[cfg(not(target_os = "linux"))]
    {
        let _ = done;
    }
}

const ALL: &[Icon] = &[
    Icon::Play,
    Icon::Pause,
    Icon::Stop,
    Icon::Previous,
    Icon::Next,
    Icon::SeekBack,
    Icon::SeekForward,
    Icon::VolumeHigh,
    Icon::VolumeMedium,
    Icon::VolumeLow,
    Icon::VolumeMuted,
    Icon::Fullscreen,
    Icon::ExitFullscreen,
    Icon::Playlist,
    Icon::Search,
    Icon::RepeatAll,
    Icon::RepeatOne,
    Icon::Shuffle,
    Icon::RepeatOff,
    Icon::Settings,
    Icon::Subtitles,
    Icon::WindowClose,
    Icon::WindowMinimize,
    Icon::WindowMaximize,
    Icon::WindowRestore,
    Icon::OpenFile,
    Icon::OpenFolder,
    Icon::Add,
    Icon::Remove,
    Icon::Clear,
    Icon::Disc,
    Icon::Url,
    Icon::Info,
    Icon::Audio,
    Icon::Video,
    Icon::Up,
    Icon::Down,
    Icon::Screenshot,
    Icon::ChevronRight,
    Icon::ChevronLeft,
    Icon::Check,
    Icon::DropDown,
    Icon::Menu,
    Icon::More,
    Icon::Save,
    Icon::Export,
    Icon::Delete,
    Icon::ZoomIn,
    Icon::ZoomOut,
    Icon::Reset,
    Icon::About,
    Icon::Speakers,
    Icon::Recent,
    Icon::Speed,
    Icon::AppGeneric,
    Icon::Eject,
];

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::path::Path;

    fn data_dirs() -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(h) = dirs::data_dir() {
            v.push(h.join("icons"));
        }
        if let Some(h) = dirs::home_dir() {
            v.push(h.join(".icons"));
        }
        let xdg = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        for d in xdg.split(':').filter(|s| !s.is_empty()) {
            v.push(Path::new(d).join("icons"));
        }
        // Inside Flatpak the host's themes are exposed here.
        v.push(PathBuf::from("/run/host/share/icons"));
        v.push(PathBuf::from("/run/host/user-share/icons"));
        v.push(PathBuf::from("/usr/share/icons"));
        v.dedup();
        v
    }

    fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
        let mut in_sec = false;
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with('[') {
                in_sec = l.trim_matches(['[', ']']).eq_ignore_ascii_case(section);
            } else if in_sec
                && let Some((k, v)) = l.split_once('=')
                && k.trim() == key
            {
                return Some(v.trim().trim_matches(['"', '\'']).to_string());
            }
        }
        None
    }

    pub fn current_theme() -> Option<String> {
        if let Ok(t) = std::env::var("MAVPLAYER_ICON_THEME") {
            return Some(t);
        }
        let cfg = dirs::config_dir()?;
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_uppercase();
        if desktop.contains("KDE")
            && let Ok(t) = std::fs::read_to_string(cfg.join("kdeglobals"))
        {
            return ini_value(&t, "Icons", "Theme").or(Some("breeze".into()));
        }
        if let Ok(out) =
            std::process::Command::new("gsettings").args(["get", "org.gnome.desktop.interface", "icon-theme"]).output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string();
            if out.status.success() && !s.is_empty() {
                return Some(s);
            }
        }
        for f in ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"] {
            if let Ok(t) = std::fs::read_to_string(cfg.join(f))
                && let Some(v) = ini_value(&t, "Settings", "gtk-icon-theme-name")
            {
                return Some(v);
            }
        }
        None
    }

    struct ThemeDir {
        path: PathBuf,
        size: i32,
        symbolic: bool,
    }

    struct Theme {
        dirs: Vec<ThemeDir>,
        inherits: Vec<String>,
    }

    fn load_theme(name: &str, roots: &[PathBuf]) -> Option<Theme> {
        let base = roots.iter().map(|r| r.join(name)).find(|p| p.join("index.theme").exists())?;
        let text = std::fs::read_to_string(base.join("index.theme")).ok()?;
        let list = ini_value(&text, "Icon Theme", "Directories").unwrap_or_default();
        let mut dirs = Vec::new();
        for d in list.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            let size = ini_value(&text, d, "Size").and_then(|s| s.parse().ok()).unwrap_or(16);
            let ctx = ini_value(&text, d, "Context").unwrap_or_default();
            if !matches!(
                ctx.as_str(),
                "Actions"
                    | "Status"
                    | "Devices"
                    | "Places"
                    | "MimeTypes"
                    | "Categories"
                    | "Emblems"
                    | "Applications"
                    | ""
            ) {
                continue;
            }
            // Same theme may be split across several roots (e.g. ~/.local and /usr).
            for r in roots {
                let p = r.join(name).join(d);
                if p.is_dir() {
                    dirs.push(ThemeDir { path: p, size, symbolic: d.contains("symbolic") });
                }
            }
        }
        // Prefer symbolic directories, then sizes closest to 16px.
        dirs.sort_by_key(|d| (!d.symbolic, (d.size - 16).abs()));
        let inherits = ini_value(&text, "Icon Theme", "Inherits")
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_default();
        Some(Theme { dirs, inherits })
    }

    fn lookup(theme: &Theme, name: &str) -> Option<PathBuf> {
        for d in &theme.dirs {
            let p = d.path.join(format!("{name}-symbolic.svg"));
            if p.exists() {
                return Some(p);
            }
        }
        // Monochrome action icons at small sizes (e.g. Breeze) work too.
        for d in theme.dirs.iter().filter(|d| d.size <= 24) {
            let p = d.path.join(format!("{name}.svg"));
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    pub fn resolve_all() -> HashMap<&'static str, PathBuf> {
        let mut out = HashMap::new();
        let Some(theme_name) = current_theme() else { return out };
        let roots = data_dirs();
        // Theme plus its inheritance chain (excluding hicolor: no symbolic set).
        let mut chain = Vec::new();
        let mut queue = vec![theme_name];
        while let Some(n) = queue.pop() {
            if chain.iter().any(|(c, _): &(String, Theme)| c == &n) || n == "hicolor" || chain.len() > 6 {
                continue;
            }
            if let Some(t) = load_theme(&n, &roots) {
                queue.extend(t.inherits.iter().rev().cloned());
                chain.push((n, t));
            }
        }
        for icon in ALL {
            let name = icon.freedesktop_name();
            if let Some(p) = chain.iter().find_map(|(_, t)| lookup(t, name)) {
                out.insert(name, p);
            }
        }
        log::debug!("system icon theme provided {}/{} icons", out.len(), ALL.len());
        out
    }
}

/// Asset source serving embedded Papirus icons and system theme files.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some(p) = path.strip_prefix("sys:") {
            return Ok(std::fs::read(p).ok().map(Cow::Owned));
        }
        if let Some(name) = path.strip_prefix("icons/").and_then(|n| n.strip_suffix(".svg")) {
            if let Some((_, data)) = BUNDLED_ICONS.iter().find(|(n, _)| *n == name) {
                return Ok(Some(Cow::Borrowed(*data)));
            }
            // Older bundled icon sets do not necessarily include document-export.
            if name == "document-export" {
                return Ok(Some(Cow::Borrowed(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M13 3H5v18h14v-7M13 3v6h6M13 3l6 6M11 13h11m-4-4 4 4-4 4"/></svg>"#)));
            }
            return Ok(None);
        }
        if path == "app-icon.svg" {
            return Ok(Some(Cow::Borrowed(include_bytes!("../assets/app-icon.svg"))));
        }
        Ok(None)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut paths: Vec<SharedString> = BUNDLED_ICONS
            .iter()
            .map(|(n, _)| SharedString::from(format!("icons/{n}.svg")))
            .filter(|p| p.starts_with(path))
            .collect();
        let export = "icons/document-export.svg";
        if export.starts_with(path) && !BUNDLED_ICONS.iter().any(|(n, _)| *n == "document-export") {
            paths.push(export.into());
        }
        Ok(paths)
    }
}
