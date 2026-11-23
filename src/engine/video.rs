//! Video decode thread: decode → (optional) libavfilter chain → frame queue.

use super::SessionCtx;
use super::av::{self, FormatCtx, Frame, ff};
use super::decoder::{DecodeResult, Decoder, GpuResize, GpuUpload};
use super::filters::Graph;
use super::sync::VideoFrame;
use super::types::*;
use super::upscale::{self, Plan};
use std::os::raw::c_int;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the window size must stay put before a GPU upscaling graph is
/// rebuilt for it (rebuilding takes a moment; during a drag it would stall).
const RESIZE_SETTLE: Duration = Duration::from_millis(300);

#[derive(Clone)]
pub struct StreamInfo {
    pub width: i32,
    pub height: i32,
    pub sar: f64,
    pub fps: f64,
    pub time_base: ff::AVRational,
    pub frame_rate: ff::AVRational,
    pub attached_pic: bool,
    /// Clockwise rotation from the container's display matrix, in degrees.
    pub rotation: i32,
}

impl StreamInfo {
    pub fn from_stream(ic: &FormatCtx, idx: usize) -> Self {
        unsafe {
            let st = ic.stream(idx);
            let par = &*(*st).codecpar;
            let sar = ff::av_guess_sample_aspect_ratio(ic.0, st, ptr::null_mut());
            let fr = ff::av_guess_frame_rate(ic.0, st, ptr::null_mut());
            let mut rotation = 0;
            let sd = ff::av_packet_side_data_get(
                par.coded_side_data,
                par.nb_coded_side_data,
                ff::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
            );
            if !sd.is_null() && (*sd).size >= 36 {
                let theta = -ff::av_display_rotation_get((*sd).data as *const i32);
                if theta.is_finite() {
                    rotation = ((theta.round() as i32 % 360) + 360) % 360;
                    rotation = ((rotation + 45) / 90 * 90) % 360;
                }
            }
            StreamInfo {
                width: par.width,
                height: par.height,
                sar: if sar.num > 0 && sar.den > 0 { av::q2d(sar) } else { 1.0 },
                fps: av::q2d(fr),
                time_base: (*st).time_base,
                frame_rate: fr,
                attached_pic: (*st).disposition & ff::AV_DISPOSITION_ATTACHED_PIC as c_int != 0,
                rotation,
            }
        }
    }

    pub fn display_aspect(&self) -> f64 {
        if self.height <= 0 {
            return 16.0 / 9.0;
        }
        let a = self.width as f64 * self.sar / self.height as f64;
        if self.rotation == 90 || self.rotation == 270 { 1.0 / a } else { a }
    }
}

fn is_hdr(f: &Frame) -> bool {
    let trc = f.raw().color_trc as c_int;
    trc == ff::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084 as c_int
        || trc == ff::AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67 as c_int
}

/// A video filter chain: the libavfilter description ("" = none needed) and
/// the GPU upscaling at its end, if any.
#[derive(PartialEq, Clone)]
struct Chain {
    desc: String,
    upscale: Option<Plan>,
}

