//! Public, FFmpeg-free data types shared between the engine and the UI.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// What to play.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaSource {
    File(PathBuf),
    Url(String),
    /// A DVD-Video disc: a device (`/dev/sr0`, `D:`), an ISO image, or a
    /// folder containing `VIDEO_TS`. `title` 0 means "pick automatically".
    Dvd {
        path: String,
        title: u32,
        angle: u32,
    },
    /// An audio CD track: `device` is a drive (`/dev/sr0`, `D:`) or a
    /// `.cue` image of an audio CD. Track 0 means "the whole disc" (the UI
    /// expands it into one item per track).
    Cd {
        device: String,
        track: u32,
    },
}

impl MediaSource {
    pub fn display_name(&self) -> String {
        match self {
            MediaSource::File(p) => {
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
            }
            MediaSource::Url(u) => {
                let trimmed = u.split(['?', '#']).next().unwrap_or(u);
                trimmed.rsplit('/').find(|s| !s.is_empty()).map(percent_decode).unwrap_or_else(|| u.clone())
            }
            MediaSource::Dvd { path, title, .. } => {
                let base = std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                if *title == 0 { format!("DVD ({base})") } else { format!("DVD ({base}) – Title {title}") }
            }
            MediaSource::Cd { track: 0, .. } => "Audio CD".into(),
            MediaSource::Cd { track, .. } => format!("Audio CD – Track {track}"),
        }
    }

    /// Stable key used for resume positions and the recent list.
    pub fn key(&self) -> String {
        match self {
            MediaSource::File(p) => p.display().to_string(),
            MediaSource::Url(u) => u.clone(),
            MediaSource::Dvd { path, title, .. } => format!("dvd://{title}/{path}"),
            MediaSource::Cd { device, track } => format!("cdda://{track}/{device}"),
        }
    }
}

/// Decodes `%XX` escapes. Works on bytes, so a `%` followed by a
/// multi-byte character is kept as-is instead of panicking.
pub fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// Identifies a track. Embedded tracks use the container stream index,
/// external subtitle files get their own id space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrackId {
    Stream(usize),
    External(usize),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub kind: TrackKind,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub hearing_impaired: bool,
    // video
    pub width: i32,
    pub height: i32,
    pub fps: f64,
    pub is_cover_art: bool,
    // audio
    pub channels: i32,
    pub channel_layout: String,
    pub sample_rate: i32,
    pub bitrate: i64,
    pub external_path: Option<PathBuf>,
}

impl Track {
    pub fn label(&self, n: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = &self.title {
            parts.push(t.clone());
        }
        if let Some(l) = &self.language {
            parts.push(language_name(l));
        }
        let mut s = if parts.is_empty() { format!("Track {n}") } else { parts.join(" – ") };
        let mut tags: Vec<String> = vec![self.codec.clone()];
        match self.kind {
            TrackKind::Audio if self.channels > 0 => tags.push(self.channel_layout.clone()),
            TrackKind::Video if self.width > 0 => tags.push(format!("{}×{}", self.width, self.height)),
            _ => {}
        }
        if self.forced {
            tags.push("forced".into());
        }
        if self.hearing_impaired {
            tags.push("SDH".into());
        }
        if self.external_path.is_some() {
            tags.push("external".into());
        }
        s.push_str(&format!("  [{}]", tags.join(", ")));
        s
    }
}

