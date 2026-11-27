//! Named playlist workspaces. The active draft is independent of the last
//! explicitly saved (or autosaved) snapshot of each workspace.

use super::collections::storage::{LoadedPlaylist, PlaylistStore, sync_directory, write_atomic};
use super::collections::{
    CollectionId, CollectionIndex, CollectionManifest, CollectionPaths, PlaylistEntry, PlaylistId, PlaylistItemId,
    PlaylistManifest,
};
use crate::engine::MediaSource;
use crate::settings::config_dir;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub sources: Vec<MediaSource>,
    pub ids: Vec<PlaylistItemId>,
    pub custom_titles: BTreeMap<usize, String>,
    pub current: Option<usize>,
    pub position: Option<f64>,
}

impl Snapshot {
    fn normalize(&mut self) {
        let len = self.sources.len();
        debug_assert_eq!(self.ids.len(), len);
        self.custom_titles.retain(|i, title| {
            let trimmed = title.trim();
            if *i >= len || trimmed.is_empty() {
                return false;
            }
            if trimmed.len() != title.len() {
                *title = trimmed.to_owned();
            }
            true
        });
        self.current = self.current.filter(|&i| i < self.sources.len());
        self.position = self.position.filter(|p| self.current.is_some() && p.is_finite() && *p >= 0.0);
    }

    fn needs_autosave(&self, saved: &Self) -> bool {
        self.ids != saved.ids
            || self.sources != saved.sources
            || self.custom_titles != saved.custom_titles
            || self.current != saved.current
            || match (self.position, saved.position) {
                (Some(a), Some(b)) => (a - b).abs() >= 5.0,
                (a, b) => a != b,
            }
    }
}

pub(crate) struct Slot {
    pub name: String,
    pub autosave: bool,
    saved: Snapshot,
    id: PlaylistId,
    stored: Option<LoadedPlaylist>,
    dirty: bool,
    draft: Option<Snapshot>,
}

impl Slot {
    fn new(name: String, autosave: bool) -> Self {
        Self {
            name,
            autosave,
            saved: Snapshot::default(),
            id: PlaylistId::new(),
            stored: None,
            dirty: true,
            draft: None,
        }
    }

    fn snapshot(&self) -> &Snapshot {
        self.draft.as_ref().unwrap_or(&self.saved)
    }
}

struct CollectionStorage {
    paths: CollectionPaths,
    collection: CollectionManifest,
    _lock: File,
}

/// Explicit collection launches do not participate in the single-window IPC.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum CollectionRequest {
    #[default]
    Default,
    Temporary,
    Writable(CollectionId),
    ReadOnly(CollectionId),
    Recover(CollectionId),
}

impl CollectionRequest {
    pub fn is_explicit(self) -> bool {
        !matches!(self, Self::Default)
    }
}

pub(crate) struct Library {
    storage: Option<CollectionStorage>,
    collection_label: String,
    temporary: bool,
    pub active: usize,
    pub slots: Vec<Slot>,
    pub writable: bool,
    dirty: bool,
}

impl Library {
    fn empty(writable: bool) -> Self {
        Self {
            storage: None,
            collection_label: "Temporary collection (memory only)".into(),
            temporary: false,
            active: 0,
            slots: vec![Slot::new("Default playlist".into(), false)],
            writable,
            dirty: false,
        }
    }

    pub fn load(request: CollectionRequest) -> Self {
        let config = config_dir();
        let result = match request {
            CollectionRequest::Default => Self::open(&config),
            CollectionRequest::Temporary => Self::new_session(&config),
            CollectionRequest::Recover(id) => Self::recover_session(&config, id),
            CollectionRequest::Writable(id) => Self::open_selected(&config, Some(id), false),
            CollectionRequest::ReadOnly(id) => Self::open_selected(&config, Some(id), true),
        };
        match result {
            Ok(library) => library,
            Err(err) => {
                eprintln!("Could not open collection; using a temporary collection: {err}");
                if matches!(request, CollectionRequest::Recover(_)) {
                    return Self::empty(false);
                }
                Self::new_session(&config).unwrap_or_else(|err| {
                    eprintln!("Could not create temporary session: {err}");
                    Self::empty(false)
                })
            }
        }
    }

