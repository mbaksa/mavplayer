//! GPU upscaling through FFmpeg's `libplacebo` filter (Vulkan): libplacebo's
//! own high-quality scalers, and mpv-style user shaders, either the ones
//! bundled from `third_party/shaders` (Anime4K, FSRCNNX, ArtCNN, RAVU, FSR,
//! NIS, KrigBilateral, CAS) or a file the user picks.
//!
//! It runs at the end of the video filter graph, only when the picture is
//! being enlarged, and outputs frames at display size so the presenter only
//! converts them to BGRA.

use super::av::{self, ff};
use super::present::output_size;
use super::types::*;
use std::os::raw::{c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::OnceLock;

macro_rules! shader {
    ($p:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/third_party/shaders/", $p))
    };
}

mod a4k {
    pub const CLAMP: &str = shader!("anime4k/Anime4K_Clamp_Highlights.glsl");
    pub const RESTORE_S: &str = shader!("anime4k/Anime4K_Restore_CNN_S.glsl");
    pub const RESTORE_M: &str = shader!("anime4k/Anime4K_Restore_CNN_M.glsl");
    pub const RESTORE_VL: &str = shader!("anime4k/Anime4K_Restore_CNN_VL.glsl");
    pub const RESTORE_SOFT_S: &str = shader!("anime4k/Anime4K_Restore_CNN_Soft_S.glsl");
    pub const RESTORE_SOFT_M: &str = shader!("anime4k/Anime4K_Restore_CNN_Soft_M.glsl");
    pub const RESTORE_SOFT_VL: &str = shader!("anime4k/Anime4K_Restore_CNN_Soft_VL.glsl");
    pub const UPSCALE_S: &str = shader!("anime4k/Anime4K_Upscale_CNN_x2_S.glsl");
    pub const UPSCALE_M: &str = shader!("anime4k/Anime4K_Upscale_CNN_x2_M.glsl");
    pub const UPSCALE_VL: &str = shader!("anime4k/Anime4K_Upscale_CNN_x2_VL.glsl");
    pub const UPSCALE_DENOISE_M: &str = shader!("anime4k/Anime4K_Upscale_Denoise_CNN_x2_M.glsl");
    pub const UPSCALE_DENOISE_VL: &str = shader!("anime4k/Anime4K_Upscale_Denoise_CNN_x2_VL.glsl");
    pub const DOWNSCALE_X2: &str = shader!("anime4k/Anime4K_AutoDownscalePre_x2.glsl");
    pub const DOWNSCALE_X4: &str = shader!("anime4k/Anime4K_AutoDownscalePre_x4.glsl");
}

const FSRCNNX_16: &str = shader!("fsrcnnx/FSRCNNX_x2_16-0-4-1.glsl");
const FSRCNNX_8: &str = shader!("fsrcnnx/FSRCNNX_x2_8-0-4-1.glsl");
const ARTCNN: &str = shader!("artcnn/ArtCNN_C4F16.glsl");
const ARTCNN_DS: &str = shader!("artcnn/ArtCNN_C4F16_DS.glsl");
const RAVU: &str = shader!("ravu/ravu-lite-ar-r4.hook");
const FSR: &str = shader!("amd/FSR.glsl");
const CAS: &str = shader!("amd/CAS.glsl");
const NIS: &str = shader!("nvidia/NVScaler.glsl");
const KRIG: &str = shader!("igv/KrigBilateral.glsl");

/// Largest picture enlarged on the GPU (zoomed-in views can ask for more).
const MAX_DIM: u32 = 7680;

/// The shader files of an Anime4K preset, in order, as in its mpv
/// `input.conf` (Anime4K v4.0 `GLSL_Instructions_Advanced.md`).
fn anime4k(mode: Anime4KMode, quality: Anime4KQuality) -> Vec<&'static str> {
    use a4k::*;
    let hq = quality == Anime4KQuality::High;
    let restore = if hq { RESTORE_VL } else { RESTORE_M };
    let restore_soft = if hq { RESTORE_SOFT_VL } else { RESTORE_SOFT_M };
    let upscale = if hq { UPSCALE_VL } else { UPSCALE_M };
    let upscale_denoise = if hq { UPSCALE_DENOISE_VL } else { UPSCALE_DENOISE_M };
    // The second, smaller stage.
    let restore2 = if hq { RESTORE_M } else { RESTORE_S };
    let restore_soft2 = if hq { RESTORE_SOFT_M } else { RESTORE_SOFT_S };
    let upscale2 = if hq { UPSCALE_M } else { UPSCALE_S };
    match mode {
        Anime4KMode::A => vec![CLAMP, restore, upscale, DOWNSCALE_X2, DOWNSCALE_X4, upscale2],
        Anime4KMode::B => vec![CLAMP, restore_soft, upscale, DOWNSCALE_X2, DOWNSCALE_X4, upscale2],
        Anime4KMode::C => vec![CLAMP, upscale_denoise, DOWNSCALE_X2, DOWNSCALE_X4, upscale2],
        Anime4KMode::AA => vec![CLAMP, restore, upscale, restore2, DOWNSCALE_X2, DOWNSCALE_X4, upscale2],
        Anime4KMode::BB => vec![CLAMP, restore_soft, upscale, DOWNSCALE_X2, DOWNSCALE_X4, restore_soft2, upscale2],
        Anime4KMode::CA => vec![CLAMP, upscale_denoise, DOWNSCALE_X2, DOWNSCALE_X4, restore2, upscale2],
    }
}

