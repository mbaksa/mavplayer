//! Individual playlist files. All mutations and recovery require exclusive
//! ownership of the collection; acquiring that lock belongs to the caller.
//! A durable pending manifest is the commit intent for the two-file write.

use super::{CollectionId, CollectionPaths, PlaylistEntry, PlaylistId, PlaylistManifest};
use crate::engine::MediaSource;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn conflict(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, message)
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(data) => Ok(Some(data)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(super) fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("Missing playlist directory"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".playlist-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temp)?;
    let written = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    let result = written.and_then(|_| fs::rename(&temp, path)).and_then(|_| sync_directory(parent));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

// The M3U representation must never silently omit or split an entry. Relative
// local sources must be made absolute by the caller before storing a manifest.
fn source_line(source: &MediaSource) -> io::Result<String> {
    let line = match source {
        MediaSource::File(path) => {
            if !path.is_absolute() {
                return Err(invalid("Internal playlists require absolute media paths"));
            }
            path.to_str().ok_or_else(|| invalid("Media path is not UTF-8"))?.to_owned()
        }
        _ => source.key(),
    };
    if line.is_empty() || line.trim() != line || line.starts_with('#') || line.contains(['\r', '\n', '\0']) {
        return Err(invalid("Media source cannot be represented in M3U8"));
    }
    if matches!(source, MediaSource::Url(_)) && !line.contains("://") {
        return Err(invalid("URL cannot be represented in M3U8"));
    }
    Ok(line)
}

fn render_m3u(manifest: &PlaylistManifest) -> io::Result<Vec<u8>> {
    let mut text = String::from("#EXTM3U\n");
    for entry in &manifest.entries {
        let line = source_line(&entry.source)?;
        if let Some(title) = &entry.custom_title {
            text.push_str(&format!("#EXTINF:-1,{}\n", title.replace(['\r', '\n', '\0'], " ")));
        }
        text.push_str(&line);
        text.push('\n');
    }
    Ok(text.into_bytes())
}

fn m3u_lines(bytes: &[u8]) -> io::Result<Vec<&str>> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("Playlist is not UTF-8"))?;
    let text = text.trim_start_matches('\u{feff}');
    if text.contains('\0') || text.lines().any(|line| line.trim_start().starts_with("#EXT-X-")) {
        return Err(invalid("Invalid internal playlist or HLS manifest"));
    }
    let lines: Vec<_> = text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')).collect();
    if lines.is_empty() && !text.lines().any(|line| line.trim() == "#EXTM3U") {
        return Err(invalid("Empty playlist without an M3U header"));
    }
    Ok(lines)
}

// Do not probe a path or require that its drive is online when reconciling.
fn new_source(line: &str, base: &Path) -> io::Result<MediaSource> {
    if let Some(path) = super::super::file_uri_path(line) {
        if !path.is_absolute() {
            return Err(invalid("File URI is not an absolute path"));
        }
        return Ok(MediaSource::File(path));
    }
    for (prefix, dvd) in [("dvd://", true), ("cdda://", false)] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let (number, path) = rest.split_once('/').ok_or_else(|| invalid("Invalid disc source"))?;
            let number = number.parse::<u32>().map_err(|_| invalid("Invalid disc track or title"))?;
            if path.is_empty() {
                return Err(invalid("Missing disc path"));
            }
            return Ok(if dvd {
                MediaSource::Dvd { path: path.into(), title: number, angle: 1 }
            } else {
                MediaSource::Cd { device: path.into(), track: number }
            });
        }
    }
    let path = Path::new(line);
    if path.is_absolute() {
        return Ok(MediaSource::File(path.to_owned()));
    }
    if line.contains("://") {
        return Ok(MediaSource::Url(line.into()));
    }
    Ok(MediaSource::File(std::path::absolute(base.join(path))?))
}