/// The libavfilter chain for the current settings. `upscale` is false when
/// GPU upscaling failed for these settings.
fn build_chain(
    adj: &VideoAdjust,
    cfg: &EngineConfig,
    info: &StreamInfo,
    frame: &Frame,
    interlaced: bool,
    target: (u32, u32),
    upscale: bool,
) -> Chain {
    let mut f: Vec<String> = Vec::new();
    // The picture's size and pixel aspect as it leaves each filter, for
    // fitting the upscaled picture to the window.
    let (mut w, mut h) = (frame.width(), frame.height());
    let mut sar = {
        let s = frame.raw().sample_aspect_ratio;
        if s.num > 0 && s.den > 0 { av::q2d(s) } else { info.sar }
    };
    match cfg.deinterlace {
        Deinterlace::Off => {}
        Deinterlace::Auto if !interlaced => {}
        mode => {
            let deint = if mode == Deinterlace::On { "all" } else { "interlaced" };
            if av::filter_available("bwdif") {
                f.push(format!("bwdif=mode=send_frame:parity=auto:deint={deint}"));
            } else if av::filter_available("yadif") {
                f.push(format!("yadif=mode=send_frame:parity=auto:deint={deint}"));
            }
        }
    }
    if cfg.tone_map_hdr && is_hdr(frame) && av::filter_available("zscale") && av::filter_available("tonemap") {
        // Tone mapping in linear float is expensive: do it at display size.
        let (dw, dh) = super::present::output_size(frame.width(), frame.height(), sar, target, false);
        let (dw, dh) = (dw & !1, dh & !1);
        if dw < frame.width() as u32 || dh < frame.height() as u32 {
            let filter = match cfg.scale_quality {
                ScaleQuality::Nearest => "point",
                ScaleQuality::Linear => "bilinear",
                ScaleQuality::Good => "bicubic",
                ScaleQuality::Best => "lanczos",
            };
            f.push(format!("zscale=w={dw}:h={dh}:f={filter},setsar=1"));
            (w, h, sar) = (dw as i32, dh as i32, 1.0);
        }
        f.push(
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p"
                .into(),
        );
    }
    if let Some([l, t, r, b]) = adj.crop {
        let (l, t, r, b) = (l.clamp(0.0, 0.45), t.clamp(0.0, 0.45), r.clamp(0.0, 0.45), b.clamp(0.0, 0.45));
        let (fw, fh) = (1.0 - l - r, 1.0 - t - b);
        f.push(format!("crop=w=trunc(iw*{fw}/2)*2:h=trunc(ih*{fh}/2)*2:x=iw*{l}:y=ih*{t}"));
        w = (w as f32 * fw / 2.0).trunc() as i32 * 2;
        h = (h as f32 * fh / 2.0).trunc() as i32 * 2;
    }
    let rotation = rotation(adj, info);
    match rotation {
        90 => f.push("transpose=clock".into()),
        180 => f.push("hflip,vflip".into()),
        270 => f.push("transpose=cclock".into()),
        _ => {}
    }
    if rotation % 180 == 90 {
        (w, h, sar) = (h, w, 1.0 / sar);
    }
    if adj.hflip {
        f.push("hflip".into());
    }
    if adj.vflip {
        f.push("vflip".into());
    }
    if adj.denoise {
        f.push("hqdn3d".into());
    }
    let eq_needed = adj.brightness.abs() > 0.001
        || (adj.contrast - 1.0).abs() > 0.001
        || (adj.saturation - 1.0).abs() > 0.001
        || (adj.gamma - 1.0).abs() > 0.001;
    if eq_needed {
        f.push(format!(
            "eq=brightness={:.3}:contrast={:.3}:saturation={:.3}:gamma={:.3}",
            adj.brightness, adj.contrast, adj.saturation, adj.gamma
        ));
    }
    if adj.hue.abs() > 0.5 || adj.grayscale {
        let s = if adj.grayscale { 0.0 } else { 1.0 };
        f.push(format!("hue=h={:.1}:s={s}", adj.hue));
    }
    if adj.sharpen > 0.01 {
        f.push(format!("unsharp=5:5:{:.2}:5:5:0", adj.sharpen.min(3.0)));
    }
    if adj.negate {
        f.push("negate".into());
    }
    // Last, so the shaders see the finished picture and nothing after them
    // works at display size.
    let upscale = if upscale { upscale::plan(cfg, w, h, sar, target) } else { None };
    if let Some(p) = &upscale {
        f.push(p.desc.clone());
    }
    Chain { desc: f.join(","), upscale }
}

/// Clockwise rotation of the picture, in degrees: the container's and the
/// user's.
fn rotation(adj: &VideoAdjust, info: &StreamInfo) -> i32 {
    let user = match adj.rotation {
        Rotation::None => 0,
        Rotation::R90 => 90,
        Rotation::R180 => 180,
        Rotation::R270 => 270,
    };
    (info.rotation + user) % 360
}

/// Whether the chain deinterlaces (see `build_chain`).
fn deinterlacing(cfg: &EngineConfig, interlaced: bool) -> bool {
    match cfg.deinterlace {
        Deinterlace::Off => false,
        Deinterlace::Auto => interlaced,
        _ => true,
    }
}