/// Which shaders a graph runs. Compared every frame to see whether the graph
/// needs rebuilding, so it names the shaders rather than holding their text.
#[derive(Clone, Debug, PartialEq)]
pub struct ShaderSet {
    upscaler: Upscaler,
    anime4k: (Anime4KMode, Anime4KQuality),
    file: Option<PathBuf>,
    chroma: bool,
    sharpen: bool,
}

impl ShaderSet {
    fn from_config(cfg: &EngineConfig) -> Option<Self> {
        if !cfg.upscaler.needs_libplacebo() || (cfg.upscaler == Upscaler::CustomShader && cfg.custom_shader.is_none()) {
            return None;
        }
        Some(Self {
            upscaler: cfg.upscaler,
            anime4k: (cfg.anime4k_mode, cfg.anime4k_quality),
            file: if cfg.upscaler == Upscaler::CustomShader { cfg.custom_shader.clone() } else { None },
            chroma: cfg.shader_chroma,
            sharpen: cfg.shader_sharpen,
        })
    }

    /// All the shaders as one mpv hook file: passes are parsed one by one,
    /// so files concatenate like mpv's `glsl-shaders` list.
    fn source(&self) -> anyhow::Result<Vec<u8>> {
        let mut parts: Vec<std::borrow::Cow<'static, [u8]>> = Vec::new();
        if self.chroma {
            parts.push(KRIG.as_bytes().into());
        }
        let main: Vec<&'static str> = match self.upscaler {
            Upscaler::Bilinear | Upscaler::Nearest | Upscaler::Bicubic | Upscaler::EwaLanczos => vec![],
            Upscaler::Fsrcnnx => vec![FSRCNNX_16],
            Upscaler::FsrcnnxFast => vec![FSRCNNX_8],
            Upscaler::ArtCnn => vec![ARTCNN],
            Upscaler::ArtCnnDenoise => vec![ARTCNN_DS],
            Upscaler::Ravu => vec![RAVU],
            Upscaler::Fsr => vec![FSR],
            Upscaler::Nis => vec![NIS],
            Upscaler::Anime4K => anime4k(self.anime4k.0, self.anime4k.1),
            Upscaler::CustomShader => vec![],
        };
        parts.extend(main.into_iter().map(|s| s.as_bytes().into()));
        if let Some(p) = &self.file {
            let data = std::fs::read(p).map_err(|e| anyhow::anyhow!("cannot read shader {}: {e}", p.display()))?;
            parts.push(data.into());
        }
        if self.sharpen {
            parts.push(CAS.as_bytes().into());
        }
        let mut out = Vec::with_capacity(parts.iter().map(|p| p.len() + 1).sum());
        for p in parts {
            out.extend_from_slice(&p);
            out.push(b'\n');
        }
        Ok(out)
    }
}