/// Expands common ISO 639 language codes.
pub fn language_name(code: &str) -> String {
    let name = match code.to_ascii_lowercase().as_str() {
        "en" | "eng" => "English",
        "fr" | "fre" | "fra" => "French",
        "de" | "ger" | "deu" => "German",
        "es" | "spa" => "Spanish",
        "it" | "ita" => "Italian",
        "pt" | "por" => "Portuguese",
        "ru" | "rus" => "Russian",
        "ja" | "jpn" => "Japanese",
        "zh" | "chi" | "zho" => "Chinese",
        "ko" | "kor" => "Korean",
        "nl" | "dut" | "nld" => "Dutch",
        "sv" | "swe" => "Swedish",
        "no" | "nor" | "nob" => "Norwegian",
        "da" | "dan" => "Danish",
        "fi" | "fin" => "Finnish",
        "pl" | "pol" => "Polish",
        "cs" | "cze" | "ces" => "Czech",
        "hu" | "hun" => "Hungarian",
        "tr" | "tur" => "Turkish",
        "el" | "gre" | "ell" => "Greek",
        "he" | "heb" => "Hebrew",
        "ar" | "ara" => "Arabic",
        "hi" | "hin" => "Hindi",
        "th" | "tha" => "Thai",
        "uk" | "ukr" => "Ukrainian",
        "ro" | "rum" | "ron" => "Romanian",
        "bg" | "bul" => "Bulgarian",
        "hr" | "hrv" => "Croatian",
        "sr" | "srp" => "Serbian",
        "vi" | "vie" => "Vietnamese",
        "id" | "ind" => "Indonesian",
        "und" => "Undetermined",
        _ => return code.to_string(),
    };
    name.to_string()
}

/// Does `lang` (a stream tag) match a user preference such as "en" / "eng"?
pub fn language_matches(lang: &str, pref: &str) -> bool {
    let (a, b) = (lang.trim().to_ascii_lowercase(), pref.trim().to_ascii_lowercase());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || language_name(&a).eq_ignore_ascii_case(&language_name(&b))
}

/// The desktop's languages, most preferred first, as ISO 639-1 codes
/// ("en", "pt"): `LANGUAGE`/`LC_ALL`/`LC_MESSAGES`/`LANG` on Unix, the user's
/// display languages on Windows and macOS.
pub fn system_languages() -> Vec<String> {
    static LANGS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LANGS
        .get_or_init(|| {
            let mut v: Vec<String> = Vec::new();
            for loc in sys_locale::get_locales() {
                if let Some(l) = locale_language(&loc)
                    && !v.contains(&l)
                {
                    v.push(l);
                }
            }
            v
        })
        .clone()
}

/// "en-US", "pt_BR.UTF-8", "sr@latin" → "en", "pt", "sr". None for "C"/"POSIX".
fn locale_language(loc: &str) -> Option<String> {
    let l = loc.split(['-', '_', '.', '@']).next()?.trim().to_ascii_lowercase();
    (l.len() >= 2 && l.len() <= 3 && l.chars().all(|c| c.is_ascii_alphabetic()) && l != "c" && l != "posix")
        .then_some(l)
}

/// Chooses the subtitle track to show when subtitles are on.
///
/// `langs` is the preference order. The audio language doesn't matter: the
/// user turned subtitles on, so they get full subtitles even when the audio
/// is in the same language. Full subtitles come before forced-only ones,
/// plain before SDH, flagged default first; with nothing in a preferred
/// language, the best of the rest.
pub fn pick_subtitle(info: &MediaInfo, langs: &[String]) -> Option<TrackId> {
    let subs: Vec<&Track> = info.tracks_of(TrackKind::Subtitle).collect();
    let is_lang = |t: &Track, l: &str| t.language.as_deref().is_some_and(|tl| language_matches(tl, l));
    let rank = |t: &&Track| (t.forced, t.hearing_impaired, !t.default);
    langs
        .iter()
        .find_map(|l| subs.iter().copied().filter(|t| is_lang(t, l)).min_by_key(rank))
        .or_else(|| subs.iter().copied().min_by_key(rank))
        .map(|t| t.id)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MediaInfo {
    pub source_name: String,
    pub format_name: String,
    pub format_long_name: String,
    pub duration: f64,
    pub start_time: f64,
    pub bitrate: i64,
    pub seekable: bool,
    pub is_live: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub metadata: Vec<(String, String)>,
    pub tracks: Vec<Track>,
    pub chapters: Vec<Chapter>,
    pub file_size: Option<u64>,
}

impl MediaInfo {
    pub fn tracks_of(&self, kind: TrackKind) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(move |t| t.kind == kind)
    }
    pub fn has_video(&self) -> bool {
        self.tracks_of(TrackKind::Video).any(|t| !t.is_cover_art)
    }
}

