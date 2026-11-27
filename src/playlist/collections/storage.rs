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
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
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
    let old_lines: Vec<_> =
        manifest.entries.iter().map(|entry| source_line(&entry.source)).collect::<io::Result<_>>()?;
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

// A collection-wide intent coordinates both playlist pairs. Exact before/after
// bytes allow recovery of a partially written pair without overwriting edits.
#[derive(serde::Serialize, serde::Deserialize)]
struct MovePair {
    id: PlaylistId,
    before_json: Vec<u8>,
    before_m3u: Vec<u8>,
    after_json: Vec<u8>,
    after_m3u: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct MoveIntent {
    version: u32,
    transaction: Uuid,
    pairs: [MovePair; 2],
}

fn move_path(paths: &CollectionPaths, collection: CollectionId) -> PathBuf {
    paths.collection_dir(collection).join("move.pending.json")
}

pub(crate) fn recover_move(paths: &CollectionPaths, collection: CollectionId, read_only: bool) -> io::Result<()> {
    let path = move_path(paths, collection);
    let Some(bytes) = read_optional(&path)? else { return Ok(()) };
    if read_only {
        return Err(conflict("Collection move requires writable recovery"));
    }
    let intent: MoveIntent = serde_json::from_slice(&bytes)?;
    if intent.version != 1 || intent.transaction.is_nil() || intent.pairs[0].id == intent.pairs[1].id {
        return Err(invalid("Invalid move intent"));
    }
    // Validate every participant before touching either one.
    for pair in &intent.pairs {
        let store = PlaylistStore::new(paths, collection, pair.id);
        store.ensure_no_pending()?;
        let before = store.checked_manifest(&pair.before_json)?;
        let after = store.checked_manifest(&pair.after_json)?;
        if before.revision.checked_add(1) != Some(after.revision) || render_m3u(&after)? != pair.after_m3u {
            return Err(invalid("Invalid move revision or M3U"));
        }
        for (file, before, after) in
            [(&store.json, &pair.before_json, &pair.after_json), (&store.m3u, &pair.before_m3u, &pair.after_m3u)]
        {
            let current = fs::read(file)?;
            if &current != before && &current != after {
                return Err(conflict("Move conflicts with newer playlist data"));
            }
        }
    }
    // Destination is always first. Source removal follows durable publication.
    for pair in &intent.pairs {
        let store = PlaylistStore::new(paths, collection, pair.id);
        for (file, after) in [(&store.m3u, &pair.after_m3u), (&store.json, &pair.after_json)] {
            if fs::read(file)? != *after {
                write_atomic(file, after)?;
            }
        }
    }
    fs::remove_file(&path)?;
    sync_directory(path.parent().unwrap_or(Path::new(".")))
}

pub(crate) fn move_items_transaction(
    paths: &CollectionPaths,
    collection: CollectionId,
    destination: &LoadedPlaylist,
    source: &LoadedPlaylist,
    mut destination_next: PlaylistManifest,
    mut source_next: PlaylistManifest,
) -> io::Result<()> {
    let path = move_path(paths, collection);
    if read_optional(&path)?.is_some() {
        return Err(conflict("Previous move requires recovery"));
    }
    fn pair(
        paths: &CollectionPaths,
        collection: CollectionId,
        loaded: &LoadedPlaylist,
        next: &mut PlaylistManifest,
    ) -> io::Result<MovePair> {
        let store = PlaylistStore::new(paths, collection, loaded.manifest.id);
        store.ensure_no_pending()?;
        if fs::read(&store.json)? != loaded.original_json || fs::read(&store.m3u)? != loaded.original_m3u {
            return Err(conflict("Playlist changed on disk; reload before moving"));
        }
        next.revision = loaded.manifest.revision.checked_add(1).ok_or_else(|| invalid("Playlist revision overflow"))?;
        store.validate(next)?;
        Ok(MovePair {
            id: next.id,
            before_json: loaded.original_json.clone(),
            before_m3u: loaded.original_m3u.clone(),
            after_json: serde_json::to_vec_pretty(next)?,
            after_m3u: render_m3u(next)?,
        })
    }
    let intent = MoveIntent {
        version: 1,
        transaction: Uuid::new_v4(),
        pairs: [
            pair(paths, collection, destination, &mut destination_next)?,
            pair(paths, collection, source, &mut source_next)?,
        ],
    };
    write_atomic(&path, &serde_json::to_vec(&intent)?)?;
    recover_move(paths, collection, false)
}

// A single cross-collection intent lives outside either participant. Both
// collection locks must be held before publishing or replaying it.
#[derive(serde::Serialize, serde::Deserialize)]
struct PlaylistMoveIntent {
    version: u32,
    transaction: Uuid,
    source_before: super::CollectionManifest,
    source_after: super::CollectionManifest,
    destination_before: super::CollectionManifest,
    destination_after: super::CollectionManifest,
    playlist: PlaylistId,
    source_json: Vec<u8>,
    source_m3u: Vec<u8>,
    destination_json: Vec<u8>,
    destination_m3u: Vec<u8>,
}

fn playlist_move_path(paths: &CollectionPaths) -> PathBuf {
    paths.index().with_file_name("playlist-move.pending.json")
}

fn playlist_move_lock(paths: &CollectionPaths) -> io::Result<fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.index().with_file_name("playlist-move.lock"))?;
    file.try_lock().map_err(|err| io::Error::other(format!("Playlist transfer busy: {err}")))?;
    Ok(file)
}

