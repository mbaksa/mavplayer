//! Independent audio analysis for local, seekable media. The envelope is
//! measured before playback effects, gain, speed changes and audio delay.
use super::av::{self, Frame, Packet, ff};
use super::filters::Graph;
use super::{EngineConfig, MediaSource};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::SystemTime;

pub const BINS: usize = 4096;
pub const MAGIC: &[u8; 8] = b"MAVWAV01";
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
pub type Peaks = Vec<[i16; 2]>;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Key {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    stream: usize,
}
impl Key {
    fn new(path: &Path, stream: usize) -> anyhow::Result<Self> {
        let path = path.canonicalize()?;
        let meta = path.metadata()?;
        anyhow::ensure!(meta.is_file(), "not a regular file");
        Ok(Self { path, size: meta.len(), modified: meta.modified()?, stream })
    }
    fn file(&self, directory: &Path) -> anyhow::Result<PathBuf> {
        let bytes = serde_json::to_vec(self)?;
        let hash = bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3));
        Ok(directory.join("waveform-v1").join(format!("{hash:016x}.bin")))
    }
}

fn cancelled(stop: &AtomicBool) -> anyhow::Result<()> {
    anyhow::ensure!(!stop.load(Ordering::Relaxed), "waveform generation cancelled");
    Ok(())
}

#[derive(Default)]
pub struct Progress {
    pub generating: AtomicBool,
    pub percent: AtomicU32,
}

pub fn load_or_generate(path: &Path, cfg: &EngineConfig, stream: usize, directory: &Path, force: bool, stop: &AtomicBool, progress: &Progress) -> anyhow::Result<Peaks> {
    cancelled(stop)?;
    let key = Key::new(path, stream)?;
    let file = key.file(directory)?;
    if !force && let Ok(peaks) = load(&file, &key) {
        cancelled(stop)?;
        anyhow::ensure!(Key::new(path, stream)? == key, "audio source changed");
        if let Ok(handle) = OpenOptions::new().write(true).open(&file) { let _ = handle.set_modified(SystemTime::now()); }
        return Ok(peaks);
    }
    progress.generating.store(true, Ordering::Relaxed);
    let peaks = generate(path, cfg, stream, stop, progress)?;
    cancelled(stop)?;
    anyhow::ensure!(Key::new(path, stream)? == key, "audio source changed during generation");
    if let Err(err) = save(&file, &key, &peaks, stop) { log::debug!("waveform cache: {err}"); }
    cancelled(stop)?;
    anyhow::ensure!(Key::new(path, stream)? == key, "audio source changed during generation");
    Ok(peaks)
}

/// Load a validated cached waveform without decoding the media.
pub fn load_cached(path: &Path, stream: usize, directory: &Path) -> anyhow::Result<Peaks> {
    let key = Key::new(path, stream)?;
    let peaks = load(&key.file(directory)?, &key)?;
    anyhow::ensure!(Key::new(path, stream)? == key, "source changed during waveform cache read");
    Ok(peaks)
}

fn load(file: &Path, key: &Key) -> anyhow::Result<Peaks> {
    let mut r = fs::File::open(file)?;
    let mut magic = [0; 8];
    r.read_exact(&mut magic)?;
    anyhow::ensure!(&magic == MAGIC, "invalid waveform format");
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    anyhow::ensure!(len <= 1024 * 1024 && r.metadata()?.len() == (12 + len + BINS * 4) as u64, "invalid waveform length");
    let mut meta = vec![0; len];
    r.read_exact(&mut meta)?;
    anyhow::ensure!(&serde_json::from_slice::<Key>(&meta)? == key, "stale waveform");
    let mut data = vec![0; BINS * 4];
    r.read_exact(&mut data)?;
    let peaks: Peaks = data.as_chunks::<4>().0.iter().map(|b| [i16::from_le_bytes([b[0], b[1]]), i16::from_le_bytes([b[2], b[3]])]).collect();
    anyhow::ensure!(peaks.iter().all(|p| p[0] <= p[1]), "invalid waveform peaks");
    Ok(peaks)
}