#[derive(Clone, Debug, Default)]
pub struct VideoParams {
    pub width: i32,
    pub height: i32,
    pub display_aspect: f64,
    pub pix_fmt: String,
    pub hwdec: Option<String>,
    pub fps: f64,
    pub decoder: String,
    pub color: String,
    pub hdr: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AudioParams {
    pub decoder: String,
    pub in_rate: i32,
    pub in_layout: String,
    pub in_format: String,
    pub out_rate: u32,
    pub out_channels: u16,
    pub device: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Loading,
    Playing,
    Paused,
    Ended,
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct Selected {
    pub video: Option<TrackId>,
    pub audio: Option<TrackId>,
    pub subtitle: Option<TrackId>,
}

#[derive(Clone, Debug)]
pub struct PlaybackState {
    pub status: Status,
    pub source: Option<MediaSource>,
    pub media: Option<Arc<MediaInfo>>,
    pub selected: Selected,
    pub video: Option<VideoParams>,
    pub audio: Option<AudioParams>,
    pub buffering: bool,
    /// Seconds of media buffered ahead of the play position.
    pub cache_secs: f64,
    /// How far into the media (seconds, like the position) the demuxer has
    /// read: the whole duration once it reached the end. NaN when unknown.
    pub cache_end: f64,
    pub dropped_frames: u64,
    pub decoded_frames: u64,
    pub seeking: bool,
    /// Where a DVD played with its menus is (`None` for anything else).
    pub dvd: Option<DvdState>,
}

/// The part of a DVD that is playing.
#[cfg_attr(not(has_dvdnav), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvdDomain {
    /// The disc's opening program (warnings, logos).
    FirstPlay,
    /// A menu.
    Menu,
    /// A title (film, episode, extra).
    Title,
}

/// A DVD played with its menus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DvdState {
    pub domain: DvdDomain,
    /// Buttons on screen (menus; some titles have them too).
    pub buttons: u32,
    /// The highlighted button (1-based, 0 when none).
    pub button: u32,
    /// Current title and chapter (0 outside titles).
    pub title: u32,
    pub chapter: u32,
    pub titles: u32,
    pub angle: u32,
    pub angles: u32,
}

impl DvdState {
    /// Arrow keys, Enter and the mouse go to the disc's buttons.
    pub fn takes_input(&self) -> bool {
        self.buttons > 0
    }
}

/// The menus a DVD can have (not every disc has all of them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvdMenu {
    /// The disc's main (title) menu.
    Title,
    /// The menu of the title playing (often the same as the main menu).
    Root,
    Subtitle,
    Audio,
    Angle,
    Chapter,
}