fn finish_playlist_move(paths: &CollectionPaths, intent: &PlaylistMoveIntent) -> io::Result<()> {
    let source = intent.source_before.id;
    let destination = intent.destination_before.id;
    let id = intent.playlist;
    if intent.version != 1
        || intent.transaction.is_nil()
        || source == destination
        || intent.source_after.id != source
        || intent.destination_after.id != destination
        || !intent.source_before.playlist_order.contains(&id)
        || intent.destination_before.playlist_order.contains(&id)
    {
        return Err(invalid("Invalid playlist transfer intent"));
    }
    let mut expected_source = intent.source_before.clone();
    expected_source.playlist_order.retain(|entry| *entry != id);
    if expected_source.active_playlist == Some(id) {
        expected_source.active_playlist = expected_source.playlist_order.first().copied();
    }
    let mut expected_destination = intent.destination_before.clone();
    expected_destination.playlist_order.push(id);
    if expected_source.playlist_order.is_empty()
        || expected_source != intent.source_after
        || expected_destination != intent.destination_after
    {
        return Err(invalid("Invalid playlist transfer manifests"));
    }
    let src = PlaylistStore::new(paths, source, id);
    let dst = PlaylistStore::new(paths, destination, id);
    src.ensure_no_pending()?;
    dst.ensure_no_pending()?;
    let original = src.checked_manifest(&intent.source_json)?;
    let moved = dst.checked_manifest(&intent.destination_json)?;
    let mut expected = original.clone();
    expected.name = moved.name.clone();
    expected.revision = original.revision.checked_add(1).ok_or_else(|| invalid("Playlist revision overflow"))?;
    if expected != moved || render_m3u(&moved)? != intent.destination_m3u {
        return Err(conflict("Playlist transfer conflicts with source data"));
    }
    for (before, after) in
        [(&intent.source_before, &intent.source_after), (&intent.destination_before, &intent.destination_after)]
    {
        before.validate().map_err(invalid)?;
        after.validate().map_err(invalid)?;
        let current: super::CollectionManifest =
            serde_json::from_slice(&fs::read(paths.collection_manifest(before.id))?)?;
        if current != *before && current != *after {
            return Err(conflict("Playlist transfer conflicts with collection data"));
        }
    }
    let source_current: super::CollectionManifest =
        serde_json::from_slice(&fs::read(paths.collection_manifest(source))?)?;
    for (path, bytes) in [(&src.json, &intent.source_json), (&src.m3u, &intent.source_m3u)] {
        match read_optional(path)? {
            Some(current) if current == *bytes => {}
            None if source_current == intent.source_after => {}
            _ => return Err(conflict("Playlist transfer conflicts with source files")),
        }
    }
    for (path, bytes) in [(&dst.json, &intent.destination_json), (&dst.m3u, &intent.destination_m3u)] {
        if let Some(current) = read_optional(path)?
            && current != *bytes
        {
            return Err(conflict("Playlist transfer destination already exists"));
        }
    }
    // Publish the destination pair and membership before removing source
    // membership. Remove source files only after both manifests are durable.
    for (path, bytes) in [(&dst.m3u, &intent.destination_m3u), (&dst.json, &intent.destination_json)] {
        if read_optional(path)?.as_ref() != Some(bytes) {
            write_atomic(path, bytes)?;
        }
    }
    for manifest in [&intent.destination_after, &intent.source_after] {
        write_atomic(&paths.collection_manifest(manifest.id), &serde_json::to_vec_pretty(manifest)?)?;
    }
    for path in [&src.json, &src.m3u] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    sync_directory(src.json.parent().unwrap_or(Path::new(".")))?;
    let path = playlist_move_path(paths);
    fs::remove_file(&path)?;
    sync_directory(path.parent().unwrap_or(Path::new(".")))
}