fn save(file: &Path, key: &Key, peaks: &Peaks, stop: &AtomicBool) -> anyhow::Result<()> {
    let _guard = super::thumbnails::cache::CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    cancelled(stop)?;
    let parent = file.parent().ok_or_else(|| anyhow::anyhow!("invalid waveform path"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".tmp-{}-{}", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
    let mut w = OpenOptions::new().write(true).create_new(true).open(&temp)?;
    let result = (|| -> anyhow::Result<()> {
        let meta = serde_json::to_vec(key)?;
        w.write_all(MAGIC)?;
        w.write_all(&u32::try_from(meta.len())?.to_le_bytes())?;
        w.write_all(&meta)?;
        for p in peaks { for v in p { w.write_all(&v.to_le_bytes())?; } }
        w.flush()?;
        drop(w);
        cancelled(stop)?;
        anyhow::ensure!(Key::new(&key.path, key.stream)? == *key, "audio source changed during generation");
        fs::rename(&temp, file)?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(temp); }
    result
}

struct Envelope {
    duration: f64,
    peaks: Peaks,
    filled: Vec<bool>,
}
impl Envelope {
    fn new(duration: f64) -> Self { Self { duration, peaks: vec![[0, 0]; BINS], filled: vec![false; BINS] } }
    fn add(&mut self, time: f64, rate: i32, channels: usize, data: &[f32]) {
        if !time.is_finite() || rate <= 0 || channels == 0 { return; }
        let frames = data.len() / channels;
        let scale = BINS as f64 / self.duration;
        let mut i = 0;
        while i < frames {
            let t = time + i as f64 / f64::from(rate);
            if t >= self.duration { break; }
            if t < 0.0 { i += 1; continue; }
            let bin = ((t * scale) as usize).min(BINS - 1);
            let next = (((bin + 1) as f64 / scale - time) * f64::from(rate)).ceil() as usize;
            let end = next.max(i + 1).min(frames);
            let mut lo = i16::MAX;
            let mut hi = i16::MIN;
            for &sample in &data[i * channels..end * channels] {
                let v = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
                lo = lo.min(v); hi = hi.max(v);
            }
            if self.filled[bin] {
                self.peaks[bin][0] = self.peaks[bin][0].min(lo);
                self.peaks[bin][1] = self.peaks[bin][1].max(hi);
            } else {
                self.peaks[bin] = [lo, hi];
                self.filled[bin] = true;
            }
            i = end;
        }
    }
}

type AudioFormat = (i32, i32, String);

struct AudioAnalysis {
    envelope: Envelope,
    graph: Option<(AudioFormat, Graph)>,
    filtered: Frame,
    next_pts: f64,
    start: f64,
}
impl AudioAnalysis {
    fn drain(&mut self) {
        let Some((_, graph)) = &mut self.graph else { return; };
        let tb = graph.out_time_base();
        while graph.pull(&mut self.filtered) {
            let raw = self.filtered.raw();
            let channels = raw.ch_layout.nb_channels;
            if channels > 0 && channels <= 256 && raw.nb_samples > 0 && raw.sample_rate > 0
                && raw.format == ff::AVSampleFormat::AV_SAMPLE_FMT_FLT as i32 && !raw.data[0].is_null()
            {
                let n = raw.nb_samples as usize * channels as usize;
                if n <= raw.linesize[0].max(0) as usize / std::mem::size_of::<f32>() {
                    let data = unsafe { std::slice::from_raw_parts(raw.data[0].cast::<f32>(), n) };
                    let time = av::ts_to_secs(raw.pts, tb) - self.start;
                    self.envelope.add(time, raw.sample_rate, channels as usize, data);
                }
            }
            self.filtered.unref();
        }
    }
    fn add(&mut self, frame: &mut Frame, tb: ff::AVRational, progress: &Progress) -> anyhow::Result<()> {
        let rate = frame.raw().sample_rate;
        anyhow::ensure!(rate > 0, "invalid audio sample rate");
        let layout = super::audio::layout_string(&frame.raw().ch_layout);
        let key = (rate, frame.format(), layout.clone());
        let mut pts = av::ts_to_secs(frame.raw().best_effort_timestamp, tb);
        if !pts.is_finite() { pts = av::ts_to_secs(frame.raw().pts, tb); }
        if !pts.is_finite() { pts = self.next_pts; }
        self.next_pts = pts + f64::from(frame.nb_samples()) / f64::from(rate);
        if self.graph.as_ref().is_none_or(|(old, _)| old != &key) {
            if let Some((_, graph)) = &mut self.graph { graph.push_eof(); }
            self.drain();
            let args = format!("time_base=1/{rate}:sample_rate={rate}:sample_fmt={}:channel_layout={layout}", av::sample_fmt_name(frame.format()));
            let chain = format!("aformat=sample_fmts=flt:sample_rates={rate}:channel_layouts={layout}");
            self.graph = Some((key, Graph::new("abuffer", &args, "abuffersink", &chain, 1)?));
        }
        unsafe { (*frame.0).pts = (pts * f64::from(rate)).round() as i64; }
        if let Some((_, graph)) = &mut self.graph { graph.push(frame)?; }
        self.drain();
        let percent = ((self.next_pts - self.start) * 100.0 / self.envelope.duration).clamp(0.0, 99.0) as u32;
        progress.percent.fetch_max(percent, Ordering::Relaxed);
        Ok(())
    }
}

fn generate(path: &Path, cfg: &EngineConfig, stream_index: usize, stop: &AtomicBool, progress: &Progress) -> anyhow::Result<Peaks> {
    let source = MediaSource::File(path.to_owned());
    let ic = super::demux::open_input(&source, cfg, None)?;
    let info = super::demux::build_media_info(&ic, &source);
    anyhow::ensure!(info.seekable && info.duration.is_finite() && info.duration > 0.0, "media not seekable");
    anyhow::ensure!(stream_index < ic.nb_streams(), "invalid audio stream");
    let stream = ic.stream(stream_index);
    unsafe {
        anyhow::ensure!((*(*stream).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_AUDIO, "not an audio stream");
        for i in 0..ic.nb_streams() { if i != stream_index { (*ic.stream(i)).discard = ff::AVDiscard::AVDISCARD_ALL; } }
    }
    let codec = super::decoder::open_codec(stream, 1, None, None)?;
    let tb = unsafe { (*stream).time_base };
    let mut analysis = AudioAnalysis { envelope: Envelope::new(info.duration), graph: None, filtered: Frame::new(), next_pts: info.start_time, start: info.start_time };
    let mut frame = Frame::new();
    let mut packet = Packet::new();
    let mut draining = false;
    loop {
        cancelled(stop)?;
        let r = unsafe { ff::avcodec_receive_frame(codec.ctx.0, frame.0) };
        if r >= 0 {
            analysis.add(&mut frame, tb, progress)?;
            frame.unref();
            continue;
        }
        if r == ff::AVERROR_EOF { break; }
        anyhow::ensure!(r == av::EAGAIN && !draining, "waveform decode: {}", av::err_str(r));
        loop {
            cancelled(stop)?;
            packet.unref();
            let r = unsafe { ff::av_read_frame(ic.0, packet.0) };
            if r == ff::AVERROR_EOF {
                av::check(unsafe { ff::avcodec_send_packet(codec.ctx.0, std::ptr::null()) }, "waveform drain")?;
                draining = true;
                break;
            }
            av::check(r, "waveform read")?;
            if packet.stream_index() == stream_index as i32 {
                av::check(unsafe { ff::avcodec_send_packet(codec.ctx.0, packet.0) }, "waveform packet")?;
                break;
            }
        }
    }
    if let Some((_, graph)) = &mut analysis.graph { graph.push_eof(); }
    analysis.drain();
    cancelled(stop)?;
    anyhow::ensure!(analysis.envelope.filled.iter().any(|v| *v), "no audio samples decoded");
    Ok(analysis.envelope.peaks)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn waveform_preserves_channel_peaks_and_media_time() {
        let mut e = Envelope::new(4.0);
        e.add(1.0, 48000, 2, &[-0.8, 0.7, -0.1, 0.1]);
        assert_eq!(e.peaks[0], [0, 0]);
        assert_eq!(e.peaks[BINS / 4], [(-0.8 * 32767.0) as i16, (0.7 * 32767.0) as i16]);
    }
    #[test]
    fn waveform_cache_rejects_changed_media() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("mav-wave-test-{}-{}", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&root)?;
        let result = (|| -> anyhow::Result<()> {
            let path = root.join("audio.wav");
            fs::write(&path, b"audio")?;
            let key = Key::new(&path, 0)?;
            let file = key.file(&root)?;
            let peaks = vec![[-1234, 2345]; BINS];
            save(&file, &key, &peaks, &AtomicBool::new(false))?;
            assert_eq!(load(&file, &key)?, peaks);
            assert!(cached(&path, 0, &root));
            fs::write(&path, b"changed audio")?;
            assert!(load(&file, &Key::new(&path, 0)?).is_err());
            assert!(!cached(&path, 0, &root));
            Ok(())
        })();
        let _ = fs::remove_dir_all(root);
        result
    }
}