/// Remote-control input for a DVD played with its menus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DvdCommand {
    Up,
    Down,
    Left,
    Right,
    Activate,
    /// Highlights the button under this point of the picture (in the
    /// video's own pixels, e.g. 0..720 × 0..480).
    Hover(f32, f32),
    /// Activates the button under this point.
    Click(f32, f32),
    Menu(DvdMenu),
    /// Back: from a menu, resume the title; otherwise the menu above.
    Back,
    PlayTitle(u32),
    Angle(u32),
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            status: Status::Idle,
            source: None,
            media: None,
            selected: Selected::default(),
            video: None,
            audio: None,
            buffering: false,
            cache_secs: 0.0,
            cache_end: f64::NAN,
            dropped_frames: 0,
            decoded_frames: 0,
            seeking: false,
            dvd: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum PlayerEvent {
    /// The file was opened and its tracks are known.
    Loaded,
    /// Playback reached the end of the file.
    EndOfFile,
    /// The item queued with `Player::set_next` took over without a gap.
    /// Its own `Loaded` (and any other events) follow.
    Advanced(MediaSource),
    /// Could not open or play the file.
    Error(String),
    /// A non-fatal problem worth telling the user about.
    Warning(String),
    /// A track changed (e.g. after a switch).
    TracksChanged,
    ScreenshotSaved(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HwDecode {
    /// Use hardware decoding for content software decoding may struggle with
    /// (above 1080p), falling back to software automatically.
    Auto,
    /// Always try hardware decoding first.
    Always,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Deinterlace {
    /// Deinterlace only frames flagged as interlaced.
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelMode {
    /// Use the media's channel count if the device supports it, otherwise downmix.
    Auto,
    Stereo,
    Mono,
    Surround51,
    Surround71,
}

/// Filter for shrinking pictures to the window (and for the HDR tone-mapping
/// path, which only ever shrinks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScaleQuality {
    /// Nearest neighbour: hard pixel edges.
    Nearest,
    /// Bilinear. Saved settings from before used the name "Fast".
    #[serde(alias = "Fast")]
    Linear,
    /// Bicubic.
    Good,
    /// Lanczos.
    Best,
}

/// How pictures smaller than the window are enlarged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Upscaler {
    /// The GPU's bilinear texture filtering when drawing (cheapest).
    #[default]
    Bilinear,
    /// Nearest neighbour on the CPU: hard pixel edges, for pixel art.
    Nearest,
    /// Bicubic, by swscale (or `scale_cuda` for pictures on the GPU) in the
    /// presenter: sharper than bilinear, and needs no libplacebo.
    Bicubic,
    /// libplacebo's EWA Lanczos (sharp), as in mpv's high-quality profile.
    EwaLanczos,
    /// FSRCNNX x2 16-0-4-1: neural luma doubler, good for live action.
    Fsrcnnx,
    /// FSRCNNX x2 8-0-4-1: lighter version of the above.
    FsrcnnxFast,
    /// ArtCNN C4F16: newer neural luma doubler, very sharp; anime and art.
    ArtCnn,
    /// ArtCNN C4F16 DS: the same, trained to also denoise and sharpen.
    ArtCnnDenoise,
    /// RAVU-Lite AR r4: light prescaler with anti-ringing.
    Ravu,
    /// AMD FidelityFX Super Resolution 1.0 (EASU + RCAS).
    Fsr,
    /// NVIDIA Image Scaling 1.0.3.
    Nis,
    /// The bundled Anime4K presets (see [`Anime4KMode`]).
    Anime4K,
    /// The user's own mpv-style `.glsl`/`.hook` shader.
    CustomShader,
}

impl Upscaler {
    /// Whether this runs on the GPU through FFmpeg's libplacebo filter.
    pub fn needs_libplacebo(self) -> bool {
        !matches!(self, Upscaler::Bilinear | Upscaler::Nearest | Upscaler::Bicubic)
    }

    /// The filter the presenter enlarges with for this upscaler, or `None`
    /// when it leaves that to the GPU's drawing (bilinear) or to libplacebo.
    pub fn presenter_filter(self) -> Option<ScaleQuality> {
        match self {
            Upscaler::Nearest => Some(ScaleQuality::Nearest),
            Upscaler::Bicubic => Some(ScaleQuality::Good),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Upscaler::Bilinear => "Bilinear (fastest)",
            Upscaler::Nearest => "Nearest neighbor (pixel art)",
            Upscaler::Bicubic => "Bicubic (sharper)",
            Upscaler::EwaLanczos => "EWA Lanczos (high quality)",
            Upscaler::Fsrcnnx => "FSRCNNX (live action)",
            Upscaler::FsrcnnxFast => "FSRCNNX fast",
            Upscaler::ArtCnn => "ArtCNN (anime, sharp)",
            Upscaler::ArtCnnDenoise => "ArtCNN denoise + sharpen",
            Upscaler::Ravu => "RAVU-Lite (light)",
            Upscaler::Fsr => "AMD FSR",
            Upscaler::Nis => "NVIDIA Image Scaling",
            Upscaler::Anime4K => "Anime4K",
            Upscaler::CustomShader => "Custom shader file",
        }
    }

    pub const ALL: [Upscaler; 13] = [
        Upscaler::Bilinear,
        Upscaler::Bicubic,
        Upscaler::Nearest,
        Upscaler::EwaLanczos,
        Upscaler::Fsrcnnx,
        Upscaler::FsrcnnxFast,
        Upscaler::ArtCnn,
        Upscaler::ArtCnnDenoise,
        Upscaler::Ravu,
        Upscaler::Fsr,
        Upscaler::Nis,
        Upscaler::Anime4K,
        Upscaler::CustomShader,
    ];
}

/// Anime4K's mpv presets. A suits blurry 1080p-like sources, B 720p with
/// aliasing or ringing, C 480p/SD; the doubled modes restore twice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Anime4KMode {
    #[default]
    A,
    B,
    C,
    AA,
    BB,
    CA,
}

impl Anime4KMode {
    pub const ALL: [Anime4KMode; 6] =
        [Anime4KMode::A, Anime4KMode::B, Anime4KMode::C, Anime4KMode::AA, Anime4KMode::BB, Anime4KMode::CA];

    pub fn name(self) -> &'static str {
        match self {
            Anime4KMode::A => "A",
            Anime4KMode::B => "B",
            Anime4KMode::C => "C",
            Anime4KMode::AA => "A+A",
            Anime4KMode::BB => "B+B",
            Anime4KMode::CA => "C+A",
        }
    }
}

