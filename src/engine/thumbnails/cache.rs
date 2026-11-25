use super::{GenerationProgress, Thumbnail, ThumbnailSpec, generate};
use crate::engine::EngineConfig;
use anyhow::Context;
use image::ImageFormat;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufReader, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const MAGIC: &[u8; 8] = b"MAVTHM02";
const MAX_BYTES: u64 = 80 * 1024 * 1024;
static CACHE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Key {
    revision: u32,
    path: PathBuf,
    size: u64,
    modified: std::time::SystemTime,
    stream: usize,
    count: u32,
    width: u32,
    height: u32,
}

impl Key {
    fn new(path: &Path, spec: ThumbnailSpec) -> anyhow::Result<Self> {
        let path = path.canonicalize()?;
        let meta = path.metadata()?;
        anyhow::ensure!(meta.is_file(), "not a regular file");
        Ok(Self { revision: 3, path, size: meta.len(), modified: meta.modified()?, stream: spec.stream, count: spec.count, width: spec.width, height: spec.height })
    }

    fn file(&self, directory: &Path) -> anyhow::Result<PathBuf> {
        // Stable FNV-1a filename; the complete key is checked on every read.
        let bytes = serde_json::to_vec(self)?;
        let hash = bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3));
        Ok(directory.join("v2").join(format!("{hash:016x}.bin")))
    }
}

/// Blocking operation: call on a dedicated worker. Cache errors do not prevent
/// playback or extraction. A changed source is never published to the cache.
pub fn load_or_generate(path: &Path, cfg: &EngineConfig, spec: ThumbnailSpec, directory: &Path, stop: &AtomicBool, force: bool, generating: &AtomicBool, progress: &GenerationProgress) -> anyhow::Result<Vec<Thumbnail>> {
    spec.validate()?;
    super::cancelled(stop)?;
    let key = Key::new(path, spec).ok();
    {
        let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if !force
            && let Some(key) = &key
            && let Ok(file) = key.file(directory)
            && let Ok(thumbnails) = load(&file, key)
        {
            super::cancelled(stop)?;
            if Key::new(path, spec).ok().as_ref() == Some(key) {
                // Explicitly update last use; filesystem atime may be disabled.
                if let Ok(handle) = OpenOptions::new().write(true).open(&file) {
                    let _ = handle.set_modified(SystemTime::now());
                }
                return Ok(thumbnails);
            }
        }
    }
    generating.store(true, Ordering::Relaxed);
    let thumbnails = generate(path, cfg, spec, stop, progress)?;
    super::cancelled(stop)?;
    if let Some(key) = &key {
        anyhow::ensure!(Key::new(path, spec).ok().as_ref() == Some(key), "source changed during thumbnail generation");
        let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        super::cancelled(stop)?;
        if let Err(err) = key.file(directory).and_then(|file| save(&file, key, &thumbnails, stop)) {
            log::debug!("thumbnail cache: {err}");
        }
    }
    super::cancelled(stop)?;
    Ok(thumbnails)
}