/// Identifies the GPU upscaling settings, to remember ones that failed.
#[derive(Clone, Debug, PartialEq)]
pub struct ShaderSetKey(Option<ShaderSet>);

impl ShaderSetKey {
    pub fn of(cfg: &EngineConfig) -> Self {
        Self(ShaderSet::from_config(cfg))
    }
}

/// The libplacebo part of the video chain: the filter description and the
/// shaders to give it.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub desc: String,
    pub shaders: ShaderSet,
}

/// Whether FFmpeg was built with libplacebo.
pub fn available() -> bool {
    av::filter_available("libplacebo")
}

/// What to append to the chain for a `w`×`h` picture (pixel aspect `sar`)
/// shown in `target`, or `None` when the picture isn't being enlarged or no
/// GPU upscaler is chosen.
pub fn plan(cfg: &EngineConfig, w: i32, h: i32, sar: f64, target: (u32, u32)) -> Option<Plan> {
    let shaders = ShaderSet::from_config(cfg)?;
    if !available() || w <= 0 || h <= 0 {
        return None;
    }
    let target = (target.0.min(MAX_DIM), target.1.min(MAX_DIM));
    let (dw, dh) = output_size(w, h, sar, target, true);
    let (nw, nh) = output_size(w, h, sar, (u32::MAX / 2, u32::MAX / 2), false);
    // Shrinking (and near 1:1) stays with swscale in the presenter.
    if dw <= nw + 2 && dh <= nh + 2 {
        return None;
    }
    let (dw, dh) = (dw & !1, dh & !1);
    let upscaler = if cfg.upscaler == Upscaler::EwaLanczos { "ewa_lanczossharp" } else { "spline36" };
    let desc = format!("libplacebo=w={dw}:h={dh}:upscaler={upscaler}:format=yuv444p,setsar=1");
    Some(Plan { desc, shaders })
}

/// Configures a filter of a graph being built for `plan`: the libplacebo
/// filter gets the shared Vulkan device, the shaders and the shader cache.
pub fn setup_filter(ctx: *mut ff::AVFilterContext, plan: &Plan) -> anyhow::Result<()> {
    unsafe {
        let name = av::from_cstr((*(*ctx).filter).name).unwrap_or_default();
        if name != "libplacebo" {
            return Ok(());
        }
        if let Some(dev) = device() {
            (*ctx).hw_device_ctx = dev; // ownership moves to the filter
        }
        let src = plan.shaders.source()?;
        if src.iter().any(|b| !b.is_ascii_whitespace()) {
            let k = av::cstr("custom_shader_bin");
            av::check(
                ff::av_opt_set_bin(
                    ctx as *mut c_void,
                    k.as_ptr(),
                    src.as_ptr(),
                    src.len() as c_int,
                    ff::AV_OPT_SEARCH_CHILDREN as c_int,
                ),
                "custom_shader_bin",
            )?;
        }
        if let Some(cache) = shader_cache() {
            let (k, v) = (av::cstr("shader_cache"), av::cstr(cache));
            ff::av_opt_set(ctx as *mut c_void, k.as_ptr(), v.as_ptr(), ff::AV_OPT_SEARCH_CHILDREN as c_int);
        }
    }
    Ok(())
}

/// Prefix for libplacebo's compiled-shader cache files, which saves most
/// of a second each time a shader graph is built.
fn shader_cache() -> Option<&'static str> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let dir = dirs::cache_dir()?.join("mavplayer").join("libplacebo");
            std::fs::create_dir_all(&dir).ok()?;
            let mut s = dir.to_str()?.to_string();
            s.push(std::path::MAIN_SEPARATOR);
            Some(s)
        })
        .as_deref()
}