/// Anime4K network size: HQ for desktop GPUs, Fast for laptops and iGPUs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Anime4KQuality {
    #[default]
    High,
    Fast,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rotation {
    None,
    R90,
    R180,
    R270,
}

/// Picture adjustments, all applied by libavfilter on the decode thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoAdjust {
    pub brightness: f32, // -1..1
    pub contrast: f32,   // 0..3, 1 = neutral
    pub saturation: f32, // 0..3, 1 = neutral
    pub gamma: f32,      // 0.1..4, 1 = neutral
    pub hue: f32,        // degrees
    pub sharpen: f32,    // 0..2
    pub denoise: bool,
    pub hflip: bool,
    pub vflip: bool,
    pub rotation: Rotation,
    pub grayscale: bool,
    pub negate: bool,
    /// Crop as fractions removed from each edge (left, top, right, bottom).
    pub crop: Option<[f32; 4]>,
}

impl Default for VideoAdjust {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            gamma: 1.0,
            hue: 0.0,
            sharpen: 0.0,
            denoise: false,
            hflip: false,
            vflip: false,
            rotation: Rotation::None,
            grayscale: false,
            negate: false,
            crop: None,
        }
    }
}

pub const EQ_BANDS: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioFx {
    pub eq_enabled: bool,
    pub preamp_db: f32,
    pub eq_db: [f32; 10],
    pub normalize: bool,
    pub night_mode: bool,
    pub stereo_widen: bool,
    pub swap_channels: bool,
    pub karaoke: bool,
}

impl Default for AudioFx {
    fn default() -> Self {
        Self {
            eq_enabled: false,
            preamp_db: 0.0,
            eq_db: [0.0; 10],
            normalize: false,
            night_mode: false,
            stereo_widen: false,
            swap_channels: false,
            karaoke: false,
        }
    }
}