    fn open_index(config: &Path, initialize: bool) -> io::Result<(CollectionPaths, CollectionIndex, File)> {
        let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message);
        fs::create_dir_all(config)?;
        let paths = CollectionPaths::new(config);
        // Never hold the global index lock while waiting for a collection owner.
        // This short critical section serializes index reads and mutations.
        let index_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(config.join("collections.lock"))?;
        index_lock.lock()?;
        if !paths.index().try_exists()? {
            if !initialize {
                return Err(io::Error::new(io::ErrorKind::NotFound, "Collection index does not exist"));
            }
            if config.join("collections-migration/collections").try_exists()?
                || config.join("collections").try_exists()?
            {
                return Err(invalid("Collection storage is not activated or is incomplete"));
            }
            // Stage the entire first-run collection before publishing it.
            let stage = config.join(format!(".collections-init-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&stage)?;
            let result = (|| -> io::Result<()> {
                let staged = CollectionPaths::new(&stage);
                let mut collection = CollectionManifest::new("Default collection".into());
                let playlist = PlaylistManifest::new("Default playlist".into());
                collection.playlist_order.push(playlist.id);
                collection.active_playlist = Some(playlist.id);
                PlaylistStore::new(&staged, collection.id, playlist.id).create(playlist)?;
                write_atomic(&staged.collection_manifest(collection.id), &serde_json::to_vec_pretty(&collection)?)?;
                let mut index = CollectionIndex::new();
                index.collection_order.push(collection.id);
                index.default_collection = Some(collection.id);
                write_atomic(&staged.index(), &serde_json::to_vec_pretty(&index)?)?;
                fs::rename(stage.join("collections"), config.join("collections"))?;
                sync_directory(config)
            })();
            let _ = fs::remove_dir_all(&stage);
            result?;
        }
        let index: CollectionIndex = serde_json::from_slice(&fs::read(paths.index())?)?;
        index.validate().map_err(invalid)?;
        Ok((paths, index, index_lock))
    }

    fn open(config: &Path) -> io::Result<Self> {
        Self::open_selected(config, None, false)
    }

    fn open_selected(config: &Path, requested: Option<CollectionId>, read_only: bool) -> io::Result<Self> {
        let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message);
        let (paths, index, index_lock) = Self::open_index(config, requested.is_none() && !read_only)?;
        let id = requested.or(index.default_collection).ok_or_else(|| invalid("Missing default collection"))?;
        if !index.collection_order.contains(&id) {
            return Err(invalid("Collection is not in the index"));
        }
        let lock = if read_only {
            None
        } else {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(paths.collection_dir(id).join("collection.lock"))?;
            lock.try_lock().map_err(|err| io::Error::other(format!("Collection {id} lock unavailable: {err}")))?;
            Some(lock)
        };
        drop(index_lock);
        Self::open_storage(paths, id, lock, read_only, false)
    }

