//! User settings and small bits of persistent state (recent files, resume
//! positions), stored as JSON in the platform config directory.

use crate::engine::{
    Anime4KMode, Anime4KQuality, AudioFx, ChannelMode, Deinterlace, EngineConfig, HwDecode, MediaSource, ScaleQuality,
    Upscaler,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    System,
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IconTheme {
    /// Use the desktop's icon theme where available (Linux), Papirus otherwise.
    System,
    Papirus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollAction {
    Volume,
    Seek,
    Nothing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubStyle {
    Outline,
    Box,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenMode {
    /// Opening files replaces the playlist.
    Replace,
    /// Opening files appends to the playlist and plays the first one.
    Append,
}

/// What playback does while the progress bar is held down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeekDragMode {
    /// Keep playing; the picture follows the bar as it is dragged.
    KeepPlaying,
    /// Pause while the bar is held and resume when it is released.
    Pause,
}

/// What opening media does while a MavPlayer window is already open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunningMode {
    /// Start another, independent window.
    NewWindow,
    /// Add to the open window's playlist and play it.
    PlayNow,
    /// Add to the open window's playlist without interrupting playback.
    Enqueue,
    /// Clear the open window's playlist and play it.
    Replace,
}

/// A control the bottom bar can show. Play/Pause is always there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BarControl {
    Stop,
    Previous,
    Next,
    Subtitles,
    AudioTrack,
    Speed,
    Repeat,
    Shuffle,
    Playlist,
    Settings,
    Fullscreen,
    Volume,
}

impl BarControl {
    /// Every control, in the bar's default order.
    pub const ALL: [BarControl; 12] = [
        Self::Stop,
        Self::Previous,
        Self::Next,
        Self::Subtitles,
        Self::AudioTrack,
        Self::Speed,
        Self::Repeat,
        Self::Shuffle,
        Self::Playlist,
        Self::Settings,
        Self::Fullscreen,
        Self::Volume,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::Previous => "Previous",
            Self::Next => "Next",
            Self::Subtitles => "Subtitle track",
            Self::AudioTrack => "Audio track",
            Self::Speed => "Playback speed",
            Self::Repeat => "Repeat",
            Self::Shuffle => "Shuffle",
            Self::Playlist => "Playlist",
            Self::Settings => "Preferences",
            Self::Fullscreen => "Fullscreen",
            Self::Volume => "Volume",
        }
    }

    /// Shown next to Play/Pause, left of the title (the rest go right of it).
    pub fn on_left(self) -> bool {
        matches!(self, Self::Stop | Self::Previous | Self::Next)
    }
}