/// The size to resize a hardware-decoded picture to on the GPU (see
/// `decoder::GpuScaler`), or `None` to download it whole.
///
/// Only when nothing in the chain needs the full picture: deinterlacing
/// needs whole fields, tone mapping its own scaling, crop and rotation
/// change what "display size" means, and GPU upscaling enlarges the
/// original. The rest of the chain (flips, colour adjustments, denoise,
/// sharpen) then works on the smaller picture. Never enlarges.
fn gpu_resize(
    adj: &VideoAdjust,
    cfg: &EngineConfig,
    info: &StreamInfo,
    frame: &Frame,
    interlaced: bool,
    target: (u32, u32),
    upscale: bool,
) -> Option<GpuResize> {
    if deinterlacing(cfg, interlaced)
        || (cfg.tone_map_hdr && is_hdr(frame))
        || adj.crop.is_some()
        || rotation(adj, info) != 0
    {
        return None;
    }
    let (w, h, sar) = (frame.width(), frame.height(), frame_sar(frame, info));
    if upscale && upscale::plan(cfg, w, h, sar, target).is_some() {
        return None;
    }
    let (dw, dh) = super::present::gpu_size(w, h, sar, target);
    // As decoded, in a format swscale converts fast, for a presenter that
    // can't convert on the GPU: just download it.
    if (dw, dh) == (w, h)
        && frame.sw_format() == ff::AVPixelFormat::AV_PIX_FMT_NV12 as c_int
        && !super::present::gpu_convert_ok()
    {
        return None;
    }
    Some(GpuResize { w: dw, h: dh, quality: cfg.scale_quality })
}

#[derive(PartialEq, Clone)]
struct GraphKey {
    w: i32,
    h: i32,
    fmt: c_int,
    chain: Chain,
    sar: (i32, i32),
    /// A seek starts a new graph: deinterlacers and denoisers hold frames
    /// from before it, which would be shown under the new position.
    serial: i32,
}

