//! Versioned, lossless cache of timeline barcode images (BGRA bytes).
use super::Thumbnail;
use image::RgbaImage;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

pub(super) const MAGIC: &[u8; 8] = b"MAVBAR01";
const DIRECTORY: &str = "barcode-v1";
const OFFSET: u64 = 0xcbf29ce484222325;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn hash(seed: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(seed, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

/// Hash the actual analysis input, not cache timestamps (which change on reads).
/// Call on a worker; reuse the result for all modes of the same sample set.
pub fn fingerprint(samples: &[Thumbnail]) -> u64 {
    let mut h = hash(OFFSET, &(samples.len() as u64).to_le_bytes());
    for sample in samples {
        h = hash(h, &sample.time.to_bits().to_le_bytes());
        h = hash(h, &sample.image.width().to_le_bytes());
        h = hash(h, &sample.image.height().to_le_bytes());
        h = hash(h, sample.image.as_raw());
    }
    h
}

pub struct Key {
    bytes: [u8; 25],
    width: u32,
    height: u32,
}

impl Key {
    pub fn new(samples: u64, duration: f64, mode: u8, width: u32, height: u32) -> Self {
        let mut bytes = [0; 25];
        bytes[..8].copy_from_slice(&samples.to_le_bytes());
        bytes[8..16].copy_from_slice(&duration.to_bits().to_le_bytes());
        bytes[16] = mode;
        bytes[17..21].copy_from_slice(&width.to_le_bytes());
        bytes[21..25].copy_from_slice(&height.to_le_bytes());
        Self { bytes, width, height }
    }

    fn file(&self, directory: &Path) -> PathBuf {
        directory.join(DIRECTORY).join(format!("{:016x}.bin", hash(OFFSET, &self.bytes)))
    }

    fn byte_len(&self) -> anyhow::Result<usize> {
        anyhow::ensure!((1..=480).contains(&self.width) && (1..=32).contains(&self.height), "invalid barcode dimensions");
        Ok((self.width * self.height * 4) as usize)
    }
}

pub fn load(directory: &Path, key: &Key) -> anyhow::Result<RgbaImage> {
    let file = key.file(directory);
    let mut r = fs::File::open(&file)?;
    let len = key.byte_len()?;
    anyhow::ensure!(r.metadata()?.len() == (8 + 25 + 8 + len) as u64, "invalid barcode length");
    let mut magic = [0; 8];
    r.read_exact(&mut magic)?;
    anyhow::ensure!(&magic == MAGIC, "invalid barcode format");
    let mut stored = [0; 25];
    r.read_exact(&mut stored)?;
    anyhow::ensure!(stored == key.bytes, "stale barcode");
    let mut checksum = [0; 8];
    r.read_exact(&mut checksum)?;
    let mut pixels = vec![0; len];
    r.read_exact(&mut pixels)?;
    anyhow::ensure!(hash(OFFSET, &pixels) == u64::from_le_bytes(checksum), "damaged barcode");
    let image = RgbaImage::from_raw(key.width, key.height, pixels).ok_or_else(|| anyhow::anyhow!("invalid barcode pixels"))?;
    if let Ok(handle) = OpenOptions::new().write(true).open(&file) { let _ = handle.set_modified(SystemTime::now()); }
    Ok(image)
}

pub fn save(directory: &Path, key: &Key, image: &RgbaImage, stop: &AtomicBool) -> anyhow::Result<()> {
    anyhow::ensure!(image.dimensions() == (key.width, key.height) && image.as_raw().len() == key.byte_len()?, "invalid barcode image");
    let _guard = super::cache::CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    super::cancelled(stop)?;
    let file = key.file(directory);
    let parent = directory.join(DIRECTORY);
    fs::create_dir_all(&parent)?;
    let temp = parent.join(format!(".tmp-{}-{}", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
    let mut w = OpenOptions::new().write(true).create_new(true).open(&temp)?;
    let result = (|| -> anyhow::Result<()> {
        w.write_all(MAGIC)?;
        w.write_all(&key.bytes)?;
        w.write_all(&hash(OFFSET, image.as_raw()).to_le_bytes())?;
        w.write_all(image.as_raw())?;
        w.flush()?;
        drop(w);
        super::cancelled(stop)?;
        fs::rename(&temp, &file)?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_roundtrip_invalidation_and_clear() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("mavplayer-barcode-test-{}-{}", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
        let result = (|| -> anyhow::Result<()> {
            let stop = AtomicBool::new(false);
            let mut samples = [Thumbnail { time: 1.0, image: RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255])) }];
            let id = fingerprint(&samples);
            let key = Key::new(id, 10.0, 1, 480, 1);
            let image = RgbaImage::from_pixel(480, 1, image::Rgba([30, 20, 10, 255]));
            save(&root, &key, &image, &stop)?;
            assert_eq!(load(&root, &key)?, image);
            assert!(load(&root, &Key::new(id, 11.0, 1, 480, 1)).is_err());
            assert!(load(&root, &Key::new(id, 10.0, 2, 480, 1)).is_err());
            samples[0].image.put_pixel(0, 0, image::Rgba([9, 2, 3, 255]));
            assert_ne!(fingerprint(&samples), id);
            assert!(load(&root, &Key::new(fingerprint(&samples), 10.0, 1, 480, 1)).is_err());
            samples[0].image.put_pixel(0, 0, image::Rgba([1, 2, 3, 255]));
            assert_eq!(fingerprint(&samples), id);
            samples[0].time = 2.0;
            assert_ne!(fingerprint(&samples), id);
            let policy = super::super::cache::Policy { directory: root.clone(), days: 0, mb: 0 };
            assert_eq!(super::super::cache::maintain(&policy, false)?.count, 1);
            let mut bytes = fs::read(key.file(&root))?;
            *bytes.last_mut().unwrap() ^= 1;
            fs::write(key.file(&root), bytes)?;
            assert!(load(&root, &key).is_err());
            stop.store(true, Ordering::Relaxed);
            assert!(save(&root, &key, &image, &stop).is_err());
            assert_eq!(super::super::cache::maintain(&policy, true)?.count, 0);
            assert!(load(&root, &key).is_err());
            Ok(())
        })();
        let _ = fs::remove_dir_all(&root);
        result
    }
}