    fn open_storage(
        paths: CollectionPaths, id: CollectionId, lock: Option<File>, read_only: bool, temporary: bool,
    ) -> io::Result<Self> {
        let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message);
        let manifest_path = paths.collection_manifest(id);
        let manifest_bytes = fs::read(&manifest_path)?;
        let collection: CollectionManifest = serde_json::from_slice(&manifest_bytes)?;
        collection.validate().map_err(invalid)?;
        if collection.id != id || collection.playlist_order.is_empty() {
            return Err(invalid("Invalid collection manifest"));
        }
        let active = collection
            .active_playlist
            .and_then(|id| collection.playlist_order.iter().position(|p| *p == id))
            .unwrap_or(0);
        let mut slots = Vec::new();
        for &id in &collection.playlist_order {
            let store = PlaylistStore::new(&paths, collection.id, id);
            if !read_only {
                store.recover()?;
            }
            let loaded = store.load()?;
            let manifest = &loaded.manifest;
            let saved = Snapshot {
                sources: manifest.entries.iter().map(|entry| entry.source.clone()).collect(),
                ids: manifest.entries.iter().map(|entry| entry.id).collect(),
                custom_titles: manifest
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(i, entry)| entry.custom_title.clone().map(|title| (i, title)))
                    .collect(),
                current: manifest.current.and_then(|id| manifest.entries.iter().position(|entry| entry.id == id)),
                position: manifest.position,
            };
            slots.push(Slot {
                name: manifest.name.clone(),
                autosave: !read_only && manifest.autosave,
                saved,
                id,
                dirty: loaded.externally_edited,
                stored: Some(loaded),
                draft: None,
            });
        }
        // A reader cannot recover pending transactions or acquire the writer's
        // lock. Each pair is checked by PlaylistStore::load; reject a manifest
        // change during loading rather than presenting a partially edited list.
        if read_only && fs::read(&manifest_path)? != manifest_bytes {
            return Err(io::Error::other("Collection changed while loading; retry"));
        }
        let dirty = !read_only && slots.iter().any(|slot| slot.dirty);
        let collection_label = if read_only { format!("Read-only: {}", collection.name) } else { collection.name.clone() };
        Ok(Self {
            storage: lock.map(|lock| CollectionStorage { paths, collection, _lock: lock }),
            collection_label,
            temporary,
            active,
            slots,
            writable: !read_only,
            dirty,
        })
    }

    fn new_session(config: &Path) -> io::Result<Self> {
        let paths = CollectionPaths::sessions(config);
        let mut collection = CollectionManifest::new("Temporary collection".into());
        let id = collection.id;
        let dir = paths.collection_dir(id);
        fs::create_dir_all(dir.parent().ok_or_else(|| io::Error::other("Missing sessions directory"))?)?;
        fs::create_dir(&dir)?;
        let lock = Self::session_lock(&paths, id)?;
        let playlist = PlaylistManifest::new("Default playlist".into());
        collection.playlist_order.push(playlist.id);
        collection.active_playlist = Some(playlist.id);
        PlaylistStore::new(&paths, id, playlist.id).create(playlist)?;
        write_atomic(&paths.collection_manifest(id), &serde_json::to_vec_pretty(&collection)?)?;
        sync_directory(&dir)?;
        Self::open_storage(paths, id, Some(lock), false, true)
    }

    fn session_lock(paths: &CollectionPaths, id: CollectionId) -> io::Result<File> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(paths.collection_dir(id).join("collection.lock"))?;
        lock.try_lock().map_err(|err| io::Error::other(format!("Session {id} lock unavailable: {err}")))?;
        Ok(lock)
    }

    fn recover_session(config: &Path, id: CollectionId) -> io::Result<Self> {
        let paths = CollectionPaths::sessions(config);
        let lock = Self::session_lock(&paths, id)?;
        if paths.collection_dir(id).join("closed").try_exists()? {
            return Err(io::Error::other("Session was closed normally"));
        }
        Self::open_storage(paths, id, Some(lock), false, true)
    }

    pub fn list_sessions(config: &Path) -> io::Result<String> {
        let paths = CollectionPaths::sessions(config);
        let entries = match fs::read_dir(config.join("sessions")) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
            Err(err) => return Err(err),
        };
        let mut lines = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Ok(id) = entry.file_name().to_string_lossy().parse::<CollectionId>() else { continue };
            let Ok(_lock) = Self::session_lock(&paths, id) else { continue };
            if !entry.path().join("closed").try_exists()? && Self::read_collection(&paths, id).is_ok() {
                lines.push(id.to_string());
            }
        }
        lines.sort();
        Ok(lines.join("\n"))
    }

    pub fn is_temporary(&self) -> bool {
        self.temporary
    }

    /// Mark a normally closed session only after its last checkpoint succeeds.
    pub fn close_temporary(&mut self) -> io::Result<()> {
        if !self.temporary {
            return Ok(());
        }
        self.flush()?;
        let Some(storage) = &self.storage else { return Ok(()) };
        let dir = storage.paths.collection_dir(storage.collection.id);
        write_atomic(&dir.join("closed"), b"closed\n")?;
        // Release the OS lock before deletion (required on Windows). The marker
        // prevents a racing recovery from opening a normally closed session.
        self.storage = None;
        self.writable = false;
        self.temporary = false;
        let discarded = dir.with_file_name(format!(".closed-{}", uuid::Uuid::new_v4()));
        fs::rename(&dir, &discarded)?;
        fs::remove_dir_all(discarded)
    }

    pub fn collection_label(&self) -> &str {
        &self.collection_label
    }

    pub fn collection_id(&self) -> Option<CollectionId> {
        self.storage.as_ref().filter(|_| !self.temporary).map(|s| s.collection.id)
    }

    pub fn collection_choices(config: &Path) -> io::Result<Vec<(CollectionId, String)>> {
        let (paths, index, _lock) = Self::open_index(config, false)?;
        index.collection_order.into_iter().map(|id| {
            Ok((id, Self::read_collection(&paths, id)?.name))
        }).collect()
    }

    pub fn open_for_switch(id: CollectionId) -> io::Result<Self> {
        Self::open_selected(&config_dir(), Some(id), false)
    }

    /// Do not silently discard unsaved drafts or a recoverable temporary session.
    pub fn can_leave(&self) -> bool {
        if self.temporary {
            return self.slots.len() == 1 && self.slots[0].name == "Default playlist"
                && self.slots[0].snapshot().sources.is_empty();
        }
        self.slots.iter().all(|slot| {
            let draft = slot.snapshot();
            draft.sources == slot.saved.sources && draft.ids == slot.saved.ids
                && draft.custom_titles == slot.saved.custom_titles
        })
    }

    /// Administrative entry points also used by the collection UI.
    pub fn list_collections(config: &Path) -> io::Result<String> {
        let (paths, index, _index_lock) = Self::open_index(config, false)?;
        let mut lines = Vec::new();
        for id in index.collection_order {
            let collection = Self::read_collection(&paths, id)?;
            let suffix = if index.default_collection == Some(id) { " (default)" } else { "" };
            lines.push(format!("{id}  {}{suffix}", collection.name));
        }
        Ok(lines.join("\n"))
    }

    fn read_collection(paths: &CollectionPaths, id: CollectionId) -> io::Result<CollectionManifest> {
        let collection: CollectionManifest = serde_json::from_slice(&fs::read(paths.collection_manifest(id))?)?;
        collection.validate().map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if collection.id != id {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Collection ID mismatch"));
        }
        Ok(collection)
    }

    pub fn create_collection(config: &Path, name: &str) -> io::Result<CollectionId> {
        let name = name.trim();
        if name.is_empty() || name.contains(['\r', '\n', '\0']) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid collection name"));
        }
        let (paths, mut index, _index_lock) = Self::open_index(config, true)?;
        for &id in &index.collection_order {
            if Self::read_collection(&paths, id)?.name.eq_ignore_ascii_case(name) {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Collection name already exists"));
            }
        }
        let mut collection = CollectionManifest::new(name.into());
        let playlist = PlaylistManifest::new("Default playlist".into());
        collection.playlist_order.push(playlist.id);
        collection.active_playlist = Some(playlist.id);
        let stage = config.join(format!(".collections-create-{}", collection.id));
        fs::create_dir(&stage)?;
        let result = (|| -> io::Result<()> {
            let staged = CollectionPaths::new(&stage);
            PlaylistStore::new(&staged, collection.id, playlist.id).create(playlist)?;
            write_atomic(&staged.collection_manifest(collection.id), &serde_json::to_vec_pretty(&collection)?)?;
            sync_directory(&staged.collection_dir(collection.id))?;
            // Publish a complete collection before adding its ID to the index.
            // On a failed index commit, leave an unindexed directory for recovery.
            fs::rename(staged.collection_dir(collection.id), paths.collection_dir(collection.id))?;
            sync_directory(paths.index().parent().ok_or_else(|| io::Error::other("Missing index directory"))?)?;
            index.collection_order.push(collection.id);
            if index.default_collection.is_none() {
                index.default_collection = Some(collection.id);
            }
            write_atomic(&paths.index(), &serde_json::to_vec_pretty(&index)?)
        })();
        let _ = fs::remove_dir_all(&stage);
        result?;
        Ok(collection.id)
    }

    pub fn snapshot(&self) -> &Snapshot {
        self.slots[self.active].snapshot()
    }
    pub fn name(&self) -> &str {
        &self.slots[self.active].name
    }
    pub fn autosave(&self) -> bool {
        self.slots[self.active].autosave
    }

    pub fn capture(&mut self, mut snapshot: Snapshot, exact: bool, manual: bool) {
        snapshot.normalize();
        let slot = &mut self.slots[self.active];
        if (manual || ((self.temporary || slot.autosave) && (exact || snapshot.needs_autosave(&slot.saved))))
            && slot.saved != snapshot
        {
            slot.saved = snapshot.clone();
            slot.dirty = true;
            self.dirty = true;
        }
        if manual && slot.stored.is_none() {
            self.dirty = true;
        }
        slot.draft = Some(snapshot);
    }

    pub fn set_autosave(&mut self, enabled: bool) {
        if self.slots[self.active].autosave != enabled {
            self.slots[self.active].autosave = enabled;
            self.slots[self.active].dirty = true;
            self.dirty = true;
        }
    }

    pub fn switch(&mut self, index: usize) -> bool {
        if index == self.active || index >= self.slots.len() {
            return false;
        }
        self.active = index;
        self.dirty = true;
        true
    }

    pub fn can_delete(&self) -> bool {
        self.writable && self.slots.len() > 1
    }

    pub fn delete_active(&mut self) -> bool {
        if !self.can_delete() {
            return false;
        }
        self.slots.remove(self.active);
        self.active = self.active.min(self.slots.len() - 1);
        self.dirty = true;
        true
    }

    pub fn valid_name(&self, name: &str, rename: bool) -> bool {
        !name.trim().is_empty()
            && !self
                .slots
                .iter()
                .enumerate()
                .any(|(i, s)| (!rename || i != self.active) && s.name.eq_ignore_ascii_case(name.trim()))
    }

    pub fn suggested_name(&self, base: &str) -> String {
        if self.valid_name(base, false) {
            return base.into();
        }
        (2..).map(|i| format!("{base} {i}")).find(|name| self.valid_name(name, false)).unwrap()
    }

    pub fn create(&mut self, name: String) {
        self.slots.push(Slot::new(name, self.writable));
        self.active = self.slots.len() - 1;
        self.dirty = true;
    }

    pub fn duplicate(&mut self, name: String, mut snapshot: Snapshot) -> bool {
        if !self.writable || !self.valid_name(&name, false) {
            return false;
        }
        snapshot.normalize();
        let mut slot = Slot::new(name, true);
        snapshot.ids = snapshot.sources.iter().map(|_| PlaylistItemId::new()).collect();
        slot.saved = snapshot;
        self.slots.push(slot);
        self.active = self.slots.len() - 1;
        self.dirty = true;
        true
    }

    pub fn rename(&mut self, name: String) {
        self.slots[self.active].name = name;
        self.slots[self.active].dirty = true;
        self.dirty = true;
    }

    /// Explicitly launched media must not replace an already saved playlist.
    /// This temporary workspace is not written unless the user saves or edits
    /// the library (e.g. switches to another playlist or enables autosave).
    pub fn start_launch_playlist(&mut self) {
        if self.snapshot().sources.is_empty() {
            return;
        }
        let name = self.suggested_name("Launch playlist");
        self.slots.push(Slot::new(name, false));
        self.active = self.slots.len() - 1;
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        if !self.writable || !self.dirty {
            return Ok(());
        }
        let storage = self.storage.as_mut().ok_or_else(|| io::Error::other("Missing collection storage"))?;
        for slot in self.slots.iter_mut().filter(|slot| slot.dirty) {
            if slot.saved.sources.len() != slot.saved.ids.len() {
                return Err(io::Error::other("Playlist source and ID counts differ"));
            }
            let entries = slot
                .saved
                .sources
                .iter()
                .zip(&slot.saved.ids)
                .enumerate()
                .map(|(i, (source, id))| PlaylistEntry {
                    id: *id,
                    source: source.clone(),
                    custom_title: slot.saved.custom_titles.get(&i).cloned(),
                })
                .collect();
            let manifest = PlaylistManifest {
                version: super::collections::FORMAT_VERSION,
                id: slot.id,
                name: slot.name.clone(),
                autosave: slot.autosave,
                revision: slot.stored.as_ref().map_or(0, |loaded| loaded.manifest.revision),
                entries,
                current: slot.saved.current.and_then(|i| slot.saved.ids.get(i).copied()),
                position: slot.saved.position,
            };
            let store = PlaylistStore::new(&storage.paths, storage.collection.id, slot.id);
            if let Some(loaded) = &mut slot.stored {
                loaded.manifest = manifest;
                store.save(loaded)?;
            } else {
                slot.stored = Some(store.create(manifest)?);
            }
            slot.dirty = false;
        }
        let mut collection = storage.collection.clone();
        collection.playlist_order = self.slots.iter().map(|slot| slot.id).collect();
        collection.active_playlist = Some(self.slots[self.active].id);
        if collection != storage.collection {
            write_atomic(&storage.paths.collection_manifest(collection.id), &serde_json::to_vec_pretty(&collection)?)?;
            storage.collection = collection;
        }
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &str, position: f64) -> Snapshot {
        Snapshot {
            sources: vec![MediaSource::File(std::env::temp_dir().join(name))],
            ids: vec![PlaylistItemId::new()],
            current: Some(0),
            position: Some(position),
            ..Default::default()
        }
    }

    #[test]
    fn temporary_session_recovers_even_with_autosave_disabled() {
        let root = std::env::temp_dir().join(format!("mavplayer-session-{}", uuid::Uuid::new_v4()));
        let mut session = Library::new_session(&root).unwrap();
        let id = session.storage.as_ref().unwrap().collection.id;
        assert!(session.is_temporary());
        assert!(!session.autosave());
        let first = snapshot("first.mp4", 12.5);
        session.capture(first.clone(), false, false);
        session.flush().unwrap();
        session.create("Second".into());
        let second = snapshot("second.mp4", 25.0);
        session.capture(second.clone(), false, false);
        session.flush().unwrap();
        assert!(Library::list_sessions(&root).unwrap().is_empty());
        assert!(Library::recover_session(&root, id).is_err());
        // Dropping without a normal close models an interrupted process.
        drop(session);
        assert_eq!(Library::list_sessions(&root).unwrap(), id.to_string());
        let mut session = Library::recover_session(&root, id).unwrap();
        assert_eq!(session.snapshot(), &second);
        assert!(session.switch(0));
        assert_eq!(session.snapshot(), &first);
        session.close_temporary().unwrap();
        assert!(Library::list_sessions(&root).unwrap().is_empty());
        assert!(Library::recover_session(&root, id).is_err());
        assert!(!root.join("collections").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collection_switch_preserves_unsaved_drafts() {
        let root = std::env::temp_dir().join(format!("mavplayer-switch-{}", uuid::Uuid::new_v4()));
        let mut library = Library::open(&root).unwrap();
        let id = library.collection_id().unwrap();
        assert_eq!(Library::collection_choices(&root).unwrap()[0].0, id);
        assert!(Library::open_selected(&root, Some(id), false).is_err());
        library.set_autosave(false);
        let draft = snapshot("draft.mp4", 0.0);
        library.capture(draft.clone(), true, false);
        assert!(!library.can_leave());
        library.capture(draft, true, true);
        library.flush().unwrap();
        assert!(library.can_leave());
        drop(library);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn different_collections_can_be_opened_and_saved_independently() {
        let root = std::env::temp_dir().join(format!("mavplayer-session-{}", uuid::Uuid::new_v4()));
        let mut first = Library::open(&root).unwrap();
        let second_id = Library::create_collection(&root, "Second collection").unwrap();
        assert!(Library::create_collection(&root, "second collection").is_err());
        let mut second = Library::open_selected(&root, Some(second_id), false).unwrap();
        assert_eq!(second.collection_label(), "Second collection");
        assert!(Library::open(&root).is_err());
        assert!(Library::open_selected(&root, Some(second_id), false).is_err());
        let a = snapshot("first.mp4", 12.5);
        let b = snapshot("second.mp4", 25.0);
        first.capture(a.clone(), true, true);
        second.capture(b.clone(), true, true);
        first.flush().unwrap();
        second.flush().unwrap();
        assert!(Library::list_collections(&root).unwrap().contains("Second collection"));
        drop(first);
        drop(second);
        let first = Library::open(&root).unwrap();
        let second = Library::open_selected(&root, Some(second_id), false).unwrap();
        assert_eq!(first.snapshot(), &a);
        assert_eq!(second.snapshot(), &b);
        drop(first);
        drop(second);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_only_snapshot_never_writes_or_recovers_a_locked_collection() {
        let root = std::env::temp_dir().join(format!("mavplayer-session-{}", uuid::Uuid::new_v4()));
        let mut writer = Library::open(&root).unwrap();
        let saved = snapshot("saved.mp4", 10.0);
        writer.capture(saved.clone(), true, true);
        writer.flush().unwrap();
        let storage = writer.storage.as_ref().unwrap();
        let id = storage.collection.id;
        let json = storage.paths.playlist_manifest(id, writer.slots[0].id);
        let before = fs::read(&json).unwrap();
        let mut reader = Library::open_selected(&root, Some(id), true).unwrap();
        assert!(!reader.writable);
        assert!(reader.collection_label().starts_with("Read-only: "));
        assert!(!reader.autosave());
        assert_eq!(reader.snapshot(), &saved);
        reader.capture(snapshot("local.mp4", 20.0), true, true);
        reader.rename("Local name".into());
        reader.flush().unwrap();
        assert_eq!(fs::read(&json).unwrap(), before);
        let pending = json.with_extension("pending.json");
        fs::write(&pending, b"unfinished transaction").unwrap();
        assert!(Library::open_selected(&root, Some(id), true).is_err());
        assert_eq!(fs::read(&pending).unwrap(), b"unfinished transaction");
        drop(reader);
        drop(writer);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collection_roundtrip_preserves_ids_and_independent_drafts() {
        let root = std::env::temp_dir().join(format!("mavplayer-session-{}", uuid::Uuid::new_v4()));
        let mut lib = Library::open(&root).unwrap();
        let first = snapshot("first.mp4", 12.5);
        lib.capture(first.clone(), true, true);
        lib.create("Second".into());
        let second = snapshot("first.mp4", 70.0);
        lib.capture(second.clone(), true, true);
        lib.set_autosave(false);
        let draft = snapshot("unsaved.mp4", 23.0);
        lib.capture(draft.clone(), true, false);
        assert!(lib.switch(0));
        assert_eq!(lib.snapshot(), &first);
        assert!(lib.switch(1));
        assert_eq!(lib.snapshot(), &draft);
        lib.flush().unwrap();
        assert!(Library::open(&root).is_err());
        drop(lib);
        let mut restored = Library::open(&root).unwrap();
        assert_eq!(restored.snapshot(), &second);
        assert!(restored.switch(0));
        assert_eq!(restored.snapshot(), &first);
        assert!(restored.duplicate("Copy".into(), first.clone()));
        assert_ne!(restored.snapshot().ids, first.ids);
        assert_eq!(restored.snapshot().sources, first.sources);
        restored.flush().unwrap();
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_occurrences_keep_their_ids_on_reorder() {
        let root = std::env::temp_dir().join(format!("mavplayer-session-{}", uuid::Uuid::new_v4()));
        let mut lib = Library::open(&root).unwrap();
        let mut saved = snapshot("movie.mp4", 10.0);
        saved.sources.push(saved.sources[0].clone());
        saved.ids.push(PlaylistItemId::new());
        lib.capture(saved.clone(), true, true);
        lib.flush().unwrap();
        saved.custom_titles.insert(0, "First".into());
        saved.custom_titles.insert(1, "Second".into());
        lib.capture(saved.clone(), true, true);
        lib.flush().unwrap();
        saved.ids.swap(0, 1);
        saved.custom_titles.insert(0, "Second".into());
        saved.custom_titles.insert(1, "First".into());
        saved.current = Some(1);
        lib.capture(saved.clone(), true, true);
        lib.flush().unwrap();
        drop(lib);
        let restored = Library::open(&root).unwrap();
        assert_eq!(restored.snapshot(), &saved);
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deleted_playlist_is_not_reopened() {
        let root = std::env::temp_dir().join(format!("mavplayer-session-{}", uuid::Uuid::new_v4()));
        let mut lib = Library::open(&root).unwrap();
        let first = snapshot("first.mp4", 10.0);
        lib.capture(first.clone(), true, true);
        lib.create("Second".into());
        lib.capture(snapshot("second.mp4", 20.0), true, true);
        lib.flush().unwrap();
        assert!(lib.delete_active());
        lib.flush().unwrap();
        drop(lib);
        let restored = Library::open(&root).unwrap();
        assert_eq!(restored.slots.len(), 1);
        assert_eq!(restored.snapshot(), &first);
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_change_keeps_failed_save_dirty() {
        let root = std::env::temp_dir().join(format!("mavplayer-session-{}", uuid::Uuid::new_v4()));
        let mut lib = Library::open(&root).unwrap();
        let saved = snapshot("first.mp4", 10.0);
        lib.capture(saved.clone(), true, true);
        lib.flush().unwrap();
        let storage = lib.storage.as_ref().unwrap();
        let path = storage.paths.playlist_m3u8(storage.collection.id, lib.slots[0].id);
        let original = fs::read(&path).unwrap();
        fs::write(&path, b"#EXTM3U\n# external edit\n").unwrap();
        lib.rename("Renamed".into());
        assert!(lib.flush().is_err());
        assert!(lib.dirty && lib.slots[0].dirty);
        assert!(lib.flush().is_err());
        fs::write(&path, original).unwrap();
        lib.flush().unwrap();
        drop(lib);
        let restored = Library::open(&root).unwrap();
        assert_eq!(restored.name(), "Renamed");
        assert_eq!(restored.snapshot(), &saved);
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launch_playlist_does_not_replace_saved_playlist() {
        let root = std::env::temp_dir().join(format!("mavplayer-session-{}", uuid::Uuid::new_v4()));
        let mut lib = Library::open(&root).unwrap();
        let saved = snapshot("saved.mp4", 10.0);
        lib.capture(saved.clone(), true, true);
        lib.flush().unwrap();
        lib.start_launch_playlist();
        lib.capture(snapshot("launch.mp4", 0.0), true, false);
        lib.flush().unwrap();
        drop(lib);
        let restored = Library::open(&root).unwrap();
        assert_eq!(restored.slots.len(), 1);
        assert_eq!(restored.snapshot(), &saved);
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }
}