pub fn run_decoder(ctx: Arc<SessionCtx>, mut dec: Decoder, info: StreamInfo, stop: Arc<AtomicBool>) {
    let mut frame = Frame::new();
    let mut filtered = Frame::new();
    let mut graph: Option<(GraphKey, Graph)> = None;
    let mut adj_gen = u64::MAX;
    let mut adj = VideoAdjust::default();
    let frame_dur =
        if info.frame_rate.num > 0 && info.frame_rate.den > 0 { 1.0 / av::q2d(info.frame_rate) } else { 0.0 };
    let mut reported_fmt = false;
    let mut saw_interlaced = false;
    let mut cfg = ctx.controls.config.lock().clone();
    let mut cfg_check = 0u32;
    // The window size the chain is built for, and a newer one waiting to
    // settle (see RESIZE_SETTLE).
    let mut target = ctx.controls.target_size();
    let mut pending_target = (target, Instant::now());
    // GPU upscaling that failed to start; not retried until settings change.
    let mut broken_upscale: Option<upscale::ShaderSetKey> = None;
    // Hardware frames come out of the decoder still on the GPU, to be
    // resized there (`gpu_resize`) or downloaded whole.
    dec.keep_hw = true;
    // The decoded size and pixel aspect of pictures resized on the GPU.
    let mut source: Option<(i32, i32, f64)> = None;
    // The serial `target` was last chosen for: after a seek, the size the
    // window has now is taken at once (a paused picture is redone that way).
    let mut target_serial = -1;
    let mut to_gpu = ToGpu { up: GpuUpload::new(), allowed: false, target, quality: cfg.scale_quality, active: false };

    while ctx.alive() && !stop.load(Ordering::Acquire) {
        match dec.decode(&mut frame) {
            DecodeResult::Aborted => break,
            DecodeResult::Eof => {
                // The last frames a deinterlacer still holds.
                if let Some((k, g)) = graph.as_mut()
                    && k.serial == dec.pkt_serial
                {
                    g.push_eof();
                    let otb = g.out_time_base();
                    while g.pull(&mut filtered) {
                        let p = av::ts_to_secs(filtered.raw().pts, otb);
                        if !p.is_finite() {
                            filtered.unref();
                            continue;
                        }
                        let sar = frame_sar(&filtered, &info);
                        let dur = frame_duration(&filtered, otb, frame_dur);
                        let vf =
                            VideoFrame { frame: filtered.take(), pts: p, duration: dur, serial: k.serial, sar, source };
                        if !push(&ctx, &dec, &mut to_gpu, vf) {
                            break;
                        }
                    }
                }
                graph = None;
                // Its graph holds the CUDA device, which the decoder lets go
                // of a while after the end (`Decoder::park`).
                to_gpu.up = GpuUpload::new();
                ctx.video_finished.store(dec.finished, Ordering::Release);
                ctx.wake_demuxer();
                continue;
            }
            DecodeResult::Frame => {}
            DecodeResult::Restart => {
                // Decoding restarts at a keyframe: from where playback is.
                // (A DVD's navigator decides where reading goes; its short
                // GOPs bring the next keyframe soon enough.)
                #[cfg(has_dvdnav)]
                let nav = ctx.nav.lock().is_some();
                #[cfg(not(has_dvdnav))]
                let nav = false;
                if !nav {
                    ctx.request_seek(super::SeekRequest {
                        target: ctx.position(),
                        relative: false,
                        precise: true,
                        direction: 0,
                        origin: 0.0,
                    });
                }
                graph = None;
                continue;
            }
        }
        let serial = dec.pkt_serial;
        let tb = info.time_base;
        let ts = frame.raw().best_effort_timestamp;
        let pts = av::ts_to_secs(ts, tb);
        unsafe { (*frame.0).pts = ts };

        // Exact seek: skip frames before the target (not cover art, which is
        // the picture for the whole file).
        if !info.attached_pic
            && let Some((target, s)) = *ctx.seek_target.lock()
            && s == serial
            && pts.is_finite()
            && pts + frame_dur.max(0.001) * 0.5 < target
        {
            frame.unref();
            continue;
        }

        if !reported_fmt {
            reported_fmt = true;
            log::debug!("startup: first video frame decoded at {:.0} ms", crate::ms_since_start());
            let (pf, cs, hdr) = (av::pix_fmt_name(frame.sw_format()), color_desc(&frame), is_hdr(&frame));
            ctx.update_state(|st| {
                if let Some(v) = st.video.as_mut() {
                    v.pix_fmt = pf;
                    v.color = cs;
                    v.hdr = hdr;
                }
            });
        }

        {
            let g = ctx.controls.video_adjust.lock();
            if g.0 != adj_gen {
                adj_gen = g.0;
                adj = g.1.clone();
            }
        }
        cfg_check = cfg_check.wrapping_add(1);
        if cfg_check.is_multiple_of(30) || info.attached_pic {
            cfg = ctx.controls.config.lock().clone();
        }
        saw_interlaced |= frame.raw().flags & ff::AV_FRAME_FLAG_INTERLACED as c_int != 0;
        let t = ctx.controls.target_size();
        if t != pending_target.0 {
            pending_target = (t, Instant::now());
        }
        // A graph (or the GPU resize) is rebuilt for a new size only once the
        // window has stopped changing size; meanwhile the presenter scales.
        // (Cover art is decoded again only once it has: see `redo_cover`.)
        let rebuilds = (graph.is_some() || source.is_some() || to_gpu.active) && !info.attached_pic;
        if t != target && (!rebuilds || pending_target.1.elapsed() >= RESIZE_SETTLE || serial != target_serial) {
            target = t;
        }
        target_serial = serial;
        let try_upscale = ctx.controls.target_known.load(Ordering::Relaxed)
            && broken_upscale.as_ref().is_none_or(|k| *k != upscale::ShaderSetKey::of(&cfg));

        // A screenshot of a picture that was resized on the GPU: save this
        // one, as decoded (downloaded whole), instead.
        let shot = ctx.screenshot_req.lock().take();
        if let Some(path) = shot {
            let whole = frame.clone_ref().map(|mut f| (!dec.is_hw(&f) || dec.download(&mut f), f));
            match whole {
                Some((true, whole)) => {
                    super::present::save_screenshot(ctx.clone(), whole, frame_sar(&frame, &info), path);
                }
                _ => ctx.emit(PlayerEvent::Warning("Screenshot failed".into())),
            }
        }
        let mut on_gpu = false;
        if dec.is_hw(&frame) {
            let decoded = (frame.width(), frame.height(), frame_sar(&frame, &info));
            let resize = if ctx.controls.target_known.load(Ordering::Relaxed) && dec.can_resize(&frame) {
                gpu_resize(&adj, &cfg, &info, &frame, saw_interlaced, target, try_upscale)
            } else {
                None
            };
            let resized = resize.is_some_and(|rs| dec.resize_on_gpu(&mut frame, rs));
            source = resized.then_some(decoded);
            on_gpu = true;
        } else {
            source = None;
        }
        let chain = build_chain(&adj, &cfg, &info, &frame, saw_interlaced, target, try_upscale);
        to_gpu.allowed = cfg.hwdec != HwDecode::Off
            && chain.upscale.is_none()
            && ctx.controls.target_known.load(Ordering::Relaxed)
            && super::present::gpu_convert_ok();
        to_gpu.target = target;
        to_gpu.quality = cfg.scale_quality;
        // A window-sized picture with no filters left to run waits in the
        // queue on the GPU, and the presenter converts it there as it
        // downloads it. Anything else is downloaded now.
        if on_gpu {
            if source.is_some() && chain.desc.is_empty() && super::present::gpu_convert_ok() {
                dec.release_downloads();
            } else if !dec.download(&mut frame) {
                frame.unref();
                continue;
            }
        }

        if chain.desc.is_empty() {
            graph = None;
            let sar = frame_sar(&frame, &info);
            let dur = frame_duration(&frame, tb, frame_dur);
            let vf = VideoFrame { frame: frame.take(), pts, duration: dur, serial, sar, source };
            if !push(&ctx, &dec, &mut to_gpu, vf) {
                continue;
            }
            continue;
        }

        let key = GraphKey {
            w: frame.width(),
            h: frame.height(),
            fmt: frame.format(),
            chain,
            sar: unsafe { ((*frame.0).sample_aspect_ratio.num, (*frame.0).sample_aspect_ratio.den) },
            serial,
        };
        if graph.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
            let t0 = Instant::now();
            graph = match make_graph(&frame, &info, &key.chain) {
                Ok(g) => {
                    if key.chain.upscale.is_some() {
                        log::debug!("upscale: graph built in {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
                    }
                    Some((key, g))
                }
                Err(e) if key.chain.upscale.is_some() => {
                    // Play on without it; the next frame builds the rest.
                    log::warn!("GPU upscaling disabled: {e}");
                    ctx.emit(PlayerEvent::Warning(format!("GPU upscaling failed: {e}")));
                    broken_upscale = Some(upscale::ShaderSetKey::of(&cfg));
                    frame.unref();
                    continue;
                }
                Err(e) => {
                    log::warn!("video filters disabled: {e}");
                    ctx.emit(PlayerEvent::Warning(format!("Video filter error: {e}")));
                    None
                }
            };
        }
        let Some((k, g)) = graph.as_mut() else {
            let sar = frame_sar(&frame, &info);
            let dur = frame_duration(&frame, tb, frame_dur);
            let vf = VideoFrame { frame: frame.take(), pts, duration: dur, serial, sar, source };
            push(&ctx, &dec, &mut to_gpu, vf);
            continue;
        };
        // libplacebo times its output by frame durations: give untimed
        // frames the stream's frame rate (or a tick).
        if k.chain.upscale.is_some() && frame.raw().duration <= 0 {
            let d = if frame_dur > 0.0 { (frame_dur / av::q2d(tb)).round() as i64 } else { 1 };
            unsafe { (*frame.0).duration = d.max(1) };
        }
        if let Err(e) = g.push(&mut frame) {
            log::debug!("{e}");
            graph = None;
            frame.unref();
            continue;
        }
        frame.unref();
        let otb = g.out_time_base();
        while g.pull(&mut filtered) {
            let p = av::ts_to_secs(filtered.raw().pts, otb);
            let sar = frame_sar(&filtered, &info);
            let dur = frame_duration(&filtered, otb, frame_dur);
            let vf = VideoFrame {
                frame: filtered.take(),
                pts: if p.is_finite() { p } else { pts },
                duration: dur,
                serial,
                sar,
                source,
            };
            if !push(&ctx, &dec, &mut to_gpu, vf) {
                break;
            }
        }
    }
}