/// Engine-level configuration (the user-facing settings that influence decoding).
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub hwdec: HwDecode,
    pub deinterlace: Deinterlace,
    pub channels: ChannelMode,
    pub audio_device: Option<String>,
    pub scale_quality: ScaleQuality,
    pub upscaler: Upscaler,
    pub anime4k_mode: Anime4KMode,
    pub anime4k_quality: Anime4KQuality,
    /// For [`Upscaler::CustomShader`].
    pub custom_shader: Option<std::path::PathBuf>,
    /// KrigBilateral chroma upscaling alongside a GPU upscaler.
    pub shader_chroma: bool,
    /// AMD CAS sharpening after a GPU upscaler.
    pub shader_sharpen: bool,
    pub tone_map_hdr: bool,
    pub audio_langs: Vec<String>,
    pub sub_langs: Vec<String>,
    /// Subtitles are on: each file starts with its best subtitle track.
    pub show_subs: bool,
    pub auto_load_subs: bool,
    pub sub_charset: Option<String>,
    pub network_cache_secs: f64,
    pub decoder_threads: u32,
    pub dvd_region: u32,
    pub user_agent: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            hwdec: HwDecode::Auto,
            deinterlace: Deinterlace::Auto,
            channels: ChannelMode::Auto,
            audio_device: None,
            scale_quality: ScaleQuality::Good,
            upscaler: Upscaler::Bilinear,
            anime4k_mode: Anime4KMode::A,
            anime4k_quality: Anime4KQuality::High,
            custom_shader: None,
            shader_chroma: false,
            shader_sharpen: false,
            tone_map_hdr: true,
            audio_langs: vec![],
            sub_langs: vec![],
            show_subs: false,
            auto_load_subs: true,
            sub_charset: None,
            network_cache_secs: 10.0,
            decoder_threads: 0,
            dvd_region: 0,
            user_agent: format!("MavPlayer/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// A decoded, colour-converted picture ready for the screen. Pixels are BGRA,
/// tightly packed, with square pixels (aspect already applied).
pub struct PresentedFrame {
    /// Size of the picture itself.
    pub width: u32,
    pub height: u32,
    /// Border around the picture in `bgra`, repeating its edge pixels: the
    /// buffer is `width + 2*pad` × `height + 2*pad`. Draw the image `pad`
    /// pixels larger on each side, clipped to the picture.
    pub pad: u32,
    pub bgra: Vec<u8>,
}

/// One displayed subtitle event.
#[derive(Clone, Debug)]
pub struct SubEvent {
    pub start: f64,
    pub end: f64,
    pub content: SubContent,
}

#[derive(Clone, Debug)]
pub enum SubContent {
    Text(String),
    Bitmap(Arc<Vec<SubBitmap>>),
}

#[derive(Debug)]
pub struct SubBitmap {
    pub id: u64,
    /// Placement relative to the subtitle canvas (normally the video frame).
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub canvas_w: u32,
    pub canvas_h: u32,
    /// BGRA pixels, premultiplication not applied.
    pub bgra: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(i: usize, kind: TrackKind, lang: &str) -> Track {
        Track {
            id: TrackId::Stream(i),
            kind,
            codec: String::new(),
            language: Some(lang.to_string()).filter(|l| !l.is_empty()),
            title: None,
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
            external_path: None,
        }
    }

    fn media(tracks: Vec<Track>) -> MediaInfo {
        MediaInfo { tracks, ..Default::default() }
    }

    fn langs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn subtitles_ignore_audio_language() {
        let mut signs = track(3, TrackKind::Subtitle, "eng");
        signs.forced = true;
        let mut sdh = track(4, TrackKind::Subtitle, "eng");
        sdh.hearing_impaired = true;
        let m = media(vec![
            track(0, TrackKind::Audio, "jpn"),
            track(1, TrackKind::Audio, "eng"),
            signs,
            sdh,
            track(5, TrackKind::Subtitle, "eng"),
            track(6, TrackKind::Subtitle, "ger"),
        ]);
        // Full English subtitles, not the signs or SDH track, whatever the audio.
        assert_eq!(pick_subtitle(&m, &langs(&["en"])), Some(TrackId::Stream(5)));
        // The first preferred language that has subtitles wins.
        assert_eq!(pick_subtitle(&m, &langs(&["fr", "de", "en"])), Some(TrackId::Stream(6)));
        // Forced-only in a preferred language beats nothing in it.
        let mut fr = track(7, TrackKind::Subtitle, "fre");
        fr.forced = true;
        let m = media(vec![fr, track(8, TrackKind::Subtitle, "spa")]);
        assert_eq!(pick_subtitle(&m, &langs(&["fr"])), Some(TrackId::Stream(7)));
    }

    #[test]
    fn subtitles_without_preferred_language() {
        // One untagged track: shown whatever the preferences.
        let m = media(vec![track(0, TrackKind::Audio, ""), track(1, TrackKind::Subtitle, "")]);
        assert_eq!(pick_subtitle(&m, &langs(&["en"])), Some(TrackId::Stream(1)));
        assert_eq!(pick_subtitle(&m, &[]), Some(TrackId::Stream(1)));
        // No preferred language: the default-flagged track.
        let mut fr = track(2, TrackKind::Subtitle, "fre");
        fr.default = true;
        let m = media(vec![track(1, TrackKind::Subtitle, "spa"), fr]);
        assert_eq!(pick_subtitle(&m, &langs(&["en"])), Some(TrackId::Stream(2)));
        assert_eq!(pick_subtitle(&media(vec![]), &[]), None);
    }

    #[test]
    fn locale_languages() {
        assert_eq!(locale_language("en-US").as_deref(), Some("en"));
        assert_eq!(locale_language("pt_BR.UTF-8").as_deref(), Some("pt"));
        assert_eq!(locale_language("sr@latin").as_deref(), Some("sr"));
        assert_eq!(locale_language("C"), None);
        assert_eq!(locale_language("POSIX"), None);
    }
}
