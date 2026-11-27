//! Persistent, disposable metadata cache, shared by all playlists.

use super::Metadata;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 8 * 1024 * 1024;
// Bound the lock table and serialize identical probes even across workers.
static LOCKS: LazyLock<[parking_lot::Mutex<()>; 64]> =
    LazyLock::new(|| std::array::from_fn(|_| parking_lot::Mutex::new(())));

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    modified: SystemTime,
}

impl Stamp {
    fn read(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        Ok(Self { size: meta.len(), modified: meta.modified()? })
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    version: u32,
    path: PathBuf,
    stamp: Stamp,
    metadata: Metadata,
}

pub(super) fn directory() -> PathBuf {
    dirs::cache_dir().map(|p| p.join("mavplayer")).unwrap_or_else(crate::settings::config_dir).join("metadata")
}

fn absolute_path(path: &Path) -> Option<PathBuf> {
    Some(if path.is_absolute() { path.to_owned() } else { std::env::current_dir().ok()?.join(path) })
}

fn hash_path(path: &Path) -> Option<u64> {
    Some(path.to_str()?.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3)))
}

fn cache_file(directory: &Path, hash: u64) -> PathBuf {
    directory.join("v1").join(format!("{hash:016x}.json"))
}

fn load(file: &Path, path: &Path) -> Option<Entry> {
    let mut bytes = Vec::new();
    File::open(file).ok()?.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_BYTES {
        return None;
    }
    let entry: Entry = serde_json::from_slice(&bytes).ok()?;
    (entry.version == VERSION && entry.path == path).then_some(entry)
}

pub(super) fn snapshot(path: &Path, directory: &Path) -> Option<Metadata> {
    let path = absolute_path(path)?;
    let file = cache_file(directory, hash_path(&path)?);
    // The writer uses an atomic rename; no need to wait for a probing worker.
    Some(load(&file, &path)?.metadata)
}