/// Pictures in system memory sent to the GPU to be resized there
/// ([`GpuUpload`]), as hardware-decoded ones are, rather than by the
/// presenter's swscale.
struct ToGpu {
    up: GpuUpload,
    /// Set for each decoded picture: whether this may be done (no shader
    /// upscaling, the window size known), the size and the scaling filter.
    allowed: bool,
    target: (u32, u32),
    quality: ScaleQuality,
    /// The last picture went to the GPU.
    active: bool,
}

impl ToGpu {
    fn apply(&mut self, vf: &mut VideoFrame) {
        self.active = false;
        if !self.allowed || vf.source.is_some() {
            return;
        }
        let (w, h, sar) = (vf.frame.width(), vf.frame.height(), vf.sar);
        let (dw, dh) = super::present::gpu_size(w, h, sar, self.target);
        let resize = GpuResize { w: dw, h: dh, quality: self.quality };
        if GpuUpload::worth_it(&vf.frame, resize) && self.up.run(&mut vf.frame, resize) {
            vf.source = Some((w, h, sar));
            vf.sar = 1.0;
            self.active = true;
        }
    }
}

fn push(ctx: &SessionCtx, dec: &Decoder, to_gpu: &mut ToGpu, mut vf: VideoFrame) -> bool {
    to_gpu.apply(&mut vf);
    let q = dec.queue.clone();
    let ok = ctx.frames.push(vf, |s| q.serial() != s);
    if let Some(mut st) = ctx.shared.state.try_lock()
        && ctx.is_current()
    {
        st.decoded_frames += 1;
    }
    ok
}