pub(crate) fn recover_playlist_move(
    paths: &CollectionPaths,
    collection: CollectionId,
    read_only: bool,
) -> io::Result<()> {
    let _guard = playlist_move_lock(paths)?;
    let Some(bytes) = read_optional(&playlist_move_path(paths))? else { return Ok(()) };
    let intent: PlaylistMoveIntent = serde_json::from_slice(&bytes)?;
    let other = if collection == intent.source_before.id {
        intent.destination_before.id
    } else if collection == intent.destination_before.id {
        intent.source_before.id
    } else {
        return Ok(());
    };
    if read_only {
        return Err(conflict("Playlist transfer requires writable recovery"));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.collection_dir(other).join("collection.lock"))?;
    lock.try_lock()
        .map_err(|err| io::Error::other(format!("Playlist transfer recovery requires both collections: {err}")))?;
    finish_playlist_move(paths, &intent)
}

pub(crate) fn move_playlist_transaction(
    paths: &CollectionPaths,
    source: &super::CollectionManifest,
    destination: &super::CollectionManifest,
    loaded: &LoadedPlaylist,
    name: String,
) -> io::Result<(super::CollectionManifest, super::CollectionManifest, LoadedPlaylist)> {
    let _guard = playlist_move_lock(paths)?;
    let path = playlist_move_path(paths);
    if read_optional(&path)?.is_some() {
        return Err(conflict("Previous playlist transfer requires recovery"));
    }
    let id = loaded.manifest.id;
    let src = PlaylistStore::new(paths, source.id, id);
    let dst = PlaylistStore::new(paths, destination.id, id);
    src.ensure_no_pending()?;
    dst.ensure_no_pending()?;
    if fs::read(&src.json)? != loaded.original_json
        || fs::read(&src.m3u)? != loaded.original_m3u
        || loaded.externally_edited
        || read_optional(&dst.json)?.is_some()
        || read_optional(&dst.m3u)?.is_some()
    {
        return Err(conflict("Playlist changed on disk or destination files already exist"));
    }
    for manifest in [source, destination] {
        let current: super::CollectionManifest =
            serde_json::from_slice(&fs::read(paths.collection_manifest(manifest.id))?)?;
        if current != *manifest {
            return Err(conflict("Collection changed on disk; reopen before moving"));
        }
    }
    let mut source_after = source.clone();
    source_after.playlist_order.retain(|entry| *entry != id);
    if source_after.active_playlist == Some(id) {
        source_after.active_playlist = source_after.playlist_order.first().copied();
    }
    let mut destination_after = destination.clone();
    destination_after.playlist_order.push(id);
    source_after.validate().map_err(invalid)?;
    destination_after.validate().map_err(invalid)?;
    if source_after.playlist_order.is_empty() || !source.playlist_order.contains(&id) || source.id == destination.id {
        return Err(invalid("Invalid playlist transfer"));
    }
    let mut manifest = loaded.manifest.clone();
    manifest.name = name;
    manifest.revision = manifest.revision.checked_add(1).ok_or_else(|| invalid("Playlist revision overflow"))?;
    dst.validate(&manifest)?;
    let intent = PlaylistMoveIntent {
        version: 1,
        transaction: Uuid::new_v4(),
        source_before: source.clone(),
        source_after,
        destination_before: destination.clone(),
        destination_after,
        playlist: id,
        source_json: loaded.original_json.clone(),
        source_m3u: loaded.original_m3u.clone(),
        destination_json: serde_json::to_vec_pretty(&manifest)?,
        destination_m3u: render_m3u(&manifest)?,
    };
    write_atomic(&path, &serde_json::to_vec(&intent)?)?;
    finish_playlist_move(paths, &intent)?;
    Ok((intent.source_after, intent.destination_after, dst.load()?))
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