struct Device(*mut ff::AVBufferRef);
unsafe impl Send for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { ff::av_buffer_unref(&mut self.0) };
    }
}

enum Slot {
    Untried,
    Unavailable,
    /// The device, and when it was last seen in use.
    Ready(Device, std::time::Instant),
}

/// One Vulkan device for the whole process. Creating one takes a second or
/// more on some drivers; without it each libplacebo filter makes its own.
static DEVICE: parking_lot::Mutex<Slot> = parking_lot::Mutex::new(Slot::Untried);

/// A new reference to the shared Vulkan device, creating it on first use.
fn device() -> Option<*mut ff::AVBufferRef> {
    let mut slot = DEVICE.lock();
    if let Slot::Untried = *slot {
        let t0 = std::time::Instant::now();
        let mut buf = ptr::null_mut();
        let r = unsafe {
            ff::av_hwdevice_ctx_create(
                &mut buf,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                ptr::null(),
                ptr::null_mut(),
                0,
            )
        };
        *slot = if r < 0 || buf.is_null() {
            log::warn!("no Vulkan device for GPU upscaling: {}", av::err_str(r));
            Slot::Unavailable
        } else {
            log::debug!("upscale: Vulkan device ready in {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
            Slot::Ready(Device(buf), std::time::Instant::now())
        };
    }
    match &mut *slot {
        Slot::Ready(d, used) => {
            *used = std::time::Instant::now();
            let r = unsafe { ff::av_buffer_ref(d.0) };
            (!r.is_null()).then_some(r)
        }
        _ => None,
    }
}

/// Closes the Vulkan device once no filter has held it for `idle` (call it
/// every second or so). It is tens of MB of driver memory.
pub fn release_idle(idle: std::time::Duration) {
    let Some(mut slot) = DEVICE.try_lock() else { return };
    let Slot::Ready(d, used) = &mut *slot else { return };
    if unsafe { ff::av_buffer_get_ref_count(d.0) } > 1 {
        *used = std::time::Instant::now();
    } else if used.elapsed() >= idle {
        log::debug!("upscale: closing the idle Vulkan device");
        *slot = Slot::Untried;
    }
}

/// Starts creating the Vulkan device in the background when a GPU upscaler
/// is chosen, so the first picture doesn't wait for it.
pub fn prewarm(cfg: &EngineConfig) {
    if cfg.upscaler.needs_libplacebo() && available() {
        std::thread::Builder::new()
            .name("vulkan-init".into())
            .spawn(|| {
                if let Some(mut d) = device() {
                    unsafe { ff::av_buffer_unref(&mut d) };
                }
            })
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(upscaler: Upscaler) -> EngineConfig {
        EngineConfig { upscaler, ..EngineConfig::default() }
    }

    #[test]
    fn only_enlarging_uses_libplacebo() {
        if !available() {
            return;
        }
        let c = cfg(Upscaler::Fsrcnnx);
        let p = plan(&c, 1280, 720, 1.0, (2560, 1440)).unwrap();
        assert!(p.desc.starts_with("libplacebo=w=2560:h=1440:"), "{}", p.desc);
        assert!(plan(&c, 1920, 1080, 1.0, (1280, 720)).is_none());
        assert!(plan(&c, 1920, 1080, 1.0, (1920, 1080)).is_none());
        assert!(plan(&cfg(Upscaler::Bilinear), 640, 360, 1.0, (2560, 1440)).is_none());
        // Anamorphic DVD: square pixels at the right shape.
        let p = plan(&c, 720, 480, 32.0 / 27.0, (1706, 960)).unwrap();
        assert!(p.desc.starts_with("libplacebo=w=1706:h=960:"), "{}", p.desc);
        // A custom shader without a file is nothing to do.
        assert!(plan(&cfg(Upscaler::CustomShader), 640, 360, 1.0, (2560, 1440)).is_none());
    }

    #[test]
    fn shader_sets_are_complete_hook_files() {
        for u in Upscaler::ALL.into_iter().filter(|u| u.needs_libplacebo() && *u != Upscaler::CustomShader) {
            for mode in Anime4KMode::ALL {
                for quality in [Anime4KQuality::High, Anime4KQuality::Fast] {
                    let set =
                        ShaderSet { upscaler: u, anime4k: (mode, quality), file: None, chroma: true, sharpen: true };
                    let src = String::from_utf8(set.source().unwrap()).unwrap();
                    assert!(src.contains("//!HOOK CHROMA"), "{u:?}: KrigBilateral missing");
                    assert!(src.contains("Contrast Adaptive Sharpening"), "{u:?}: CAS missing");
                    if u != Upscaler::EwaLanczos {
                        assert!(src.matches("//!HOOK").count() > 4, "{u:?} {mode:?}");
                    }
                }
            }
        }
        // Anime4K presets match mpv's lists in length.
        assert_eq!(anime4k(Anime4KMode::A, Anime4KQuality::High).len(), 6);
        assert_eq!(anime4k(Anime4KMode::BB, Anime4KQuality::Fast).len(), 7);
        assert_eq!(anime4k(Anime4KMode::CA, Anime4KQuality::Fast).len(), 6);
    }

    /// Builds real graphs (when this machine has libplacebo and Vulkan) and
    /// checks frames come out display-sized; also times a rebuild.
    #[test]
    fn shader_graph_upscales_frames() {
        use super::super::av::Frame;
        use super::super::filters::Graph;
        match device() {
            Some(mut d) if available() => unsafe { ff::av_buffer_unref(&mut d) },
            Some(mut d) => {
                unsafe { ff::av_buffer_unref(&mut d) };
                eprintln!("skipped: no libplacebo/Vulkan");
                return;
            }
            None => {
                eprintln!("skipped: no libplacebo/Vulkan");
                return;
            }
        }
        for (u, mode) in [
            (Upscaler::Anime4K, Anime4KMode::A),
            (Upscaler::Fsrcnnx, Anime4KMode::A),
            (Upscaler::EwaLanczos, Anime4KMode::A),
        ] {
            let c = EngineConfig { upscaler: u, anime4k_mode: mode, shader_chroma: true, ..EngineConfig::default() };
            let p = plan(&c, 320, 180, 1.0, (1280, 720)).unwrap();
            let args = format!(
                "video_size=320x180:pix_fmt={}:time_base=1/25:pixel_aspect=1/1",
                ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32
            );
            let t0 = std::time::Instant::now();
            let mut g = Graph::with_setup("buffer", &args, "buffersink", &p.desc, 2, &mut |f| setup_filter(f, &p))
                .unwrap_or_else(|e| panic!("{u:?}: {e}"));
            eprintln!("{u:?}: graph built in {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
            let mut f = Frame::new();
            unsafe {
                (*f.0).width = 320;
                (*f.0).height = 180;
                (*f.0).format = ff::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int;
                assert!(ff::av_frame_get_buffer(f.0, 0) >= 0);
                for (i, v) in [(0usize, 100u8), (1, 128), (2, 128)] {
                    let rows = if i == 0 { 180 } else { 90 };
                    ptr::write_bytes((*f.0).data[i], v, (*f.0).linesize[i] as usize * rows);
                }
            }
            // Playback feeds a steady stream; libplacebo times its output
            // from the frames' durations.
            for pts in 0..3 {
                unsafe {
                    (*f.0).pts = pts;
                    (*f.0).duration = 1;
                }
                g.push(&mut f).unwrap();
            }
            g.push_eof();
            let out = Frame::new();
            let r = unsafe { ff::av_buffersink_get_frame(g.sink, out.0) };
            assert!(r >= 0, "{u:?}: no frame out: {}", super::super::av::err_str(r));
            assert_eq!((out.width(), out.height()), (1280, 720), "{u:?}");
            assert_eq!(out.format(), ff::AVPixelFormat::AV_PIX_FMT_YUV444P as c_int);
        }
    }
}