fn read_u32(r: &mut impl Read) -> anyhow::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_blob(r: &mut impl Read, max: u32) -> anyhow::Result<Vec<u8>> {
    let len = read_u32(r)?;
    anyhow::ensure!(len <= max, "invalid cache record size");
    let mut bytes = vec![0; len as usize];
    r.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// A cache-only lookup: never opens the media with FFmpeg. A representative
/// lookup decodes only the nearest JPEG, not the entire set.
pub fn load_cached(path: &Path, spec: ThumbnailSpec, directory: &Path, target: Option<f64>) -> anyhow::Result<Vec<Thumbnail>> {
    spec.validate()?;
    let key = Key::new(path, spec)?;
    let file = key.file(directory)?;
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let result = load_selected(&file, &key, target)?;
    anyhow::ensure!(Key::new(path, spec)? == key, "source changed during cache read");
    if let Ok(handle) = OpenOptions::new().write(true).open(&file) {
        let _ = handle.set_modified(SystemTime::now());
    }
    Ok(result)
}

fn decode_jpeg(jpeg: Vec<u8>, key: &Key, time: f64) -> anyhow::Result<Thumbnail> {
    let mut reader = image::ImageReader::with_format(Cursor::new(jpeg), ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(key.width);
    limits.max_image_height = Some(key.height);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?.into_rgba8();
    anyhow::ensure!(image.width() <= key.width && image.height() <= key.height, "invalid cached dimensions");
    Ok(Thumbnail { time, image })
}

fn load(file: &Path, key: &Key) -> anyhow::Result<Vec<Thumbnail>> {
    load_selected(file, key, None)
}

fn load_selected(file: &Path, key: &Key, target: Option<f64>) -> anyhow::Result<Vec<Thumbnail>> {
    let file = fs::File::open(file)?;
    anyhow::ensure!(file.metadata()?.len() <= MAX_BYTES, "oversized cache file");
    let mut r = BufReader::new(file);
    let mut magic = [0; 8];
    r.read_exact(&mut magic)?;
    anyhow::ensure!(&magic == MAGIC, "unsupported cache format");
    let stored: Key = serde_json::from_slice(&read_blob(&mut r, 1024 * 1024)?)?;
    anyhow::ensure!(&stored == key, "stale cache");
    let count = read_u32(&mut r)?;
    anyhow::ensure!(count > 0 && count <= super::MAX_THUMBNAILS, "invalid cache count");
    let mut result = Vec::with_capacity(if target.is_some() { 1 } else { count as usize });
    let mut previous = None;
    let mut best: Option<(f64, Vec<u8>)> = None;
    for _ in 0..count {
        let mut b = [0; 8];
        r.read_exact(&mut b)?;
        let time = f64::from_le_bytes(b);
        anyhow::ensure!(time.is_finite() && time >= 0.0 && previous.is_none_or(|p| p < time), "invalid cached timestamp");
        previous = Some(time);
        let jpeg = read_blob(&mut r, 2 * 1024 * 1024)?;
        if let Some(target) = target {
            if best.as_ref().is_none_or(|(t, _)| (time - target).abs() < (*t - target).abs()) {
                best = Some((time, jpeg));
            }
        } else {
            result.push(decode_jpeg(jpeg, key, time)?);
        }
    }
    if let Some((time, jpeg)) = best { result.push(decode_jpeg(jpeg, key, time)?); }
    Ok(result)
}

fn write_blob(w: &mut impl Write, bytes: &[u8]) -> anyhow::Result<()> {
    w.write_all(&u32::try_from(bytes.len())?.to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

fn save(file: &Path, key: &Key, thumbnails: &[Thumbnail], stop: &AtomicBool) -> anyhow::Result<()> {
    let parent = file.parent().context("invalid cache path")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".tmp-{}-{}", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
    let handle = OpenOptions::new().write(true).create_new(true).open(&temp)?;
    let result = (|| -> anyhow::Result<()> {
        let mut w = BufWriter::new(handle);
        w.write_all(MAGIC)?;
        write_blob(&mut w, &serde_json::to_vec(key)?)?;
        w.write_all(&u32::try_from(thumbnails.len())?.to_le_bytes())?;
        for thumb in thumbnails {
            super::cancelled(stop)?;
            w.write_all(&thumb.time.to_le_bytes())?;
            let rgb = image::DynamicImage::ImageRgba8(thumb.image.clone()).into_rgb8();
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85).encode_image(&rgb)?;
            write_blob(&mut w, &jpeg)?;
        }
        w.flush()?;
        anyhow::ensure!(w.get_ref().metadata()?.len() <= MAX_BYTES, "cache entry too large");
        drop(w);
        super::cancelled(stop)?;
        // Same-directory rename prevents readers observing incomplete records.
        fs::rename(&temp, file)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub directory: PathBuf,
    pub days: u32,
    pub mb: u32,
}

#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub bytes: u64,
    pub count: usize,
}

/// Only remove MavPlayer cache records, never arbitrary files in a chosen folder.
/// Versions 1 and 2 are included so obsolete PNG records can also be cleared.
pub fn maintain(policy: &Policy, clear: bool) -> anyhow::Result<Stats> {
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = Vec::new();
    for (version, magic) in [("v1", b"MAVTHM01"), ("v2", MAGIC)] {
        let dir = policy.directory.join(version);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().and_then(|s| s.strip_suffix(".bin")) else { continue };
            if name.len() != 16 || !name.bytes().all(|b| b.is_ascii_hexdigit()) || !entry.file_type()?.is_file() {
                continue;
            }
            let Ok(mut file) = fs::File::open(entry.path()) else { continue };
            let mut header = [0; 8];
            if file.read_exact(&mut header).is_err() || &header != magic {
                continue;
            }
            let meta = file.metadata()?;
            entries.push((meta.modified()?, meta.len(), entry.path()));
        }
    }
    entries.sort_by_key(|e| e.0);
    let mut stats = Stats { bytes: entries.iter().map(|e| e.1).sum(), count: entries.len() };
    let now = SystemTime::now();
    for (modified, bytes, path) in entries {
        let expired = policy.days != 0 && now.duration_since(modified).unwrap_or_default() >= Duration::from_secs(u64::from(policy.days) * 86400);
        let oversized = policy.mb != 0 && stats.bytes > u64::from(policy.mb) * 1024 * 1024;
        if clear || expired || oversized {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            stats.bytes = stats.bytes.saturating_sub(bytes);
            stats.count -= 1;
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_only_lookup_selects_representative_and_rejects_changed_source() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("mavplayer-cache-test-{}-{}", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&root)?;
        let result = (|| -> anyhow::Result<()> {
            let source = root.join("test.mp4");
            fs::write(&source, b"test media fingerprint")?;
            let spec = ThumbnailSpec { stream: 0, count: 20, width: 8, height: 8 };
            let key = Key::new(&source, spec)?;
            let images: Vec<_> = [0., 10., 20.].into_iter().map(|time| Thumbnail { time, image: image::RgbaImage::from_pixel(8, 8, image::Rgba([30, 50, 70, 255])) }).collect();
            save(&key.file(&root)?, &key, &images, &AtomicBool::new(false))?;
            let representative = load_cached(&source, spec, &root, Some(8.))?;
            assert_eq!(representative.len(), 1);
            assert_eq!(representative[0].time, 10.);
            assert_eq!(load_cached(&source, spec, &root, None)?.len(), 3);
            fs::write(&source, b"new contents of a different size")?;
            assert!(load_cached(&source, spec, &root, None).is_err());
            Ok(())
        })();
        let _ = fs::remove_dir_all(root);
        result
    }

    #[test]
    fn cleanup_preserves_unrelated_files_and_expires_old_records() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("mavplayer-cache-test-{}-{}", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
        let dir = root.join("v2");
        fs::create_dir_all(&dir)?;
        let result = (|| -> anyhow::Result<()> {
            let old = dir.join("0000000000000001.bin");
            let recent = dir.join("0000000000000002.bin");
            let unrelated = dir.join("0000000000000003.bin");
            fs::write(&old, MAGIC)?;
            fs::write(&recent, MAGIC)?;
            fs::write(&unrelated, b"unrelated")?;
            OpenOptions::new().write(true).open(&old)?.set_modified(SystemTime::now() - Duration::from_secs(8 * 86400))?;
            let policy = Policy { directory: root.clone(), days: 7, mb: 0 };
            let stats = maintain(&policy, false)?;
            assert_eq!(stats.count, 1);
            assert!(!old.exists());
            assert!(recent.exists() && unrelated.exists());
            assert_eq!(maintain(&policy, true)?.count, 0);
            assert!(!recent.exists());
            assert!(unrelated.exists());
            Ok(())
        })();
        let _ = fs::remove_dir_all(root);
        result
    }
}