fn frame_sar(f: &Frame, info: &StreamInfo) -> f64 {
    let s = f.raw().sample_aspect_ratio;
    if s.num > 0 && s.den > 0 { av::q2d(s) } else { info.sar }
}

fn frame_duration(f: &Frame, tb: ff::AVRational, fallback: f64) -> f64 {
    let d = f.raw().duration;
    if d > 0 { d as f64 * av::q2d(tb) } else { fallback }
}

fn make_graph(frame: &Frame, info: &StreamInfo, chain: &Chain) -> anyhow::Result<Graph> {
    let f = frame.raw();
    let sar = if f.sample_aspect_ratio.num > 0 { f.sample_aspect_ratio } else { ff::AVRational { num: 1, den: 1 } };
    let tb = info.time_base;
    let fr = if info.frame_rate.num > 0 { info.frame_rate } else { ff::AVRational { num: 25, den: 1 } };
    let args = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}:frame_rate={}/{}:colorspace={}:range={}",
        f.width,
        f.height,
        f.format,
        tb.num,
        tb.den,
        sar.num,
        sar.den,
        fr.num,
        fr.den,
        f.colorspace as c_int,
        f.color_range as c_int
    );
    let threads = std::thread::available_parallelism().map(|n| n.get().min(8) as i32).unwrap_or(4);
    match &chain.upscale {
        Some(plan) => Graph::with_setup("buffer", &args, "buffersink", &chain.desc, threads, &mut |f| {
            upscale::setup_filter(f, plan)
        }),
        None => Graph::new("buffer", &args, "buffersink", &chain.desc, threads),
    }
}

fn color_desc(f: &Frame) -> String {
    unsafe {
        let r = f.raw();
        let cs = av::from_cstr(ff::av_color_space_name(r.colorspace)).unwrap_or_default();
        let pr = av::from_cstr(ff::av_color_primaries_name(r.color_primaries)).unwrap_or_default();
        let trc = av::from_cstr(ff::av_color_transfer_name(r.color_trc)).unwrap_or_default();
        let range = av::from_cstr(ff::av_color_range_name(r.color_range)).unwrap_or_default();
        let known = |v: &String| !v.is_empty() && v != "unknown" && v != "reserved" && v != "unspecified";
        let mut parts: Vec<String> = [cs, pr, trc].into_iter().filter(known).collect();
        parts.dedup();
        let range = match range.as_str() {
            "tv" => "limited range",
            "pc" => "full range",
            _ => "",
        };
        match (parts.is_empty(), range.is_empty()) {
            (true, true) => String::new(),
            (true, false) => range.to_string(),
            (false, true) => parts.join("/"),
            (false, false) => format!("{} ({range})", parts.join("/")),
        }
    }
}