/// Reads the bar's control list, skipping entries this version doesn't know
/// (so a settings file from a newer version doesn't reset every setting).
fn de_bar_controls<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<BarControl>, D::Error> {
    let raw = Vec::<serde_json::Value>::deserialize(d)?;
    let mut out: Vec<BarControl> = Vec::new();
    for c in raw.into_iter().filter_map(|v| serde_json::from_value(v).ok()) {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // Interface
    pub theme: ThemeMode,
    pub icon_theme: IconTheme,
    pub show_menubar: bool,
    pub osd_messages: bool,
    pub controls_autohide_secs: f64,
    pub show_remaining_time: bool,
    pub fit_window_to_video: bool,
    pub single_click_pause: bool,
    pub drag_video_to_move_window: bool,
    pub double_click_fullscreen: bool,
    /// Playlist entries play on a double click (a single click only selects
    /// them) rather than on a single click.
    pub playlist_double_click: bool,
    pub scroll_action: ScrollAction,
    pub show_playlist: bool,
    /// Going fullscreen starts with the playlist hidden, whatever the window
    /// showed (it can still be opened there; leaving restores the window's).
    pub hide_playlist_in_fullscreen: bool,
    /// Keyboard shortcuts changed from the defaults: command id (see
    /// `ui::keymap::COMMANDS`) to its keystrokes. An empty list unbinds it.
    pub key_bindings: BTreeMap<String, Vec<String>>,
    /// Controls shown in the bottom bar, in order.
    #[serde(deserialize_with = "de_bar_controls")]
    pub bar_controls: Vec<BarControl>,
    // Playback
    pub resume_playback: bool,
    pub auto_play_next: bool,
    /// Files you open start paused (moving on to the next item never does).
    pub start_paused: bool,
    pub open_mode: OpenMode,
    pub when_running: RunningMode,
    /// Closing the window leaves MavPlayer running with a system tray icon,
    /// so media opened later plays without a cold start.
    pub keep_in_tray: bool,
    /// "Play in the background": closing the window while a file is open
    /// hides it to the tray and playback goes on; the tray icon's menu has
    /// playback controls.
    pub tray_playback: bool,
    pub start_fullscreen: bool,
    pub seek_step: f64,
    pub seek_step_large: f64,
    pub volume_step: f64,
    pub max_volume: u32,
    pub exact_seeks: bool,
    pub seek_drag: SeekDragMode,
    pub screenshot_dir: Option<PathBuf>,
    // Video
    pub hwdec: HwDecode,
    pub deinterlace: Deinterlace,
    pub scale_quality: ScaleQuality,
    pub upscaler: Upscaler,
    pub anime4k_mode: Anime4KMode,
    pub anime4k_quality: Anime4KQuality,
    pub custom_shader: Option<PathBuf>,
    pub shader_chroma: bool,
    pub shader_sharpen: bool,
    pub tone_map_hdr: bool,
    // Audio
    pub audio_device: Option<String>,
    pub channels: ChannelMode,
    pub audio_langs: String,
    pub audio_fx: AudioFx,
    // Subtitles
    pub sub_langs: String,
    /// Subtitles are on. Only the user changes it (the subtitle button, V,
    /// the track menus, Preferences), and it carries over to later files and launches.
    #[serde(alias = "subs_enabled_by_default")]
    pub show_subtitles: bool,
    pub auto_load_subs: bool,
    pub sub_scale: f32,
    pub sub_margin: f32,
    pub sub_style: SubStyle,
    pub sub_charset: String,
    // Network & discs
    pub network_cache_secs: f64,
    pub dvd_region: u32,
    pub dvd_device: Option<String>,
    // Advanced
    pub decoder_threads: u32,
    // Remembered state
    pub volume: f64,
    pub muted: bool,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub window_size: Option<(f32, f32)>,
    pub recent: Vec<MediaSource>,
    pub last_dir: Option<PathBuf>,
    /// Which defaults this file was written with (see `Settings::from_value`).
    pub config_version: u32,
}

/// Bumped when a changed default should also reach settings files that
/// still hold the old default (see `Settings::from_value`).
const CONFIG_VERSION: u32 = 1;

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            icon_theme: IconTheme::System,
            show_menubar: true,
            osd_messages: true,
            controls_autohide_secs: 2.0,
            show_remaining_time: false,
            fit_window_to_video: true,
            single_click_pause: false,
            drag_video_to_move_window: false,
            double_click_fullscreen: true,
            playlist_double_click: false,
            scroll_action: ScrollAction::Volume,
            show_playlist: false,
            hide_playlist_in_fullscreen: true,
            key_bindings: BTreeMap::new(),
            bar_controls: BarControl::ALL.to_vec(),
            resume_playback: true,
            auto_play_next: true,
            start_paused: false,
            open_mode: OpenMode::Replace,
            when_running: RunningMode::NewWindow,
            keep_in_tray: false,
            tray_playback: false,
            start_fullscreen: false,
            seek_step: 5.0,
            seek_step_large: 60.0,
            volume_step: 5.0,
            max_volume: 130,
            exact_seeks: true,
            seek_drag: SeekDragMode::KeepPlaying,
            screenshot_dir: None,
            hwdec: HwDecode::Always,
            deinterlace: Deinterlace::Auto,
            scale_quality: ScaleQuality::Good,
            upscaler: Upscaler::Bilinear,
            anime4k_mode: Anime4KMode::A,
            anime4k_quality: Anime4KQuality::High,
            custom_shader: None,
            shader_chroma: false,
            shader_sharpen: false,
            tone_map_hdr: true,
            audio_device: None,
            channels: ChannelMode::Auto,
            audio_langs: String::new(),
            audio_fx: AudioFx::default(),
            sub_langs: String::new(),
            show_subtitles: false,
            auto_load_subs: true,
            sub_scale: 1.0,
            sub_margin: 0.05,
            sub_style: SubStyle::Outline,
            sub_charset: String::new(),
            network_cache_secs: 10.0,
            dvd_region: 0,
            dvd_device: None,
            decoder_threads: 0,
            volume: 1.0,
            muted: false,
            repeat: RepeatMode::Off,
            shuffle: false,
            window_size: None,
            recent: Vec::new(),
            last_dir: None,
            config_version: CONFIG_VERSION,
        }
    }
}

