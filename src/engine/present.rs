//! The presenter thread: shows each decoded frame at the right moment
//! relative to the master clock, converting it to BGRA just in time.

use super::av::{self, Frame, ff};
use super::filters::Graph;
use super::sync::{VideoFrame, now};
use super::types::*;
use super::{SeekRequest, SessionCtx};
use std::os::raw::{c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const SYNC_THRESHOLD_MIN: f64 = 0.04;
const SYNC_THRESHOLD_MAX: f64 = 0.1;
const FRAMEDUP_THRESHOLD: f64 = 0.1;
const NOSYNC_THRESHOLD: f64 = 10.0;
const MAX_FRAME_DURATION: f64 = 10.0;
/// Border around presented pictures (see [`extend_edges`]). One pixel is
/// all bilinear sampling reaches.
const PRESENT_PAD: u32 = 1;

/// Cached swscale context for YUV → BGRA (+ scale) conversion, and the GPU
/// conversion for pictures still on the GPU ([`GpuConvert`]).
pub struct Converter {
    sws: *mut ff::SwsContext,
    key: (i32, i32, c_int, u32, u32, c_int, c_int, i32),
    /// The output's alpha channel needs forcing to opaque.
    fix_alpha: bool,
    /// U and V planes split out of NV12/NV21 chroma (see [`split_chroma`]).
    chroma: Vec<u8>,
    gpu: GpuConvert,
    /// When a picture was last converted on the GPU.
    gpu_used: std::time::Instant,
}
unsafe impl Send for Converter {}

/// Set once converting on the GPU has failed to start (FFmpeg before 9.1,
/// whose `scale_cuda` has no RGB output): pictures are downloaded instead.
static GPU_CONVERT_BROKEN: AtomicBool = AtomicBool::new(false);

/// Whether the presenter can convert pictures that are still on the GPU
/// there (assumed until it fails), so the video thread may leave them there.
pub fn gpu_convert_ok() -> bool {
    !GPU_CONVERT_BROKEN.load(Ordering::Relaxed)
}

/// Converts a hardware (CUDA) picture to BGRA on the GPU with `scale_cuda`
/// and downloads the result straight into the presented image.
///
/// The queue's pictures are window-sized NV12 on the GPU
/// (`decoder::GpuScaler`). Downloading them and converting with swscale took
/// the presenter 1–2 ms a frame at 1080p and kept the queue in system memory;
/// here the CPU only copies the finished picture out of the driver's staging
/// buffer. The colours match swscale's (checked for BT.601, BT.709, full
/// range and 10-bit sources), and the alpha channel comes out opaque.
struct GpuConvert {
    graph: Option<(GpuConvertKey, Graph)>,
    out: Frame,
}

#[derive(Clone, Copy, PartialEq)]
struct GpuConvertKey {
    frames: usize,
    src: (i32, i32),
    dst: (u32, u32),
    algo: &'static str,
    colorspace: c_int,
    range: c_int,
}

impl GpuConvert {
    /// `f` to `dw`×`dh` BGRA, inside a `pad`-pixel border; `None` if the GPU
    /// can't (`GPU_CONVERT_BROKEN` says whether it ever will).
    fn convert(&mut self, f: &Frame, dw: u32, dh: u32, pad: u32, quality: ScaleQuality) -> Option<Vec<u8>> {
        let r = f.raw();
        // A plain colour conversion: bilinear keeps every luma sample as it
        // is and interpolates the chroma (swscale's fast path repeats it).
        let algo = match quality {
            _ if (dw as i32, dh as i32) == (r.width, r.height) => "bilinear",
            ScaleQuality::Nearest => "nearest",
            ScaleQuality::Linear => "bilinear",
            ScaleQuality::Good => "bicubic",
            ScaleQuality::Best => "lanczos",
        };
        let key = GpuConvertKey {
            frames: unsafe { (*r.hw_frames_ctx).data } as usize,
            src: (r.width, r.height),
            dst: (dw, dh),
            algo,
            colorspace: r.colorspace as c_int,
            range: r.color_range as c_int,
        };
        if self.graph.as_ref().is_none_or(|(k, _)| *k != key) {
            self.graph = None;
            let args = format!(
                "video_size={}x{}:pix_fmt={}:time_base=1/1000:pixel_aspect=1/1:colorspace={}:range={}",
                r.width, r.height, r.format, key.colorspace, key.range
            );
            let chain = format!("scale_cuda=w={dw}:h={dh}:interp_algo={algo}:format=bgra");
            match Graph::hw(&args, r.hw_frames_ctx, &chain) {
                Ok(g) => self.graph = Some((key, g)),
                Err(e) => {
                    log::debug!("video: converting on the GPU unavailable, downloading pictures instead: {e:#}");
                    crate::engine::present::GPU_CONVERT_BROKEN.store(true, std::sync::atomic::Ordering::Relaxed);
                    return None;
                }
            }
        }
        let (_, g) = self.graph.as_mut()?;
        let mut src = f.clone_ref()?;
        if g.push(&mut src).is_err() || !g.pull(&mut self.out) {
            self.graph = None;
            return None;
        }
        let (pad, w, h) = (pad as usize, dw as usize, dh as usize);
        let stride = (w + 2 * pad) * 4;
        let len = stride * (h + 2 * pad);
        // Not zeroed: the download writes every byte of the picture, and
        // the border is copied from it. Pinned, the GPU writes it directly;
        // otherwise the driver copies through a staging buffer of its own
        // (6.5% of a core for 1080p60, a quarter of one at 4K).
        let mut out = match crate::mem::frame_buffer(len) {
            Some((v, pinned)) => {
                if !pinned && pin::pin(v.as_ptr(), v.capacity(), self.out.raw().hw_frames_ctx) {
                    crate::mem::mark_pinned(v.as_ptr(), pin::unpin);
                }
                v
            }
            None => Vec::with_capacity(len),
        };
        let dst = Frame::new();
        // No unpinning (another session's presenter letting go of the GPU)
        // while the GPU writes.
        let busy = pin::busy();
        let r = unsafe {
            let d = &mut *dst.0;
            // A frame without a buffer gets one of FFmpeg's own, and the
            // pointers set here are ignored: lend it `out`.
            d.buf[0] = ff::av_buffer_create(out.as_mut_ptr(), len, Some(borrowed), ptr::null_mut(), 0);
            if d.buf[0].is_null() {
                self.out.unref();
                return None;
            }
            d.format = ff::AVPixelFormat::AV_PIX_FMT_BGRA as c_int;
            d.width = dw as c_int;
            d.height = dh as c_int;
            d.data[0] = out.as_mut_ptr().add(pad * stride + pad * 4);
            d.linesize[0] = stride as c_int;
            ff::av_hwframe_transfer_data(dst.0, self.out.0, 0)
        };
        drop(busy);
        drop(dst);
        if r < 0 {
            self.out.unref();
            log::debug!("video: downloading a converted picture failed: {}", av::err_str(r));
            return None;
        }
        // SAFETY: the download filled the picture; extend_edges fills the
        // border from it before anything reads it.
        unsafe { out.set_len(len) };
        self.out.unref();
        if pad > 0 {
            extend_edges(&mut out, w, h, pad);
        }
        Some(out)
    }
}

/// Page-locking picture buffers (`mem::frame_buffer`) for CUDA, through the
/// driver library FFmpeg loaded.
#[cfg(target_os = "linux")]
mod pin {
    use super::ff;
    use std::os::raw::{c_int, c_uint, c_void};
    use std::sync::OnceLock;

    struct Cuda {
        register: unsafe extern "C" fn(*mut c_void, usize, c_uint) -> c_int,
        unregister: unsafe extern "C" fn(*mut c_void) -> c_int,
        push: unsafe extern "C" fn(ff::CUcontext) -> c_int,
        pop: unsafe extern "C" fn(*mut ff::CUcontext) -> c_int,
    }

    fn cuda() -> Option<&'static Cuda> {
        static CUDA: OnceLock<Option<Cuda>> = OnceLock::new();
        CUDA.get_or_init(|| unsafe {
            let lib = libc::dlopen(c"libcuda.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD);
            if lib.is_null() {
                return None;
            }
            let f = |name: &std::ffi::CStr| libc::dlsym(lib, name.as_ptr());
            let (r, u, pu, po) = (
                f(c"cuMemHostRegister_v2"),
                f(c"cuMemHostUnregister"),
                f(c"cuCtxPushCurrent_v2"),
                f(c"cuCtxPopCurrent_v2"),
            );
            // The library stays loaded: FFmpeg holds it while a device is open,
            // and the reference taken here keeps it for good.
            if r.is_null() || u.is_null() || pu.is_null() || po.is_null() {
                return None;
            }
            Some(Cuda {
                register: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void, usize, c_uint) -> c_int>(
                    r,
                ),
                unregister: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void) -> c_int>(u),
                push: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(ff::CUcontext) -> c_int>(pu),
                pop: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut ff::CUcontext) -> c_int>(po),
            })
        })
        .as_ref()
    }

    /// The CUDA device buffers were pinned through (a reference of our own,
    /// so it stays open until they are unpinned), and how many are pinned.
    static DEVICE: parking_lot::Mutex<(usize, usize)> = parking_lot::Mutex::new((0, 0));

    /// `CU_MEMHOSTREGISTER_PORTABLE`: pinned for every CUDA context.
    const PORTABLE: c_uint = 1;

    unsafe fn context(dev: *mut ff::AVBufferRef) -> ff::CUcontext {
        unsafe {
            let hw = (*dev).data as *const ff::AVHWDeviceContext;
            (*((*hw).hwctx as *const ff::AVCUDADeviceContext)).cuda_ctx
        }
    }

    /// Pins `size` bytes at `ptr` for the device of `frames` (a CUDA frames
    /// context). False if it can't.
    pub fn pin(ptr: *const u8, size: usize, frames: *mut ff::AVBufferRef) -> bool {
        let Some(cu) = cuda() else { return false };
        let mut d = DEVICE.lock();
        unsafe {
            let fc = (*frames).data as *const ff::AVHWFramesContext;
            let dev = (*fc).device_ref;
            // Pins through another device are still held: don't mix.
            if d.0 != 0 && (*(d.0 as *mut ff::AVBufferRef)).data != (*dev).data {
                return false;
            }
            if (cu.push)(context(dev)) != 0 {
                return false;
            }
            let r = (cu.register)(ptr as *mut c_void, size, PORTABLE);
            let mut popped = std::ptr::null_mut();
            (cu.pop)(&mut popped);
            if r != 0 {
                log::debug!("video: pinning a picture buffer failed ({r})");
                return false;
            }
            if d.0 == 0 {
                d.0 = ff::av_buffer_ref(dev) as usize;
            }
        }
        d.1 += 1;
        true
    }

    /// Holds off [`unpin`] while the GPU writes into a pinned buffer.
    pub fn busy() -> parking_lot::MutexGuard<'static, (usize, usize)> {
        DEVICE.lock()
    }

    /// Unpins a buffer [`pin`] pinned (`mem` calls it before unmapping).
    pub fn unpin(ptr: *mut u8, _size: usize) {
        let Some(cu) = cuda() else { return };
        let mut d = DEVICE.lock();
        if d.0 == 0 {
            return;
        }
        unsafe {
            let dev = d.0 as *mut ff::AVBufferRef;
            if (cu.push)(context(dev)) == 0 {
                (cu.unregister)(ptr as *mut c_void);
                let mut popped = std::ptr::null_mut();
                (cu.pop)(&mut popped);
            }
            d.1 = d.1.saturating_sub(1);
            if d.1 == 0 {
                let mut dev = dev;
                ff::av_buffer_unref(&mut dev);
                d.0 = 0;
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod pin {
    pub fn pin(_: *const u8, _: usize, _: *mut super::ff::AVBufferRef) -> bool {
        false
    }
    pub fn unpin(_: *mut u8, _: usize) {}
    pub struct Busy;
    pub fn busy() -> Busy {
        Busy
    }
}

/// The free callback of a buffer that only borrows its memory.
unsafe extern "C" fn borrowed(_: *mut c_void, _: *mut u8) {}

impl Converter {
    pub fn new() -> Self {
        Self {
            sws: ptr::null_mut(),
            key: (0, 0, -1, 0, 0, -1, -1, 0),
            fix_alpha: true,
            chroma: Vec::new(),
            gpu: GpuConvert { graph: None, out: Frame::new() },
            gpu_used: std::time::Instant::now(),
        }
    }

    /// Drops the GPU conversion if it went unused for `idle`, returning
    /// whether it did (see `GPU_IDLE`).
    fn release_gpu_if_idle(&mut self, idle: Duration) -> bool {
        if self.gpu.graph.is_none() || self.gpu_used.elapsed() < idle {
            return false;
        }
        self.gpu = GpuConvert { graph: None, out: Frame::new() };
        true
    }

    /// Converts `f` to tightly-packed BGRA of `dw`×`dh`, surrounded by a
    /// border of `pad` pixels that repeat the picture's outermost pixels (so
    /// the image is `dw + 2*pad` × `dh + 2*pad`).
    pub fn convert(&mut self, f: &Frame, dw: u32, dh: u32, pad: u32, quality: ScaleQuality) -> Option<Vec<u8>> {
        if !f.raw().hw_frames_ctx.is_null() {
            self.gpu_used = std::time::Instant::now();
            if gpu_convert_ok()
                && let Some(out) = self.gpu.convert(f, dw, dh, pad, quality)
            {
                return Some(out);
            }
            // Download it and convert it here.
            let sw = Frame::new();
            unsafe {
                if ff::av_hwframe_transfer_data(sw.0, f.0, 0) < 0 || ff::av_frame_copy_props(sw.0, f.0) < 0 {
                    return None;
                }
            }
            return self.convert(&sw, dw, dh, pad, quality);
        }
        let r = f.raw();
        let scaled = dw as i32 != r.width || dh as i32 != r.height;
        let flags = match quality {
            ScaleQuality::Nearest => ff::SwsFlags::SWS_POINT,
            ScaleQuality::Linear => ff::SwsFlags::SWS_BILINEAR,
            ScaleQuality::Good => ff::SwsFlags::SWS_BICUBIC,
            ScaleQuality::Best => ff::SwsFlags::SWS_LANCZOS,
        } as c_int;
        // The quality setting is for resizing. Without a resize this is a
        // plain colour conversion, and only swscale's unscaled SIMD path
        // (which exact rounding turns off) keeps it cheap: 0.7 ms for a 1080p
        // frame instead of 6-15 ms. "Linear" always takes the fast paths.
        // "Good" adds exact rounding, and "Best" (and nearest neighbour, for
        // hard-edged enlarging) also full horizontal chroma, which doubles
        // the cost of a downscale.
        let accurate = ff::SwsFlags::SWS_ACCURATE_RND as c_int;
        let full_chroma = ff::SwsFlags::SWS_FULL_CHR_H_INT as c_int;
        let flags = match quality {
            _ if !scaled => flags,
            ScaleQuality::Linear => flags,
            ScaleQuality::Good => flags | accurate,
            ScaleQuality::Best | ScaleQuality::Nearest => flags | accurate | full_chroma,
        };
        // swscale has no fast path from NV12 (what hardware decoders hand
        // over) to RGB, but a fast one from planar YUV: split the chroma
        // into planes first, which costs a fraction of the difference.
        let split = !scaled && flags & accurate == 0 && nv_planar(r.format).is_some();
        let src_format = match nv_planar(r.format) {
            Some(planar) if split => planar,
            _ => r.format,
        };
        let key = (r.width, r.height, src_format, dw, dh, r.colorspace as c_int, r.color_range as c_int, flags);
        unsafe {
            if self.sws.is_null() || self.key != key {
                ff::sws_freeContext(self.sws);
                self.sws = ptr::null_mut();
                let src_fmt: ff::AVPixelFormat = std::mem::transmute(r.format);
                let ctx = ff::sws_alloc_context();
                if ctx.is_null() {
                    return None;
                }
                let set = |k: &str, v: i64| {
                    let k = av::cstr(k);
                    ff::av_opt_set_int(ctx as *mut c_void, k.as_ptr(), v, 0);
                };
                set("srcw", r.width as i64);
                set("srch", r.height as i64);
                set("src_format", src_format as i64);
                set("dstw", dw as i64);
                set("dsth", dh as i64);
                set("dst_format", ff::AVPixelFormat::AV_PIX_FMT_BGRA as i64);
                set("sws_flags", flags as i64);
                let threads = std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4);
                set("threads", threads as i64);
                if ff::sws_init_context(ctx, ptr::null_mut(), ptr::null_mut()) < 0 {
                    ff::sws_freeContext(ctx);
                    log::warn!("swscale cannot convert from {}", av::pix_fmt_name(r.format));
                    return None;
                }
                // Correct YUV matrix: BT.709 for HD unless tagged otherwise.
                let mut cs = r.colorspace as c_int;
                if cs == ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED as c_int
                    || cs == ff::AVColorSpace::AVCOL_SPC_RESERVED as c_int
                {
                    cs = if r.height > 576 { ff::SWS_CS_ITU709 as c_int } else { ff::SWS_CS_ITU601 as c_int };
                }
                let full_src = r.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG || is_rgb_or_jpeg(src_fmt);
                ff::sws_setColorspaceDetails(
                    ctx,
                    ff::sws_getCoefficients(cs),
                    full_src as c_int,
                    ff::sws_getCoefficients(ff::SWS_CS_DEFAULT as c_int),
                    1,
                    0,
                    1 << 16,
                    1 << 16,
                );
                self.sws = ctx;
                self.key = key;
                self.fix_alpha = !opaque_yuv(r.format);
            }
            let (pad, w, h) = (pad as usize, dw as usize, dh as usize);
            let stride = (w + 2 * pad) * 4;
            let len = stride * (h + 2 * pad);
            let first = pad * stride + pad * 4;
            // Not zeroed first: swscale writes every byte of the picture and
            // the border is copied from it, and clearing a 4K frame's 33 MB
            // each time costs as much as a fast conversion.
            let mut out: Vec<u8> = Vec::with_capacity(len);
            let dst_data = [out.as_mut_ptr().add(first), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
            let dst_stride = [stride as c_int, 0, 0, 0];
            let (mut src_data, mut src_stride) = (r.data, r.linesize);
            if split {
                let (cw, ch) = (((r.width + 1) / 2) as usize, ((r.height + 1) / 2) as usize);
                let swap = r.format == ff::AVPixelFormat::AV_PIX_FMT_NV21 as c_int;
                split_chroma(r.data[1], r.linesize[1] as isize, cw, ch, swap, &mut self.chroma);
                let (u, v) = self.chroma.split_at_mut(cw * ch);
                src_data[1] = u.as_mut_ptr();
                src_data[2] = v.as_mut_ptr();
                src_stride[1] = cw as c_int;
                src_stride[2] = cw as c_int;
            }
            let done = ff::sws_scale(
                self.sws,
                src_data.as_ptr() as *const *const u8,
                src_stride.as_ptr(),
                0,
                r.height,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            );
            if done <= 0 {
                return None;
            }
            // Rows swscale didn't produce (never expected for a whole frame).
            let done = (done as usize).min(h);
            if done < h {
                ptr::write_bytes(out.as_mut_ptr().add((pad + done) * stride), 0, (h - done) * stride);
            }
            out.set_len(len);
            // Force opaque alpha. swscale already writes 255 when converting
            // from YUV without alpha (nearly all video), so only RGB(X) and
            // alpha sources need this full-frame pass.
            if self.fix_alpha {
                for px in out.as_chunks_mut::<4>().0 {
                    px[3] = 255;
                }
            }
            if pad > 0 {
                extend_edges(&mut out, w, h, pad);
            }
            Some(out)
        }
    }
}

/// Fills the `pad`-pixel border around a `w`×`h` BGRA picture (stored with
/// that border, row stride `(w + 2*pad) * 4`) with copies of its edge pixels.
///
/// The GPU samples pictures with bilinear filtering, which near an edge
/// blends in the texel beyond it. GPUI packs images into shared atlas
/// textures without any gap, so without this border the picture's outermost
/// rows and columns picked up the neighbouring video frame or uninitialised
/// texture memory, and flickered as frames alternated between atlas slots.
fn extend_edges(buf: &mut [u8], w: usize, h: usize, pad: usize) {
    let stride = (w + 2 * pad) * 4;
    for row in buf.chunks_exact_mut(stride).skip(pad).take(h) {
        let (l, r) = (pad * 4, (pad + w - 1) * 4);
        let (left, right): ([u8; 4], [u8; 4]) = (row[l..l + 4].try_into().unwrap(), row[r..r + 4].try_into().unwrap());
        for px in row[..l].as_chunks_mut::<4>().0 {
            *px = left;
        }
        for px in row[r + 4..].as_chunks_mut::<4>().0 {
            *px = right;
        }
    }
    let (top, bottom) = (pad * stride, (pad + h - 1) * stride);
    for y in 0..pad {
        buf.copy_within(top..top + stride, y * stride);
        buf.copy_within(bottom..bottom + stride, (pad + h + y) * stride);
    }
}

/// The planar layout of a semi-planar 8-bit 4:2:0 format (NV12, NV21).
fn nv_planar(fmt: c_int) -> Option<c_int> {
    use ff::AVPixelFormat::*;
    (fmt == AV_PIX_FMT_NV12 as c_int || fmt == AV_PIX_FMT_NV21 as c_int).then_some(AV_PIX_FMT_YUV420P as c_int)
}

/// Splits interleaved chroma (`cw` pairs per row, `ch` rows, rows `stride`
/// bytes apart, which is negative for flipped pictures) into a U plane
/// followed by a V plane in `out`. `swap` is for NV21's V-first order.
fn split_chroma(src: *const u8, stride: isize, cw: usize, ch: usize, swap: bool, out: &mut Vec<u8>) {
    out.resize(cw * ch * 2, 0);
    let (u, v) = out.split_at_mut(cw * ch);
    let (u, v) = if swap { (v, u) } else { (u, v) };
    for (y, (ur, vr)) in u.chunks_exact_mut(cw).zip(v.chunks_exact_mut(cw)).enumerate() {
        // SAFETY: the frame's chroma plane holds `ch` rows of at least
        // `2 * cw` bytes, `stride` apart.
        let row = unsafe { std::slice::from_raw_parts(src.offset(y as isize * stride), cw * 2) };
        for ((pair, u), v) in row.as_chunks::<2>().0.iter().zip(ur).zip(vr) {
            *u = pair[0];
            *v = pair[1];
        }
    }
}

/// A YUV (or gray) format without alpha: swscale's conversion to BGRA
/// fills the alpha channel with 255 itself.
fn opaque_yuv(fmt: c_int) -> bool {
    unsafe {
        let d = ff::av_pix_fmt_desc_get(std::mem::transmute::<c_int, ff::AVPixelFormat>(fmt));
        if d.is_null() {
            return false;
        }
        let flags = (*d).flags;
        let bad =
            ff::AV_PIX_FMT_FLAG_RGB | ff::AV_PIX_FMT_FLAG_ALPHA | ff::AV_PIX_FMT_FLAG_PAL | ff::AV_PIX_FMT_FLAG_HWACCEL;
        flags & bad as u64 == 0
    }
}

fn is_rgb_or_jpeg(f: ff::AVPixelFormat) -> bool {
    use ff::AVPixelFormat::*;
    matches!(
        f,
        AV_PIX_FMT_YUVJ420P
            | AV_PIX_FMT_YUVJ422P
            | AV_PIX_FMT_YUVJ444P
            | AV_PIX_FMT_RGB24
            | AV_PIX_FMT_BGR24
            | AV_PIX_FMT_RGBA
            | AV_PIX_FMT_BGRA
            | AV_PIX_FMT_ARGB
            | AV_PIX_FMT_ABGR
            | AV_PIX_FMT_GBRP
            | AV_PIX_FMT_PAL8
    )
}

impl Drop for Converter {
    fn drop(&mut self) {
        unsafe { ff::sws_freeContext(self.sws) }
    }
}

/// Output size: square pixels, fitted inside the target. Never larger than
/// the source unless `enlarge` is set (otherwise the GPU does the enlarging).
///
/// A resized picture gets an even width: swscale's SIMD output paths need
/// one, and an odd width made every conversion about three times slower
/// (9 ms instead of 3 for 1080p into a 1447-pixel-wide window).
pub fn output_size(w: i32, h: i32, sar: f64, target: (u32, u32), enlarge: bool) -> (u32, u32) {
    let sar = if sar.is_finite() && sar > 0.0 { sar } else { 1.0 };
    let (mut dw, mut dh) =
        if sar >= 1.0 { ((w as f64 * sar).round(), h as f64) } else { (w as f64, (h as f64 / sar).round()) };
    let (tw, th) = (target.0.max(16) as f64, target.1.max(16) as f64);
    let fit = (tw / dw).min(th / dh);
    let scale = if enlarge { fit } else { fit.min(1.0) };
    dw = (dw * scale).round().max(2.0);
    dh = (dh * scale).round().max(2.0);
    let dw = dw as u32;
    let dw = if dw as i32 != w && dw % 2 == 1 { dw - 1 } else { dw };
    (dw, dh as u32)
}

/// The size a hardware-decoded `w`×`h` picture is resized to on the GPU for
/// `target` (see `decoder::GpuScaler`): [`output_size`] without enlarging,
/// with an even height too when resized (NV12 halves it for chroma).
pub fn gpu_size(w: i32, h: i32, sar: f64, target: (u32, u32)) -> (i32, i32) {
    let (dw, dh) = output_size(w, h, sar, target, false);
    let (dw, dh) = (dw as i32, dh as i32);
    let dh = if dh != h && dh % 2 == 1 { dh - 1 } else { dh };
    (dw, dh.max(2))
}

/// Once-a-second presenter statistics for `-v`, to see where video stalls.
struct Stats {
    since: std::time::Instant,
    shown: u32,
    dropped: u32,
    converted: u32,
    convert_time: Duration,
    size: (u32, u32),
}

impl Stats {
    fn new() -> Self {
        Self {
            since: std::time::Instant::now(),
            shown: 0,
            dropped: 0,
            converted: 0,
            convert_time: Duration::ZERO,
            size: (0, 0),
        }
    }

    fn report(&mut self, ctx: &SessionCtx) {
        if self.since.elapsed() < Duration::from_secs(1) || !log::log_enabled!(log::Level::Debug) {
            return;
        }
        if self.shown + self.dropped + self.converted > 0 || !ctx.paused() {
            log::debug!(
                "video: {} shown, {} dropped, {} converted at {}x{} (avg {:.1} ms); {} queued{}{}",
                self.shown,
                self.dropped,
                self.converted,
                self.size.0,
                self.size.1,
                self.convert_time.as_secs_f64() * 1000.0 / self.converted.max(1) as f64,
                ctx.frames.len(),
                if ctx.paused() { ", paused" } else { "" },
                if ctx.audio_pending() { ", waiting for audio" } else { "" },
            );
        }
        *self = Self::new();
    }
}

struct Prepared {
    serial: i32,
    pts: f64,
    /// What it was converted for: a resize or a quality change redoes it.
    look: Look,
    image: PresentedFrame,
}

/// Output size request, scaling filters (downscaling, and the one enlarging
/// is done with here, if any) a picture was converted with, and whether the
/// window was out of sight (nothing converted at all).
type Look = ((u32, u32), (ScaleQuality, Option<ScaleQuality>), bool);

fn current_look(ctx: &SessionCtx) -> Look {
    let hidden = ctx.controls.video_hidden.load(Ordering::Relaxed);
    let cfg = ctx.controls.config.lock();
    (ctx.controls.target_size(), (cfg.scale_quality, cfg.upscaler.presenter_filter()), hidden)
}

/// How often a paused picture is redrawn while the window is being resized.
const RESHOW_INTERVAL: Duration = Duration::from_millis(50);

/// How long a paused window must keep its new size before a picture resized
/// on the GPU for the old size is decoded again for it.
const REFRESH_SETTLE: Duration = Duration::from_millis(300);

pub fn run(ctx: Arc<SessionCtx>, stop: Arc<AtomicBool>) {
    let mut conv = Converter::new();
    let mut frame_timer = now();
    let mut last: Option<(f64, f64, i32)> = None; // (pts, duration, serial)
    let mut prepared: Option<Prepared> = None;
    let mut dropped: u64 = 0;
    let mut stats = Stats::new();
    // The look of the picture on screen, and when it was last redone.
    let mut shown_look: Option<Look> = None;
    let mut reshown = std::time::Instant::now();
    // The look last seen while paused and since when, and the look a paused
    // picture was last decoded again for.
    let mut paused_look: (Option<Look>, std::time::Instant) = (None, std::time::Instant::now());
    let mut refreshed: Option<Look> = None;
    // Cover art: the look last seen since the picture was shown and since
    // when, and the look it was last decoded again for.
    let mut cover_look: (Option<Look>, std::time::Instant) = (None, std::time::Instant::now());
    let mut cover_redone: Option<Look> = None;

    while ctx.alive() && !stop.load(Ordering::Acquire) {
        stats.report(&ctx);
        if conv.release_gpu_if_idle(GPU_IDLE) {
            let_go_of_gpu(&ctx);
        }
        let paused = ctx.paused();
        let step = ctx.step.load(Ordering::Acquire);
        let show_one = ctx.show_one.load(Ordering::Acquire);
        let serial = ctx.vq.serial();

        // Discard frames from before the latest seek.
        let head = ctx.frames.with_front(|a, b| a.map(|a| (a.pts, a.duration, a.serial, b.map(|b| (b.pts, b.serial)))));
        let Some((pts, dur, fserial, next)) = head else {
            // Cover art on screen for another window size or other scaling:
            // once that settles, have it decoded (and upscaled) again.
            if ctx.cover_art.load(Ordering::Acquire) && !ctx.is_standby() && shown_look.is_some() {
                let look = current_look(&ctx);
                if cover_look.0 != Some(look) {
                    cover_look = (Some(look), std::time::Instant::now());
                }
                if Some(look) != shown_look
                    && !look.2
                    && cover_redone != Some(look)
                    && cover_look.1.elapsed() >= REFRESH_SETTLE
                {
                    cover_redone = Some(look);
                    log::debug!("video: decoding the cover art again for a {}x{} window", look.0.0, look.0.1);
                    ctx.redo_cover();
                }
            }
            ctx.frames.wait(Duration::from_millis(10));
            continue;
        };
        if fserial != serial {
            ctx.frames.pop();
            continue;
        }

        // Preloaded next item: have the first picture ready for the switch.
        if ctx.is_standby() {
            ensure_prepared(&ctx, &mut conv, &mut prepared, serial, &mut stats);
            std::thread::sleep(Duration::from_millis(5));
            frame_timer = now();
            continue;
        }

        // While paused, or while the audio device is still opening, show the
        // first picture straight away but hold the rest (so a file opened
        // paused isn't black, and audio and video start together).
        let shown_this_serial = last.map(|l| l.2) == Some(fserial);
        let audio_pending = ctx.audio_pending();
        let hold = (paused || audio_pending) && shown_this_serial;
        if hold && !step && !show_one {
            // The window was resized (or the scaling changed) while paused:
            // redo the picture on screen rather than leave it stretched.
            let look = current_look(&ctx);
            if shown_look.is_some_and(|l| l != look) && !look.2 && reshown.elapsed() >= RESHOW_INTERVAL {
                reshown = std::time::Instant::now();
                shown_look = Some(look);
                let last =
                    ctx.last_frame.lock().as_ref().and_then(|l| l.frame.clone_ref().map(|f| (f, l.sar, l.source)));
                if let Some((frame, sar, source)) = last {
                    let vf = VideoFrame { frame, pts: 0.0, duration: 0.0, serial, sar, source };
                    if let Some(image) = prepare(&ctx, &mut conv, &vf, &mut stats)
                        && ctx.is_current()
                    {
                        *ctx.shared.frame_slot.lock() = Some(image);
                        ctx.wake();
                    }
                }
            }
            // The picture was resized on the GPU for the old window size:
            // what's on screen now is that one rescaled. Once the window
            // settles, decode it again for the new size (as a paused filter
            // change does, with a precise seek to the same position).
            if paused_look.0 != Some(look) {
                paused_look = (Some(look), std::time::Instant::now());
            }
            if paused
                && !look.2
                && refreshed != Some(look)
                && paused_look.1.elapsed() >= REFRESH_SETTLE
                && ctx.last_frame.lock().as_ref().is_some_and(|l| l.stale_for(look.0))
            {
                refreshed = Some(look);
                log::debug!("video: redoing the paused picture for a {}x{} window", look.0.0, look.0.1);
                ctx.request_seek(SeekRequest {
                    target: ctx.position(),
                    relative: false,
                    precise: true,
                    direction: 0,
                    origin: 0.0,
                });
            }
            // Keep the next picture converted so un-pausing is instant.
            ensure_prepared(&ctx, &mut conv, &mut prepared, serial, &mut stats);
            std::thread::sleep(Duration::from_millis(10));
            frame_timer = now();
            continue;
        }

        if last.map(|l| l.2) != Some(fserial) {
            frame_timer = now();
        }

        let speed = ctx.controls.speed().max(0.01);
        let last_duration = match last {
            Some((lp, ld, ls)) if ls == fserial => {
                let d = pts - lp;
                if !d.is_finite() || d <= 0.0 || d > MAX_FRAME_DURATION { ld } else { d }
            }
            _ => 0.0,
        } / speed;

        let instant = step || show_one || ((paused || audio_pending) && !shown_this_serial);
        let delay = if instant { 0.0 } else { target_delay(&ctx, last_duration) };
        let t = now();
        if !instant && t < frame_timer + delay {
            ensure_prepared(&ctx, &mut conv, &mut prepared, serial, &mut stats);
            let wait = (frame_timer + delay - now()).clamp(0.0, 0.01);
            if wait > 0.0005 {
                std::thread::sleep(Duration::from_secs_f64(wait));
            }
            continue;
        }
        frame_timer += delay;
        if delay > 0.0 && t - frame_timer > SYNC_THRESHOLD_MAX {
            frame_timer = t;
        }

        // Drop the frame if we're already late for the next one.
        if !instant
            && ctx.audio_is_master()
            && let Some((npts, nserial)) = next
            && nserial == fserial
        {
            let d = npts - pts;
            let dd = if d.is_finite() && d > 0.0 && d < MAX_FRAME_DURATION { d } else { dur } / speed;
            if t > frame_timer + dd {
                if let Some(vf) = ctx.frames.pop() {
                    last = Some((vf.pts, vf.duration, vf.serial));
                }
                prepared = None;
                dropped += 1;
                stats.dropped += 1;
                if let Some(mut st) = ctx.shared.state.try_lock()
                    && ctx.is_current()
                {
                    st.dropped_frames = dropped;
                }
                continue;
            }
        }

        ensure_prepared(&ctx, &mut conv, &mut prepared, serial, &mut stats);
        let Some(vf) = ctx.frames.pop() else { continue };
        let look = current_look(&ctx);
        let image = match prepared.take() {
            // Out of sight: keep time, skip the conversion.
            _ if look.2 => None,
            Some(p) if p.serial == vf.serial && p.pts.to_bits() == vf.pts.to_bits() && p.look == look => Some(p.image),
            _ => prepare(&ctx, &mut conv, &vf, &mut stats),
        };
        if vf.pts.is_finite() {
            ctx.vclk.set(vf.pts, vf.serial);
        }
        last = Some((vf.pts, vf.duration, vf.serial));
        if image.is_some() || look.2 {
            stats.shown += 1;
            shown_look = Some(look);
            if let Some(image) = image
                && ctx.is_current()
            {
                *ctx.shared.frame_slot.lock() = Some(image);
            }
            *ctx.last_frame.lock() = Some(LastFrame { frame: vf.frame, sar: vf.sar, source: vf.source });
            if ctx.seeking.swap(false, Ordering::AcqRel) {
                ctx.update_state(|st| st.seeking = false);
            }
            ctx.wake();
        }
        if step {
            ctx.step.store(false, Ordering::Release);
        }
        if show_one {
            ctx.show_one.store(false, Ordering::Release);
        }
    }
}

/// How long the presenter goes without converting a picture on the GPU
/// before it lets go of it: paused, or at the end of a file. The picture on
/// screen, the last frame and the pinned buffers would otherwise keep the
/// CUDA device (about 100 MB of video memory and 40 MB of RAM) open after
/// the decoder has closed (`decoder::release_idle_hw`).
const GPU_IDLE: Duration = Duration::from_secs(10);

fn let_go_of_gpu(ctx: &SessionCtx) {
    log::debug!("video: idle, letting go of the GPU");
    if let Some(l) = ctx.last_frame.lock().as_mut()
        && !l.frame.raw().hw_frames_ctx.is_null()
    {
        // Into a buffer of our own: one FFmpeg allocates for the download
        // comes from a pool that holds the device.
        let sw = Frame::new();
        // SAFETY: a fresh frame, given its own buffer, filled from a
        // hardware frame we hold.
        let ok = unsafe {
            let fc = (*l.frame.raw().hw_frames_ctx).data as *const ff::AVHWFramesContext;
            (*sw.0).format = (*fc).sw_format as c_int;
            (*sw.0).width = l.frame.width();
            (*sw.0).height = l.frame.height();
            ff::av_frame_get_buffer(sw.0, 0) >= 0
                && ff::av_hwframe_transfer_data(sw.0, l.frame.0, 0) >= 0
                && ff::av_frame_copy_props(sw.0, l.frame.0) >= 0
        };
        if ok {
            l.frame = sw;
        }
    }
    crate::mem::unpin_all();
}

fn ensure_prepared(
    ctx: &SessionCtx,
    conv: &mut Converter,
    prepared: &mut Option<Prepared>,
    serial: i32,
    stats: &mut Stats,
) {
    let head = ctx.frames.with_front(|a, _| a.map(|a| (a.pts, a.serial)));
    let Some((pts, s)) = head else { return };
    if s != serial {
        return;
    }
    let look = current_look(ctx);
    if look.2 {
        return;
    }
    if prepared.as_ref().is_some_and(|p| p.serial == s && p.pts.to_bits() == pts.to_bits() && p.look == look) {
        return;
    }
    // Convert outside the queue lock: clone a reference to the head frame.
    let fr = ctx
        .frames
        .with_front(|a, _| a.and_then(|a| a.frame.clone_ref().map(|f| (f, a.sar, a.pts, a.serial, a.source))));
    if let Some((f, sar, pts, serial, source)) = fr {
        let vf = VideoFrame { frame: f, pts, duration: 0.0, serial, sar, source };
        if let Some(image) = prepare(ctx, conv, &vf, stats) {
            *prepared = Some(Prepared { serial, pts, look, image });
        }
    }
}

fn prepare(ctx: &SessionCtx, conv: &mut Converter, vf: &VideoFrame, stats: &mut Stats) -> Option<PresentedFrame> {
    let (w, h) = (vf.frame.width(), vf.frame.height());
    if w <= 0 || h <= 0 {
        return None;
    }
    let target = ctx.controls.target_size();
    let (quality, upscaler) = {
        let c = ctx.controls.config.lock();
        (c.scale_quality, c.upscaler)
    };
    // The GPU enlarges with bilinear filtering when it draws the picture,
    // which would blur away nearest neighbour's hard edges and is softer
    // than bicubic: those enlarge here instead. (Shader upscalers already
    // made the picture display-sized.)
    let up = upscaler.presenter_filter();
    let (dw, dh) = output_size(w, h, vf.sar, target, up.is_some());
    let enlarging = dw > w as u32 || dh > h as u32;
    let quality = match up {
        Some(q) if enlarging => q,
        _ => quality,
    };
    let t0 = std::time::Instant::now();
    let bgra = conv.convert(&vf.frame, dw, dh, PRESENT_PAD, quality);
    stats.converted += 1;
    stats.convert_time += t0.elapsed();
    stats.size = (dw, dh);
    let Some(bgra) = bgra else {
        log::debug!("video: converting a {w}x{h} frame to {dw}x{dh} failed");
        return None;
    };
    // Bitmap subtitles are positioned on the picture as decoded.
    let (cw, ch) = vf.source.map_or((w, h), |(sw, sh, _)| (sw, sh));
    ctx.sub_canvas.store(((cw as u64) << 32) | ch as u64, Ordering::Relaxed);
    Some(PresentedFrame { width: dw, height: dh, pad: PRESENT_PAD, bgra })
}

/// ffplay's `compute_target_delay`: stretch or shrink the wait so the video
/// clock converges on the master (audio) clock.
fn target_delay(ctx: &SessionCtx, delay: f64) -> f64 {
    if !ctx.audio_is_master() {
        return delay;
    }
    let master = ctx.master_clock() + ctx.controls.audio_delay.get();
    let diff = ctx.vclk.get(ctx.vq.serial()) - master;
    let thr = delay.clamp(SYNC_THRESHOLD_MIN, SYNC_THRESHOLD_MAX);
    if !diff.is_finite() || diff.abs() >= NOSYNC_THRESHOLD {
        return delay;
    }
    if diff <= -thr {
        (delay + diff).max(0.0)
    } else if diff >= thr && delay > FRAMEDUP_THRESHOLD {
        delay + diff
    } else if diff >= thr {
        2.0 * delay
    } else {
        delay
    }
}

/// The picture on screen, as the presenter last showed it.
pub struct LastFrame {
    pub frame: Frame,
    pub sar: f64,
    /// See [`VideoFrame::source`].
    pub source: Option<(i32, i32, f64)>,
}

impl LastFrame {
    /// Whether this was resized on the GPU and a window of `target` would
    /// get a different size: it has to be decoded again for that.
    fn stale_for(&self, target: (u32, u32)) -> bool {
        self.source.is_some_and(|(w, h, sar)| gpu_size(w, h, sar, target) != (self.frame.width(), self.frame.height()))
    }
}

/// Saves the last shown frame at native resolution as PNG.
pub fn screenshot_async(ctx: Arc<SessionCtx>, path: PathBuf) {
    let frame = ctx.last_frame.lock().as_ref().and_then(|l| l.frame.clone_ref().map(|f| (f, l.sar)));
    let Some((frame, sar)) = frame else {
        ctx.emit(PlayerEvent::Warning("No picture to capture".into()));
        return;
    };
    save_screenshot(ctx, frame, sar, path);
}

/// Saves `frame` (in system memory) at its own resolution as PNG, on a
/// thread of its own.
pub fn save_screenshot(ctx: Arc<SessionCtx>, frame: Frame, sar: f64, path: PathBuf) {
    std::thread::spawn(move || {
        let mut conv = Converter::new();
        let (w, h) = output_size(frame.width(), frame.height(), sar, (u32::MAX / 2, u32::MAX / 2), false);
        let Some(mut bgra) = conv.convert(&frame, w, h, 0, ScaleQuality::Best) else {
            ctx.emit(PlayerEvent::Warning("Screenshot failed".into()));
            return;
        };
        for px in bgra.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match image::RgbaImage::from_raw(w, h, bgra).map(|img| img.save(&path)) {
            Some(Ok(())) => ctx.emit(PlayerEvent::ScreenshotSaved(path)),
            Some(Err(e)) => ctx.emit(PlayerEvent::Warning(format!("Screenshot failed: {e}"))),
            None => ctx.emit(PlayerEvent::Warning("Screenshot failed".into())),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_shrinks_but_only_enlarges_when_asked() {
        // 1080p into a 960x540 area: halved either way.
        assert_eq!(output_size(1920, 1080, 1.0, (960, 540), false), (960, 540));
        assert_eq!(output_size(1920, 1080, 1.0, (960, 540), true), (960, 540));
        // 320x240 into 1280x960: left to the GPU, or enlarged 4x here.
        assert_eq!(output_size(320, 240, 1.0, (1280, 960), false), (320, 240));
        assert_eq!(output_size(320, 240, 1.0, (1280, 960), true), (1280, 960));
        // Anamorphic 720x480 (32:27 SAR) keeps its shape when enlarged.
        assert_eq!(output_size(720, 480, 32.0 / 27.0, (1706, 960), true), (1706, 960));
        // Resized pictures get an even width; unresized ones keep theirs.
        assert_eq!(output_size(1920, 1080, 1.0, (1447, 900), false), (1446, 814));
        assert_eq!(output_size(1279, 720, 1.0, (1920, 1080), false), (1279, 720));
    }

    fn test_frame(fmt: ff::AVPixelFormat, w: i32, h: i32) -> Frame {
        let f = Frame::new();
        unsafe {
            (*f.0).width = w;
            (*f.0).height = h;
            (*f.0).format = fmt as c_int;
            assert!(ff::av_frame_get_buffer(f.0, 0) >= 0);
            // Pixel values don't matter for these tests; the luma plane gets
            // a mid grey so the conversion has something defined to read.
            std::ptr::write_bytes((*f.0).data[0], 120, (*f.0).linesize[0] as usize * h as usize);
        }
        f
    }

    #[test]
    fn conversion_output_is_opaque() {
        use ff::AVPixelFormat::*;
        for quality in [ScaleQuality::Nearest, ScaleQuality::Linear, ScaleQuality::Good, ScaleQuality::Best] {
            // YUV skips the alpha pass, packed RGB with padding doesn't.
            for fmt in [AV_PIX_FMT_YUV420P, AV_PIX_FMT_NV12, AV_PIX_FMT_YUV420P10LE, AV_PIX_FMT_GRAY8, AV_PIX_FMT_BGR0]
            {
                let f = test_frame(fmt, 64, 36);
                let mut conv = Converter::new();
                for (dw, dh) in [(64, 36), (32, 18), (128, 72)] {
                    let out = conv.convert(&f, dw, dh, 0, quality).unwrap_or_else(|| panic!("{fmt:?} {quality:?}"));
                    assert_eq!(out.len(), (dw * dh * 4) as usize);
                    assert!(out.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "{fmt:?} {quality:?} {dw}x{dh}");
                }
            }
        }
    }

    #[test]
    fn nv12_converts_like_planar_yuv() {
        use ff::AVPixelFormat::*;
        let (w, h) = (66, 38);
        let px = |x: usize, y: usize, c: usize| ((x * 37 + y * 11 + c * 101) % 256) as u8;
        let nv12 = test_frame(AV_PIX_FMT_NV12, w, h);
        let nv21 = test_frame(AV_PIX_FMT_NV21, w, h);
        let yuv = test_frame(AV_PIX_FMT_YUV420P, w, h);
        unsafe {
            for f in [&nv12, &nv21, &yuv] {
                let r = &*f.0;
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        *r.data[0].add(y * r.linesize[0] as usize + x) = px(x, y, 0);
                    }
                }
            }
            for y in 0..h as usize / 2 {
                for x in 0..w as usize / 2 {
                    let (u, v) = (px(x, y, 1), px(x, y, 2));
                    let (a, b, c) = (&*nv12.0, &*nv21.0, &*yuv.0);
                    *a.data[1].add(y * a.linesize[1] as usize + 2 * x) = u;
                    *a.data[1].add(y * a.linesize[1] as usize + 2 * x + 1) = v;
                    *b.data[1].add(y * b.linesize[1] as usize + 2 * x) = v;
                    *b.data[1].add(y * b.linesize[1] as usize + 2 * x + 1) = u;
                    *c.data[1].add(y * c.linesize[1] as usize + x) = u;
                    *c.data[2].add(y * c.linesize[2] as usize + x) = v;
                }
            }
        }
        let convert = |f: &Frame| Converter::new().convert(f, w as u32, h as u32, 1, ScaleQuality::Good).unwrap();
        let want = convert(&yuv);
        assert_eq!(convert(&nv12), want);
        assert_eq!(convert(&nv21), want);

        // Flipped pictures (the vflip filter) have negative strides.
        let flip = |f: &Frame| unsafe {
            let r = &mut *f.0;
            for (p, rows) in [(0, h), (1, h / 2), (2, h / 2)] {
                if !r.data[p].is_null() {
                    r.data[p] = r.data[p].offset((rows - 1) as isize * r.linesize[p] as isize);
                    r.linesize[p] = -r.linesize[p];
                }
            }
        };
        flip(&nv12);
        flip(&yuv);
        assert_eq!(convert(&nv12), convert(&yuv));
        flip(&nv12);
        flip(&yuv);
    }

    #[test]
    fn padded_conversion_repeats_the_edges() {
        let f = test_frame(ff::AVPixelFormat::AV_PIX_FMT_YUV420P, 64, 36);
        let mut conv = Converter::new();
        let (w, h, pad) = (48usize, 27usize, 2usize);
        let out = conv.convert(&f, w as u32, h as u32, pad as u32, ScaleQuality::Good).unwrap();
        let stride = w + 2 * pad;
        assert_eq!(out.len(), stride * (h + 2 * pad) * 4);
        let px = |x: usize, y: usize| &out[(y * stride + x) * 4..(y * stride + x) * 4 + 4];
        for y in 0..h + 2 * pad {
            for x in 0..stride {
                let (cx, cy) = (x.clamp(pad, pad + w - 1), y.clamp(pad, pad + h - 1));
                assert_eq!(px(x, y), px(cx, cy), "({x},{y})");
                assert_eq!(px(x, y)[3], 255);
            }
        }
    }

    #[test]
    fn extend_edges_copies_outermost_pixels() {
        // 2x2 picture with distinct pixels, 1-pixel border.
        let (w, h, pad) = (2, 2, 1);
        let stride = w + 2 * pad;
        let mut buf = vec![0u8; stride * (h + 2 * pad) * 4];
        let ids = [[1u8, 2], [3, 4]];
        for (y, row) in ids.iter().enumerate() {
            for (x, &v) in row.iter().enumerate() {
                buf[((y + pad) * stride + x + pad) * 4..][..4].copy_from_slice(&[v; 4]);
            }
        }
        extend_edges(&mut buf, w, h, pad);
        let got: Vec<u8> = buf.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(got, vec![1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
    }

    #[test]
    fn old_fast_scaling_setting_still_loads() {
        let q: ScaleQuality = serde_json::from_str("\"Fast\"").unwrap();
        assert_eq!(q, ScaleQuality::Linear);
        let q: ScaleQuality = serde_json::from_str("\"Nearest\"").unwrap();
        assert_eq!(q, ScaleQuality::Nearest);
    }
}