// External M3U edits own the source sequence, JSON owns item-specific data.
// A duplicate source has no occurrence identity in ordinary M3U, so refuse
// ambiguous edits instead of guessing which occurrence gets a custom title.
fn reconcile(manifest: &mut PlaylistManifest, bytes: &[u8], base: &Path) -> io::Result<bool> {
    let old_lines: Vec<_> = manifest.entries.iter().map(|entry| source_line(&entry.source)).collect::<io::Result<_>>()?;
    let new_lines = m3u_lines(bytes)?;
    if old_lines.iter().map(String::as_str).eq(new_lines.iter().copied()) {
        return Ok(false);
    }
    let mut old: HashMap<&str, Vec<&PlaylistEntry>> = HashMap::new();
    for (line, entry) in old_lines.iter().zip(&manifest.entries) {
        old.entry(line.as_str()).or_default().push(entry);
    }
    // A relative path or file URI may refer to exactly the same absolute
    // path. Resolve it lexically, without accessing the media filesystem.
    let normalized = new_lines
        .iter()
        .map(|&line| {
            if old.contains_key(line) {
                Ok((line.to_owned(), None))
            } else {
                let source = new_source(line, base)?;
                Ok((source_line(&source)?, Some(source)))
            }
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut counts = HashMap::new();
    for (line, _) in &normalized {
        *counts.entry(line.as_str()).or_insert(0usize) += 1;
    }
    for (line, entries) in &old {
        if let Some(&count) = counts.get(*line) {
            if entries.len() != 1 || count != 1 {
                return Err(invalid("Ambiguous external edit of duplicate playlist entries"));
            }
        }
    }
    drop(counts);
    let entries: Vec<_> = normalized
        .into_iter()
        .map(|(line, source)| match old.get(line.as_str()) {
            Some(entries) => Ok((*entries[0]).clone()),
            None => source.map(PlaylistEntry::new).ok_or_else(|| invalid("Missing playlist source")),
        })
        .collect::<io::Result<_>>()?;
    manifest.entries = entries;
    if manifest.current.is_some_and(|id| !manifest.entries.iter().any(|entry| entry.id == id)) {
        manifest.current = None;
        manifest.position = None;
    }
    manifest.validate().map_err(invalid)?;
    Ok(true)
}

#[derive(Debug)]
pub(crate) struct LoadedPlaylist {
    pub manifest: PlaylistManifest,
    pub externally_edited: bool,
    // Exact bytes are an optimistic revision check, including edits made by
    // programs that don't understand our JSON revision counter.
    original_json: Vec<u8>,
    original_m3u: Vec<u8>,
}

pub(crate) struct PlaylistStore {
    id: PlaylistId,
    json: PathBuf,
    m3u: PathBuf,
    pending: PathBuf,
}

impl PlaylistStore {
    pub fn new(paths: &CollectionPaths, collection: CollectionId, playlist: PlaylistId) -> Self {
        let json = paths.playlist_manifest(collection, playlist);
        let m3u = paths.playlist_m3u8(collection, playlist);
        let pending = json.with_extension("pending.json");
        Self { id: playlist, json, m3u, pending }
    }

    fn checked_manifest(&self, bytes: &[u8]) -> io::Result<PlaylistManifest> {
        let manifest: PlaylistManifest = serde_json::from_slice(bytes)?;
        self.validate(&manifest)?;
        Ok(manifest)
    }

    fn validate(&self, manifest: &PlaylistManifest) -> io::Result<()> {
        manifest.validate().map_err(invalid)?;
        if manifest.id != self.id {
            return Err(invalid("Playlist ID does not match storage path"));
        }
        Ok(())
    }

    fn ensure_no_pending(&self) -> io::Result<()> {
        match fs::metadata(&self.pending) {
            Ok(_) => Err(conflict("Playlist transaction requires recovery")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Read-only loading never modifies the pair or recovers a transaction.
    pub fn load(&self) -> io::Result<LoadedPlaylist> {
        self.ensure_no_pending()?;
        let original_json = fs::read(&self.json)?;
        let original_m3u = fs::read(&self.m3u)?;
        self.ensure_no_pending()?;
        if fs::read(&self.json)? != original_json {
            return Err(conflict("Playlist changed while loading; retry"));
        }
        let mut manifest = self.checked_manifest(&original_json)?;
        let externally_edited = reconcile(&mut manifest, &original_m3u, self.m3u.parent().unwrap_or(Path::new(".")))?;
        Ok(LoadedPlaylist { manifest, externally_edited, original_json, original_m3u })
    }

    /// Create a new pair without overwriting an existing playlist.
    pub fn create(&self, mut manifest: PlaylistManifest) -> io::Result<LoadedPlaylist> {
        self.ensure_no_pending()?;
        if read_optional(&self.json)?.is_some() || read_optional(&self.m3u)?.is_some() {
            return Err(conflict("Playlist already exists"));
        }
        manifest.revision = 1;
        self.commit(manifest)
    }

    /// Save a previously loaded (or created) playlist. On any failure after
    /// writing the intent, recover and reload before attempting another save.
    pub fn save(&self, loaded: &mut LoadedPlaylist) -> io::Result<()> {
        self.ensure_no_pending()?;
        if fs::read(&self.json)? != loaded.original_json || fs::read(&self.m3u)? != loaded.original_m3u {
            return Err(conflict("Playlist changed on disk; reload before saving"));
        }
        let old = self.checked_manifest(&loaded.original_json)?;
        let mut next = loaded.manifest.clone();
        next.revision = old.revision;
        if next == old && !loaded.externally_edited {
            loaded.manifest.revision = old.revision;
            return Ok(());
        }
        next.revision = old.revision.checked_add(1).ok_or_else(|| invalid("Playlist revision overflow"))?;
        *loaded = self.commit(next)?;
        Ok(())
    }

    fn commit(&self, manifest: PlaylistManifest) -> io::Result<LoadedPlaylist> {
        self.validate(&manifest)?;
        let original_m3u = render_m3u(&manifest)?;
        let original_json = serde_json::to_vec_pretty(&manifest)?;
        write_atomic(&self.pending, &original_json)?;
        self.finish(&original_json, &original_m3u)?;
        Ok(LoadedPlaylist { manifest, externally_edited: false, original_json, original_m3u })
    }

    fn finish(&self, json: &[u8], m3u: &[u8]) -> io::Result<()> {
        // Position-only and other JSON-only updates need not rewrite M3U.
        if read_optional(&self.m3u)?.as_deref() != Some(m3u) {
            write_atomic(&self.m3u, m3u)?;
        }
        if read_optional(&self.json)?.as_deref() != Some(json) {
            write_atomic(&self.json, json)?;
        }
        fs::remove_file(&self.pending)?;
        sync_directory(self.pending.parent().unwrap_or(Path::new(".")))
    }

    /// Roll forward a committed intent after a process crash. Idempotent as
    /// long as the caller holds exclusive collection ownership.
    pub fn recover(&self) -> io::Result<bool> {
        let Some(json) = read_optional(&self.pending)? else { return Ok(false) };
        let manifest = self.checked_manifest(&json)?;
        if let Some(current) = read_optional(&self.json)? {
            let current = self.checked_manifest(&current)?;
            if current.revision > manifest.revision || (current.revision == manifest.revision && current != manifest) {
                return Err(conflict("Pending transaction conflicts with current playlist revision"));
            }
        }
        let m3u = render_m3u(&manifest)?;
        self.finish(&json, &m3u)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        store: PlaylistStore,
        manifest: PlaylistManifest,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("mavplayer-playlist-test-{}", Uuid::new_v4()));
            let paths = CollectionPaths::new(&root);
            let mut manifest = PlaylistManifest::new("Test playlist".into());
            let mut a = PlaylistEntry::new(MediaSource::File(root.join("Šuma.mp4")));
            a.custom_title = Some("Forest".into());
            manifest.current = Some(a.id);
            manifest.position = Some(12.5);
            manifest.entries = vec![a, PlaylistEntry::new(MediaSource::Url("https://example.com/video".into()))];
            let store = PlaylistStore::new(&paths, CollectionId::new(), manifest.id);
            Self { root, store, manifest }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn roundtrip_and_internal_reorder_preserve_occurrence_ids() {
        let f = Fixture::new();
        let mut manifest = f.manifest.clone();
        let mut duplicate = PlaylistEntry::new(manifest.entries[0].source.clone());
        duplicate.custom_title = Some("Different title".into());
        let duplicate_id = duplicate.id;
        manifest.entries.push(duplicate);
        let mut loaded = f.store.create(manifest).unwrap();
        assert_eq!(loaded.manifest.revision, 1);
        f.store.save(&mut loaded).unwrap();
        assert_eq!(loaded.manifest.revision, 1);
        assert_eq!(f.store.load().unwrap().manifest, loaded.manifest);
        loaded.manifest.entries.swap(0, 2);
        loaded.manifest.autosave = true;
        f.store.save(&mut loaded).unwrap();
        let restored = f.store.load().unwrap();
        assert_eq!(restored.manifest, loaded.manifest);
        assert_eq!(restored.manifest.revision, 2);
        assert_eq!(restored.manifest.entries[0].id, duplicate_id);
        assert_eq!(restored.manifest.entries[0].custom_title.as_deref(), Some("Different title"));
        assert_eq!(restored.manifest.current, f.manifest.current);
        assert_eq!(restored.manifest.position, Some(12.5));
        assert!(!restored.externally_edited);
        assert!(f.store.create(f.manifest.clone()).is_err());
    }

    #[test]
    fn external_reorder_and_new_entries_preserve_unambiguous_metadata() {
        let f = Fixture::new();
        f.store.create(f.manifest.clone()).unwrap();
        let old = &f.manifest.entries[0];
        let line = source_line(&old.source).unwrap();
        let m3u = format!("\u{feff}#EXTM3U\r\nhttps://example.com/video\r\n{line}\r\nnew.mp4\r\n");
        fs::write(&f.store.m3u, m3u.as_bytes()).unwrap();
        let mut loaded = f.store.load().unwrap();
        assert!(loaded.externally_edited);
        assert_eq!(&loaded.manifest.entries[1], old);
        assert_eq!(loaded.manifest.current, Some(old.id));
        assert_eq!(loaded.manifest.entries[2].source, MediaSource::File(f.store.m3u.parent().unwrap().join("new.mp4")));
        assert_eq!(fs::read(&f.store.m3u).unwrap(), m3u.as_bytes());
        f.store.save(&mut loaded).unwrap();
        assert!(!f.store.load().unwrap().externally_edited);
        assert_eq!(f.store.load().unwrap().manifest, loaded.manifest);
    }

    #[test]
    fn relative_external_path_preserves_existing_item() {
        let f = Fixture::new();
        let mut manifest = f.manifest.clone();
        manifest.entries[0].source = MediaSource::File(f.store.m3u.parent().unwrap().join("clip.mp4"));
        let id = manifest.entries[0].id;
        f.store.create(manifest).unwrap();
        fs::write(&f.store.m3u, "#EXTM3U\nclip.mp4\nhttps://example.com/video\n").unwrap();
        let loaded = f.store.load().unwrap();
        assert!(loaded.externally_edited);
        assert_eq!(loaded.manifest.entries[0].id, id);
        assert_eq!(loaded.manifest.entries[0].custom_title.as_deref(), Some("Forest"));
    }

    #[test]
    fn external_removal_clears_resume_position() {
        let f = Fixture::new();
        f.store.create(f.manifest.clone()).unwrap();
        fs::write(&f.store.m3u, "#EXTM3U\n").unwrap();
        let loaded = f.store.load().unwrap();
        assert!(loaded.externally_edited);
        assert!(loaded.manifest.entries.is_empty());
        assert_eq!(loaded.manifest.current, None);
        assert_eq!(loaded.manifest.position, None);
    }

    #[test]
    fn ambiguous_external_duplicate_edit_is_not_written() {
        let f = Fixture::new();
        let mut manifest = f.manifest.clone();
        let duplicate = PlaylistEntry::new(manifest.entries[0].source.clone());
        manifest.entries.push(duplicate);
        let loaded = f.store.create(manifest).unwrap();
        let line = source_line(&loaded.manifest.entries[0].source).unwrap();
        let m3u = format!("#EXTM3U\n{line}\nhttps://example.com/video\n");
        fs::write(&f.store.m3u, &m3u).unwrap();
        assert_eq!(f.store.load().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&f.store.json).unwrap(), loaded.original_json);
        assert_eq!(fs::read_to_string(&f.store.m3u).unwrap(), m3u);
    }

    #[test]
    fn save_rejects_stale_load_and_external_edits() {
        let f = Fixture::new();
        let mut first = f.store.create(f.manifest.clone()).unwrap();
        let mut stale = f.store.load().unwrap();
        first.manifest.name = "Renamed".into();
        f.store.save(&mut first).unwrap();
        assert!(f.store.save(&mut stale).is_err());
        fs::write(&f.store.m3u, "#EXTM3U\n").unwrap();
        assert!(f.store.save(&mut first).is_err());
        assert!(!f.store.pending.exists());
    }

    #[test]
    fn pending_transaction_recovers_after_only_m3u_was_replaced() {
        let f = Fixture::new();
        let mut next = f.store.create(f.manifest.clone()).unwrap().manifest;
        next.revision += 1;
        next.entries.swap(0, 1);
        let json = serde_json::to_vec_pretty(&next).unwrap();
        write_atomic(&f.store.pending, &json).unwrap();
        write_atomic(&f.store.m3u, &render_m3u(&next).unwrap()).unwrap();
        assert!(f.store.load().is_err());
        assert!(f.store.recover().unwrap());
        assert!(!f.store.recover().unwrap());
        assert_eq!(f.store.load().unwrap().manifest, next);
    }

    #[test]
    fn pending_create_recovers_and_corrupt_intent_is_not_overwritten() {
        let f = Fixture::new();
        let mut manifest = f.manifest.clone();
        manifest.revision = 1;
        write_atomic(&f.store.pending, &serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        assert!(f.store.recover().unwrap());
        assert_eq!(f.store.load().unwrap().manifest, manifest);
        write_atomic(&f.store.pending, b"invalid JSON").unwrap();
        assert!(f.store.recover().is_err());
        assert_eq!(fs::read(&f.store.pending).unwrap(), b"invalid JSON");
    }

    #[test]
    fn disc_source_parameters_survive_json_roundtrip() {
        let f = Fixture::new();
        let mut manifest = f.manifest.clone();
        manifest.entries.push(PlaylistEntry::new(MediaSource::Dvd { path: "/dev/sr0".into(), title: 2, angle: 3 }));
        manifest.entries.push(PlaylistEntry::new(MediaSource::Cd { device: "/dev/sr0".into(), track: 4 }));
        let loaded = f.store.create(manifest).unwrap();
        assert_eq!(f.store.load().unwrap().manifest, loaded.manifest);
    }

    #[test]
    fn invalid_sources_and_hls_are_rejected_without_writing() {
        let f = Fixture::new();
        let mut manifest = f.manifest.clone();
        manifest.entries.push(PlaylistEntry::new(MediaSource::File("relative.mp4".into())));
        assert!(f.store.create(manifest).is_err());
        assert!(!f.store.pending.exists());
        assert!(m3u_lines(b"#EXTM3U\n#EXT-X-VERSION:3\n").is_err());
        assert!(m3u_lines(b"").is_err());
        assert!(m3u_lines(b"\xff").is_err());
    }
}