fn split_langs(s: &str) -> Vec<String> {
    s.split([',', ' ', ';']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// The user's own subtitle languages, then the system's (so "auto" means
/// the desktop language, and a list that matches nothing still falls back
/// to it).
fn sub_lang_prefs(own: &str) -> Vec<String> {
    let mut v = split_langs(own);
    for l in crate::engine::system_languages() {
        if !v.iter().any(|x| crate::engine::language_matches(x, &l)) {
            v.push(l);
        }
    }
    v
}

impl Settings {
    /// Either setting that needs a tray icon is on.
    pub fn wants_tray(&self) -> bool {
        self.keep_in_tray || self.tray_playback
    }

    pub fn engine_config(&self) -> EngineConfig {
        EngineConfig {
            hwdec: self.hwdec,
            deinterlace: self.deinterlace,
            channels: self.channels,
            audio_device: self.audio_device.clone(),
            scale_quality: self.scale_quality,
            upscaler: self.upscaler,
            anime4k_mode: self.anime4k_mode,
            anime4k_quality: self.anime4k_quality,
            custom_shader: self.custom_shader.clone(),
            shader_chroma: self.shader_chroma,
            shader_sharpen: self.shader_sharpen,
            tone_map_hdr: self.tone_map_hdr,
            audio_langs: split_langs(&self.audio_langs),
            sub_langs: sub_lang_prefs(&self.sub_langs),
            show_subs: self.show_subtitles,
            auto_load_subs: self.auto_load_subs,
            sub_charset: Some(self.sub_charset.trim().to_string()).filter(|s| !s.is_empty()),
            network_cache_secs: self.network_cache_secs,
            decoder_threads: self.decoder_threads,
            dvd_region: self.dvd_region,
            ..EngineConfig::default()
        }
    }

    pub fn add_recent(&mut self, src: &MediaSource) {
        self.recent.retain(|r| r != src);
        self.recent.insert(0, src.clone());
        self.recent.truncate(15);
    }

    pub fn screenshot_dir(&self) -> PathBuf {
        self.screenshot_dir
            .clone()
            .or_else(|| dirs::picture_dir().map(|p| p.join("MavPlayer")))
            .or_else(|| dirs::home_dir().map(|p| p.join("MavPlayer Screenshots")))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MAVPLAYER_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("mavplayer")
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Option<T> {
    let data = std::fs::read(p).ok()?;
    match serde_json::from_slice(&data) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("ignoring unreadable {}: {e}", p.display());
            None
        }
    }
}

/// Writes atomically so a crash never leaves a half-written file. The
/// temporary file is per process, so two windows saving at once can't mix
/// their writes, and (on Unix) readable only by the user: these files hold
/// what was watched, and stream URLs can carry credentials.
fn write_json<T: Serialize>(p: &Path, v: &T) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let written = opts.open(&tmp).and_then(|mut f| f.write_all(&serde_json::to_vec_pretty(v)?));
    if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, p)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

impl Settings {
    pub fn load() -> Self {
        let raw: Option<serde_json::Value> = read_json(&config_dir().join("settings.json"));
        raw.and_then(|v| Self::from_value(v).ok()).unwrap_or_default()
    }

    fn from_value(v: serde_json::Value) -> serde_json::Result<Self> {
        // Before upscaling had its own setting, choosing nearest-neighbour
        // scaling also enlarged with it: keep that for old settings files.
        let had_upscaler = v.get("upscaler").is_some();
        let version = v.get("config_version").and_then(|x| x.as_u64()).unwrap_or(0);
        let mut s: Settings = serde_json::from_value(v)?;
        if !had_upscaler && s.scale_quality == ScaleQuality::Nearest {
            s.upscaler = Upscaler::Nearest;
        }
        // Version 1: hardware decoding defaults to Always. Older files saved
        // the old default (Automatic) whether or not anyone chose it.
        if version < 1 && s.hwdec == HwDecode::Auto {
            s.hwdec = HwDecode::Always;
        }
        s.config_version = CONFIG_VERSION;
        Ok(s)
    }
    pub fn save(&self) {
        if let Err(e) = write_json(&config_dir().join("settings.json"), self) {
            log::warn!("could not save settings: {e}");
        }
    }
}

/// Remembered playback positions, keyed by `MediaSource::key()`.
#[derive(Default, Serialize, Deserialize)]
pub struct ResumeStore {
    positions: HashMap<String, f64>,
    /// Keys from least to most recently stored, so the oldest go first when
    /// the store is full. (Missing from files written before it existed.)
    #[serde(default)]
    order: Vec<String>,
}

const MAX_RESUME: usize = 500;

impl ResumeStore {
    pub fn load() -> Self {
        read_json(&config_dir().join("resume.json")).unwrap_or_default()
    }
    pub fn save(&self) {
        if let Err(e) = write_json(&config_dir().join("resume.json"), self) {
            log::warn!("could not save resume positions: {e}");
        }
    }
    pub fn get(&self, key: &str) -> Option<f64> {
        self.positions.get(key).copied()
    }
    /// Stores a position unless it's at the very start or end.
    pub fn remember(&mut self, key: String, pos: f64, duration: f64) {
        if !(duration > 60.0 && pos > 10.0 && pos < duration - 20.0) {
            self.forget(&key);
            return;
        }
        self.order.retain(|k| *k != key);
        self.order.push(key.clone());
        self.positions.insert(key, pos);
        if self.positions.len() > MAX_RESUME {
            // Entries saved before `order` existed count as the oldest.
            let ordered: std::collections::HashSet<&String> = self.order.iter().collect();
            let mut victims: Vec<String> = self.positions.keys().filter(|k| !ordered.contains(k)).cloned().collect();
            victims.extend(self.order.iter().cloned());
            victims.truncate(self.positions.len() - MAX_RESUME + 100);
            for k in victims {
                self.forget(&k);
            }
        }
    }

    pub fn forget(&mut self, key: &str) {
        self.positions.remove(key);
        self.order.retain(|k| k != key);
    }

    pub fn clear(&mut self) {
        self.positions.clear();
        self.order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_store_evicts_the_oldest() {
        let mut r = ResumeStore::default();
        for i in 0..=MAX_RESUME {
            r.remember(format!("f{i}"), 100.0, 1000.0);
        }
        // Touch the first one again: it is now the newest.
        r.remember("f0".into(), 200.0, 1000.0);
        r.remember("new".into(), 100.0, 1000.0);
        assert!(r.positions.len() <= MAX_RESUME);
        assert_eq!(r.get("f0"), Some(200.0));
        assert_eq!(r.get("new"), Some(100.0));
        assert_eq!(r.get("f1"), None);
        assert_eq!(r.positions.len(), r.order.len());
    }

    #[test]
    fn bar_controls_load_leniently() {
        // Older files have no list: today's full bar.
        let s: Settings = serde_json::from_str("{\"muted\":true}").unwrap();
        assert_eq!(s.bar_controls, BarControl::ALL.to_vec());
        assert!(s.muted);
        // Unknown entries (from a newer version) and repeats are skipped
        // rather than resetting every setting.
        let s: Settings =
            serde_json::from_str(r#"{"bar_controls":["Volume","Teleport","Stop","Volume"],"muted":true}"#).unwrap();
        assert_eq!(s.bar_controls, vec![BarControl::Volume, BarControl::Stop]);
        assert!(s.muted);
        let round: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.bar_controls, s.bar_controls);
    }

    #[test]
    fn old_nearest_scaling_also_upscales() {
        let load = |j: &str| Settings::from_value(serde_json::from_str(j).unwrap()).unwrap();
        assert_eq!(load(r#"{"scale_quality":"Nearest"}"#).upscaler, Upscaler::Nearest);
        assert_eq!(load(r#"{"scale_quality":"Good"}"#).upscaler, Upscaler::Bilinear);
        assert_eq!(load(r#"{"scale_quality":"Nearest","upscaler":"Bilinear"}"#).upscaler, Upscaler::Bilinear);
        assert_eq!(load(r#"{"upscaler":"Anime4K"}"#).upscaler, Upscaler::Anime4K);
    }

    #[test]
    fn old_automatic_hwdec_becomes_always() {
        let load = |j: &str| Settings::from_value(serde_json::from_str(j).unwrap()).unwrap();
        assert_eq!(Settings::default().hwdec, HwDecode::Always);
        assert_eq!(load(r#"{"hwdec":"Auto"}"#).hwdec, HwDecode::Always);
        assert_eq!(load(r#"{"hwdec":"Off"}"#).hwdec, HwDecode::Off);
        // Chosen after the change: kept.
        assert_eq!(load(r#"{"hwdec":"Auto","config_version":1}"#).hwdec, HwDecode::Auto);
    }
}