fn save(file: &Path, entry: &Entry) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(entry)?;
    anyhow::ensure!(bytes.len() as u64 <= MAX_BYTES, "metadata cache entry too large");
    let parent = file.parent().ok_or_else(|| anyhow::anyhow!("invalid cache path"))?;
    crate::engine::thumbnails::cache::create_private_dir(parent)?;
    // Unique even across multiple threads and processes writing the same key.
    let temp = parent.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut handle = opts.open(&temp)?;
    let result = (|| -> anyhow::Result<()> {
        handle.write_all(&bytes)?;
        handle.flush()?;
        drop(handle);
        fs::rename(&temp, file)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Blocking disk work: callers already run on background workers. The path
/// key deliberately avoids canonicalization so offline paths remain usable.
pub(super) fn load_or_probe(
    path: &Path,
    directory: &Path,
    allow_offline: bool,
    probe: impl FnOnce() -> Option<Metadata>,
) -> Option<(Metadata, bool)> {
    let path = absolute_path(path)?;
    let Some(hash) = hash_path(&path) else {
        let mut metadata = probe()?;
        metadata.refresh_sidecars(&path);
        metadata.refresh_camera(&path);
        return Some((metadata, true));
    };
    let _guard = LOCKS[(hash % LOCKS.len() as u64) as usize].lock();
    let file = cache_file(directory, hash);
    let cached = load(&file, &path);
    let stamp = match Stamp::read(&path) {
        Ok(stamp) => stamp,
        Err(err) if err.kind() == io::ErrorKind::InvalidInput => return None,
        Err(_) => return if allow_offline { cached.map(|entry| (entry.metadata, false)) } else { None },
    };
    if let Some(mut entry) = cached
        && entry.stamp == stamp
    {
        log::debug!("metadata cache hit: {}", path.display());
        let camera_changed = entry.metadata.refresh_camera(&path);
        let subtitles_changed = allow_offline && entry.metadata.refresh_sidecars(&path);
        if (camera_changed || subtitles_changed) && let Err(err) = save(&file, &entry) {
            log::debug!("metadata cache: {err}");
        }
        return Some((entry.metadata, true));
    }
    log::debug!("metadata cache miss: {}", path.display());
    let mut metadata = probe()?;
    metadata.refresh_sidecars(&path);
    metadata.refresh_camera(&path);
    // Do not publish a record for a file replaced while FFmpeg was probing it.
    if Stamp::read(&path).ok().as_ref() != Some(&stamp) {
        return None;
    }
    let entry = Entry { version: VERSION, path, stamp, metadata };
    if let Err(err) = save(&file, &entry) {
        log::debug!("metadata cache: {err}");
    }
    Some((entry.metadata, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::{EngineConfig, MediaInfo, Track, TrackId, TrackKind};

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        cache: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("mavplayer-metadata-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let source = root.join("video.mp4");
            fs::write(&source, b"test").unwrap();
            let cache = root.join("cache");
            Self { root, source, cache }
        }

        fn load(&self, offline: bool, probe: impl FnOnce() -> Option<Metadata>) -> Option<(Metadata, bool)> {
            load_or_probe(&self.source, &self.cache, offline, probe)
        }

        fn file(&self) -> PathBuf {
            cache_file(&self.cache, hash_path(&self.source).unwrap())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn metadata(duration: f64) -> Metadata {
        Metadata {
            info: MediaInfo { duration, ..Default::default() },
            video_stream: Some(0),
            default_audio_stream: Some(1),
            sidecars: None,
            video_formats: vec![],
            camera_sidecars: None,
            camera_reader_version: 0,
        }
    }

    #[test]
    fn camera_sidecar_changes_without_media_probe() {
        let f = Fixture::new();
        let sidecar = f.source.with_file_name("videoM01.XML");
        f.load(true, || Some(metadata(42.0))).unwrap();
        fs::write(&sidecar, r#"<NonRealTimeMeta><Device manufacturer="Sony" modelName="ILCE-7M4"/>
            <AcquisitionRecord><Group name="CameraUnitMetadataSet"><Item name="CaptureGammaEquation" value="s-log3-cine"/></Group></AcquisitionRecord></NonRealTimeMeta>"#).unwrap();
        let (m, _) = f.load(true, || panic!("unexpected probe")).unwrap();
        assert_eq!(m.info.camera.unwrap().capture_gamma.as_deref(), Some("s-log3-cine"));
        fs::write(&sidecar, r#"<NonRealTimeMeta><Device manufacturer="Sony" modelName="ILCE-7M4"/>
            <AcquisitionRecord><Group name="CameraUnitMetadataSet"><Item name="CaptureGammaEquation" value="hlg"/></Group></AcquisitionRecord></NonRealTimeMeta>"#).unwrap();
        let (m, _) = f.load(true, || panic!("unexpected probe")).unwrap();
        assert_eq!(m.info.camera.unwrap().capture_gamma.as_deref(), Some("hlg"));
        fs::remove_file(sidecar).unwrap();
        assert!(f.load(true, || panic!("unexpected probe")).unwrap().0.info.camera.is_none());
    }

    #[test]
    fn cache_miss_then_hit_and_changed_source() {
        let f = Fixture::new();
        assert_eq!(f.load(false, || Some(metadata(42.0))).unwrap().0.info.duration, 42.0);
        assert_eq!(f.load(false, || panic!("unexpected probe")).unwrap().0.info.duration, 42.0);
        fs::write(&f.source, b"different size").unwrap();
        assert_eq!(f.load(false, || Some(metadata(12.0))).unwrap().0.info.duration, 12.0);
    }

    #[test]
    fn offline_cache_is_only_used_for_browsing() {
        let f = Fixture::new();
        f.load(false, || Some(metadata(42.0))).unwrap();
        fs::remove_file(&f.source).unwrap();
        assert_eq!(snapshot(&f.source, &f.cache).unwrap().info.duration, 42.0);
        let (m, available) = f.load(true, || panic!("unexpected probe")).unwrap();
        assert_eq!(m.info.duration, 42.0);
        assert!(!available);
        assert!(f.load(false, || panic!("unexpected probe")).is_none());
    }

    #[test]
    fn modification_time_invalidates_same_size_source() {
        let f = Fixture::new();
        f.load(false, || Some(metadata(42.0))).unwrap();
        let handle = OpenOptions::new().write(true).open(&f.source).unwrap();
        handle.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000)).unwrap();
        assert_eq!(f.load(false, || Some(metadata(12.0))).unwrap().0.info.duration, 12.0);
    }

    #[test]
    fn sidecar_changes_do_not_require_a_new_media_probe() {
        let f = Fixture::new();
        let cfg = EngineConfig::default();
        assert!(!f.load(true, || Some(metadata(42.0))).unwrap().0.result(&cfg, true).has_subtitles);
        let subs = f.root.join("Subs");
        fs::create_dir(&subs).unwrap();
        fs::write(subs.join("video.srt"), b"subtitle").unwrap();
        assert!(f.load(true, || panic!("unexpected probe")).unwrap().0.result(&cfg, true).has_subtitles);
        fs::remove_dir_all(subs).unwrap();
        assert!(!f.load(true, || panic!("unexpected probe")).unwrap().0.result(&cfg, true).has_subtitles);
    }

    #[test]
    fn audio_preferences_are_resolved_from_cached_tracks() {
        let f = Fixture::new();
        let track = |id, language: &str| Track {
            id: TrackId::Stream(id),
            kind: TrackKind::Audio,
            codec: "aac".into(),
            language: Some(language.into()),
            title: None,
            default: id == 1,
            forced: false,
            hearing_impaired: false,
            width: 0,
            height: 0,
            fps: 0.0,
            is_cover_art: false,
            channels: 2,
            channel_layout: "stereo".into(),
            sample_rate: 48000,
            bitrate: 128000,
            external_path: None,
        };
        let mut m = metadata(42.0);
        m.info.seekable = true;
        m.info.tracks = vec![track(1, "eng"), track(2, "hrv")];
        let mut cfg = EngineConfig::default();
        assert_eq!(f.load(false, || Some(m)).unwrap().0.result(&cfg, true).audio_stream, Some(1));
        cfg.audio_langs = vec!["hrv".into()];
        assert_eq!(f.load(false, || panic!("unexpected probe")).unwrap().0.result(&cfg, true).audio_stream, Some(2));
        cfg.audio_langs = vec!["jpn".into()];
        assert_eq!(f.load(false, || panic!("unexpected probe")).unwrap().0.result(&cfg, true).audio_stream, Some(1));
    }

    #[test]
    fn corrupt_cache_is_rebuilt() {
        let f = Fixture::new();
        f.load(false, || Some(metadata(42.0))).unwrap();
        fs::write(f.file(), b"invalid json").unwrap();
        assert_eq!(f.load(false, || Some(metadata(12.0))).unwrap().0.info.duration, 12.0);
        assert_eq!(f.load(false, || panic!("unexpected probe")).unwrap().0.info.duration, 12.0);
    }

    #[test]
    fn changed_during_probe_is_not_published() {
        let f = Fixture::new();
        assert!(
            f.load(false, || {
                fs::write(&f.source, b"different size").unwrap();
                Some(metadata(42.0))
            })
            .is_none()
        );
        assert!(!f.file().exists());
    }

    #[test]
    fn concurrent_requests_share_one_probe() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let f = Fixture::new();
        let count = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let result = f.load(false, || {
                        count.fetch_add(1, Ordering::Relaxed);
                        Some(metadata(42.0))
                    });
                    assert!(result.is_some());
                });
            }
        });
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }
}
