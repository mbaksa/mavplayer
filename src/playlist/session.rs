//! Named playlist workspaces. The active draft is independent of the last
//! explicitly saved (or autosaved) snapshot of each workspace.

use super::collections::storage::{
    LoadedPlaylist, PlaylistStore, move_items_transaction, move_playlist_transaction, recover_move,
    recover_playlist_move, sync_directory, write_atomic,
};
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

    fn from_manifest(manifest: &PlaylistManifest) -> Self {
        Self {
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
        }
    }

    fn same_contents(&self, other: &Self) -> bool {
        self.ids == other.ids && self.sources == other.sources && self.custom_titles == other.custom_titles
    }

    fn append_entries(&mut self, entries: &[PlaylistEntry]) {
        for entry in entries {
            let index = self.sources.len();
            self.sources.push(entry.source.clone());
            self.ids.push(entry.id);
            if let Some(title) = &entry.custom_title {
                self.custom_titles.insert(index, title.clone());
            }
        }
    }

    fn remove_entries(&mut self, ids: &[PlaylistItemId]) {
        let removed: std::collections::HashSet<_> = ids.iter().copied().collect();
        let current = self.current.and_then(|i| self.ids.get(i)).copied();
        let mut next = Self::default();
        for (i, (source, id)) in self.sources.iter().zip(&self.ids).enumerate() {
            if removed.contains(id) {
                continue;
            }
            let index = next.sources.len();
            next.sources.push(source.clone());
            next.ids.push(*id);
            if let Some(title) = self.custom_titles.get(&i) {
                next.custom_titles.insert(index, title.clone());
            }
            if current == Some(*id) {
                next.current = Some(index);
                next.position = self.position;
            }
        }
        *self = next;
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
    watch: Option<super::collections::watch::TransferWatch>,
    retry_transfers: bool,
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

#[derive(Default)]
pub(crate) struct MoveProgress {
    pub received: usize,
    pub finished: usize,
    pub active_added: Vec<PlaylistEntry>,
    pub active_removed: Vec<PlaylistItemId>,
    pub retry: bool,
    pub notice: Option<String>,
}

pub(crate) struct Library {
    storage: Option<CollectionStorage>,
    collection_label: String,
    temporary: bool,
    pub keep_session: bool,
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
            keep_session: false,
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
            match lock.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    let name = Self::read_collection(&paths, id)?.name;
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("Collection \"{name}\" is already open in another window."),
                    ));
                }
                Err(std::fs::TryLockError::Error(err)) => return Err(err),
            }
            Some(lock)
        };
        drop(index_lock);
        Self::open_storage(paths, id, lock, read_only, false)
    }

    fn open_storage(
        paths: CollectionPaths,
        id: CollectionId,
        lock: Option<File>,
        read_only: bool,
        temporary: bool,
    ) -> io::Result<Self> {
        let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message);
        if !temporary {
            recover_playlist_move(&paths, id, read_only)?;
        }
        recover_move(&paths, id, read_only)?;
        let manifest_path = paths.collection_manifest(id);
        let manifest_bytes = fs::read(&manifest_path)?;
        let mut collection: CollectionManifest = serde_json::from_slice(&manifest_bytes)?;
        collection.validate().map_err(invalid)?;
        if collection.id != id || collection.playlist_order.is_empty() {
            return Err(invalid("Invalid collection manifest"));
        }
        if !read_only && !temporary {
            // Recover existing playlist pairs before resolving imported names.
            for &playlist in &collection.playlist_order {
                PlaylistStore::new(&paths, id, playlist).recover()?;
            }
            super::collections::inbox::import_pending(&paths, &mut collection)?;
            super::collections::inbox::import_items(&paths, &collection, |_| true, |_| {})?;
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
        let collection_label =
            if read_only { format!("Read-only: {}", collection.name) } else { collection.name.clone() };
        Ok(Self {
            storage: lock.map(|lock| CollectionStorage {
                paths,
                collection,
                watch: None,
                retry_transfers: false,
                _lock: lock,
            }),
            collection_label,
            temporary,
            keep_session: false,
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
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "This session is already open in another window"));
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(err),
        }
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

    pub fn temporary_for_switch() -> io::Result<Self> {
        Self::new_session(&config_dir())
    }

    pub fn recover_for_switch(id: CollectionId) -> io::Result<Self> {
        Self::recover_session(&config_dir(), id)
    }

    pub fn session_choices(config: &Path) -> io::Result<Vec<(CollectionId, String)>> {
        let paths = CollectionPaths::sessions(config);
        Ok(Self::list_sessions(config)?
            .lines()
            .filter_map(|line| {
                let id = line.parse::<CollectionId>().ok()?;
                let collection = Self::read_collection(&paths, id).ok()?;
                Some((id, format!("{} · {} playlists · {}", collection.name, collection.playlist_order.len(), id)))
            })
            .collect())
    }

    /// Keep a checkpoint on disk if requested, otherwise discard on normal exit.
    pub fn finish_temporary(&mut self) -> io::Result<()> {
        if self.temporary && self.keep_session { self.flush() } else { self.close_temporary() }
    }

    /// Publish a complete copy before retiring the original recovery session.
    /// Keep playlist/item identities and leave the session intact on failure.
    pub fn promote_session(&mut self, config: &Path, name: &str) -> io::Result<()> {
        if !self.temporary {
            return Err(io::Error::other("No temporary session to save"));
        }
        let name = name.trim();
        if name.is_empty() || name.contains(['\r', '\n', '\0']) {
            return Err(io::Error::other("Invalid collection name"));
        }
        self.flush()?;
        let old = self.storage.as_ref().ok_or_else(|| io::Error::other("Missing session storage"))?;
        let (paths, mut index, _index_lock) = Self::open_index(config, true)?;
        for &id in &index.collection_order {
            if Self::read_collection(&paths, id)?.name.eq_ignore_ascii_case(name) {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Collection name already exists"));
            }
        }
        let mut collection = old.collection.clone();
        collection.id = CollectionId::new();
        collection.name = name.to_owned();
        let id = collection.id;
        let stage = config.join(format!(".collections-promote-{id}"));
        fs::create_dir(&stage)?;
        let result = (|| -> io::Result<Self> {
            let staged = CollectionPaths::new(&stage);
            for slot in &self.slots {
                let manifest =
                    &slot.stored.as_ref().ok_or_else(|| io::Error::other("Missing saved playlist"))?.manifest;
                PlaylistStore::new(&staged, id, slot.id).create(manifest.clone())?;
            }
            write_atomic(&staged.collection_manifest(id), &serde_json::to_vec_pretty(&collection)?)?;
            sync_directory(&staged.collection_dir(id))?;
            fs::rename(staged.collection_dir(id), paths.collection_dir(id))?;
            let lock = Self::session_lock(&paths, id)?;
            let next = Self::open_storage(CollectionPaths::new(config), id, Some(lock), false, false)?;
            sync_directory(paths.index().parent().ok_or_else(|| io::Error::other("Missing index directory"))?)?;
            index.collection_order.push(id);
            if index.default_collection.is_none() {
                index.default_collection = Some(id);
            }
            write_atomic(&paths.index(), &serde_json::to_vec_pretty(&index)?)?;
            Ok(next)
        })();
        let _ = fs::remove_dir_all(&stage);
        let next = result?;
        if let Err(err) = self.close_temporary() {
            log::warn!("Collection saved; could not retire recovery session: {err}");
        }
        *self = next;
        Ok(())
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
        index.collection_order.into_iter().map(|id| Ok((id, Self::read_collection(&paths, id)?.name))).collect()
    }

    pub fn open_for_switch(id: CollectionId) -> io::Result<Self> {
        Self::open_selected(&config_dir(), Some(id), false)
    }

    /// Do not silently discard unsaved drafts or a recoverable temporary session.
    pub fn can_leave(&self) -> bool {
        if self.temporary {
            return self.keep_session
                || (self.slots.len() == 1
                    && self.slots[0].name == "Default playlist"
                    && self.slots[0].snapshot().sources.is_empty());
        }
        self.slots.iter().all(|slot| {
            let draft = slot.snapshot();
            draft.sources == slot.saved.sources
                && draft.ids == slot.saved.ids
                && draft.custom_titles == slot.saved.custom_titles
        })
    }

    pub fn rename_collection(&mut self, config: &Path, name: &str) -> io::Result<()> {
        let name = name.trim();
        if name.is_empty() || name.contains(['\r', '\n', '\0']) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid collection name"));
        }
        let id = self.collection_id().ok_or_else(|| io::Error::other("No writable permanent collection"))?;
        let (paths, index, _lock) = Self::open_index(config, false)?;
        for &other in &index.collection_order {
            if other != id && Self::read_collection(&paths, other)?.name.eq_ignore_ascii_case(name) {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Collection name already exists"));
            }
        }
        let storage = self.storage.as_mut().unwrap();
        let mut collection = storage.collection.clone();
        collection.name = name.to_owned();
        write_atomic(&paths.collection_manifest(id), &serde_json::to_vec_pretty(&collection)?)?;
        storage.collection = collection;
        self.collection_label = name.to_owned();
        Ok(())
    }

    pub fn edit_collection_index(&self, config: &Path, move_by: Option<isize>) -> io::Result<()> {
        let id = self.collection_id().ok_or_else(|| io::Error::other("No writable permanent collection"))?;
        let (paths, mut index, _lock) = Self::open_index(config, false)?;
        let pos = index
            .collection_order
            .iter()
            .position(|&other| other == id)
            .ok_or_else(|| io::Error::other("Collection is not in the index"))?;
        if let Some(delta) = move_by {
            let next = pos.saturating_add_signed(delta).min(index.collection_order.len() - 1);
            index.collection_order.swap(pos, next);
        } else {
            index.default_collection = Some(id);
        }
        write_atomic(&paths.index(), &serde_json::to_vec_pretty(&index)?)
    }

    pub fn collection_management_state(&self, config: &Path) -> io::Result<(bool, bool, bool, bool)> {
        let id = self.collection_id().ok_or_else(|| io::Error::other("No writable permanent collection"))?;
        let (_, index, _lock) = Self::open_index(config, false)?;
        let pos = index
            .collection_order
            .iter()
            .position(|&other| other == id)
            .ok_or_else(|| io::Error::other("Collection is not in the index"))?;
        Ok((
            pos > 0,
            pos + 1 < index.collection_order.len(),
            index.default_collection == Some(id),
            index.collection_order.len() > 1,
        ))
    }

    /// Acquire a replacement before removing the current collection from the index.
    /// Keep its files intact for later recovery; never delete media files.
    pub fn remove_collection(&self, config: &Path) -> io::Result<Self> {
        let id = self.collection_id().ok_or_else(|| io::Error::other("No writable permanent collection"))?;
        if !self.can_leave() {
            return Err(io::Error::other("Save unsaved playlists before deleting the collection"));
        }
        let (paths, mut index, _lock) = Self::open_index(config, false)?;
        let mut replacement = None;
        for other in index.collection_order.iter().copied().filter(|&other| other != id) {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(paths.collection_dir(other).join("collection.lock"))?;
            if lock.try_lock().is_err() {
                continue;
            }
            replacement =
                Some((other, Self::open_storage(CollectionPaths::new(config), other, Some(lock), false, false)?));
            break;
        }
        let (other, next) = replacement.ok_or_else(|| io::Error::other("No other writable collection is available"))?;
        index.collection_order.retain(|&entry| entry != id);
        if index.default_collection == Some(id) {
            index.default_collection = Some(other);
        }
        write_atomic(&paths.index(), &serde_json::to_vec_pretty(&index)?)?;
        Ok(next)
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

    /// Append independent copies to an inactive playlist without changing the
    /// active workspace or committing unrelated unsaved destination edits.
    pub fn copy_items(&mut self, target: usize, source: &Snapshot, indices: &[usize]) -> io::Result<usize> {
        if !self.writable || target == self.active || target >= self.slots.len() {
            return Err(io::Error::other("Destination playlist is not writable"));
        }
        if indices.is_empty() || indices.iter().any(|&i| i >= source.sources.len()) {
            return Err(io::Error::other("Invalid playlist selection"));
        }
        let slot = &self.slots[target];
        if slot.snapshot().sources != slot.saved.sources
            || slot.snapshot().ids != slot.saved.ids
            || slot.snapshot().custom_titles != slot.saved.custom_titles
        {
            return Err(io::Error::other("Save destination playlist edits before copying"));
        }
        // Finish earlier writes before changing the destination.
        self.flush()?;
        let slot = &mut self.slots[target];
        let copies: Vec<_> = indices
            .iter()
            .map(|&i| PlaylistEntry {
                id: PlaylistItemId::new(),
                source: source.sources[i].clone(),
                custom_title: source.custom_titles.get(&i).cloned(),
            })
            .collect();
        fn append(snapshot: &mut Snapshot, entries: &[PlaylistEntry]) {
            for entry in entries {
                let index = snapshot.sources.len();
                snapshot.sources.push(entry.source.clone());
                snapshot.ids.push(entry.id);
                if let Some(title) = &entry.custom_title {
                    snapshot.custom_titles.insert(index, title.clone());
                }
            }
        }
        append(&mut slot.saved, &copies);
        if let Some(draft) = &mut slot.draft {
            append(draft, &copies);
        }
        slot.dirty = true;
        self.dirty = true;
        self.flush()?;
        Ok(copies.len())
    }

    fn check_move_source(&self, playlist: PlaylistId, selected: Option<&[PlaylistItemId]>) -> io::Result<()> {
        let Some(storage) = &self.storage else { return Err(io::Error::other("The source collection is unavailable")) };
        use super::collections::transfer;
        if transfer::requests(&storage.paths, storage.collection.id, false)?
            .iter()
            .any(|request| request.snapshot.id == playlist)
        {
            return Err(io::Error::other("A move from this playlist is already waiting to finish"));
        }
        if transfer::unfinished_import(&storage.paths, storage.collection.id, playlist, selected)? {
            return Err(io::Error::other("These items were received in a previous move that has not finished yet"));
        }
        Ok(())
    }

    pub fn move_items(&mut self, target: usize, source: &Snapshot, indices: &[usize]) -> io::Result<usize> {
        if !self.writable || target == self.active || target >= self.slots.len() || indices.is_empty() {
            return Err(io::Error::other("Invalid move destination or selection"));
        }
        let same_contents =
            |a: &Snapshot, b: &Snapshot| a.sources == b.sources && a.ids == b.ids && a.custom_titles == b.custom_titles;
        if !same_contents(source, &self.slots[self.active].saved)
            || !same_contents(self.slots[target].snapshot(), &self.slots[target].saved)
        {
            return Err(io::Error::other("Save both playlist edits before moving"));
        }
        if indices.iter().any(|&i| i >= source.ids.len()) || indices.windows(2).any(|w| w[0] >= w[1]) {
            return Err(io::Error::other("The selected items have changed; select them again"));
        }
        let selected: Vec<_> = indices.iter().map(|&i| source.ids[i]).collect();
        self.check_move_source(self.slots[self.active].id, Some(&selected))?;
        self.flush()?;
        let storage = self.storage.as_ref().ok_or_else(|| io::Error::other("Missing collection storage"))?;
        let src =
            self.slots[self.active].stored.as_ref().ok_or_else(|| io::Error::other("Save source playlist first"))?;
        let dst =
            self.slots[target].stored.as_ref().ok_or_else(|| io::Error::other("Save destination playlist first"))?;
        let mut src_next = src.manifest.clone();
        let mut dst_next = dst.manifest.clone();
        let moved: std::collections::HashSet<_> = indices.iter().map(|&i| source.ids[i]).collect();
        if dst_next.entries.iter().any(|entry| moved.contains(&entry.id)) {
            return Err(io::Error::other("Destination already contains a moved item ID"));
        }
        dst_next.entries.extend(src_next.entries.iter().filter(|entry| moved.contains(&entry.id)).cloned());
        src_next.entries.retain(|entry| !moved.contains(&entry.id));
        src_next.current = source.current.and_then(|i| source.ids.get(i).copied()).filter(|id| !moved.contains(id));
        src_next.position = src_next.current.and(source.position);
        let result = move_items_transaction(&storage.paths, storage.collection.id, dst, src, dst_next, src_next);
        if let Err(err) = result {
            // Do not allow stale in-memory snapshots to overwrite a committed intent.
            self.writable = false;
            return Err(io::Error::other(format!("{err}; reopen collection before further writes")));
        }
        for index in [self.active, target] {
            let slot = &mut self.slots[index];
            let loaded = match PlaylistStore::new(&storage.paths, storage.collection.id, slot.id).load() {
                Ok(loaded) => loaded,
                Err(err) => {
                    self.writable = false;
                    return Err(err);
                }
            };
            let m = &loaded.manifest;
            slot.saved = Snapshot {
                sources: m.entries.iter().map(|e| e.source.clone()).collect(),
                ids: m.entries.iter().map(|e| e.id).collect(),
                custom_titles: m
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(i, e)| e.custom_title.clone().map(|t| (i, t)))
                    .collect(),
                current: m.current.and_then(|id| m.entries.iter().position(|e| e.id == id)),
                position: m.position,
            };
            slot.draft = None;
            slot.stored = Some(loaded);
        }
        Ok(indices.len())
    }

    pub fn move_playlist_to(&mut self, target: &mut Self, snapshot: &Snapshot) -> io::Result<String> {
        if !self.writable || !target.writable || self.temporary || target.temporary || self.slots.len() < 2 {
            return Err(io::Error::other(
                "Moving requires two writable permanent collections and another source playlist",
            ));
        }
        let saved = &self.slots[self.active].saved;
        if snapshot.sources != saved.sources
            || snapshot.ids != saved.ids
            || snapshot.custom_titles != saved.custom_titles
        {
            return Err(io::Error::other("Save source playlist edits before moving"));
        }
        self.check_move_source(self.slots[self.active].id, None)?;
        if let Some(destination) = &target.storage {
            if super::collections::transfer::pending_source_overlaps(
                &destination.paths,
                destination.collection.id,
                self.slots[self.active].id,
                &snapshot.ids,
            )? {
                return Err(io::Error::other(
                    "The destination collection is still finishing an earlier move involving this playlist",
                ));
            }
        }
        self.capture(snapshot.clone(), true, true);
        self.flush()?;
        target.flush()?;
        let source = self.storage.as_mut().ok_or_else(|| io::Error::other("Missing source storage"))?;
        let destination = target.storage.as_mut().ok_or_else(|| io::Error::other("Missing destination storage"))?;
        let slot = &self.slots[self.active];
        let loaded = slot.stored.as_ref().ok_or_else(|| io::Error::other("Save source playlist first"))?;
        // Reject collisions instead of silently merging identities.
        if target.slots.iter().any(|other| other.id == slot.id) {
            return Err(io::Error::other("Destination already contains this playlist ID"));
        }
        let name = if target.slots.iter().any(|other| other.name.eq_ignore_ascii_case(&slot.name)) {
            (2..)
                .map(|i| format!("{} {i}", slot.name))
                .find(|name| !target.slots.iter().any(|other| other.name.eq_ignore_ascii_case(name)))
                .unwrap()
        } else {
            slot.name.clone()
        };
        let result =
            move_playlist_transaction(&source.paths, &source.collection, &destination.collection, loaded, name.clone());
        let (source_next, destination_next, stored) = match result {
            Ok(result) => result,
            Err(err) => {
                self.writable = false;
                target.writable = false;
                return Err(io::Error::other(format!("{err}; reopen collections before further writes")));
            }
        };
        source.collection = source_next;
        destination.collection = destination_next;
        let mut slot = self.slots.remove(self.active);
        slot.name = name.clone();
        slot.stored = Some(stored);
        slot.draft = None;
        target.slots.push(slot);
        self.active = 0;
        Ok(name)
    }

    // Called by the transfer processor only for a saved, inactive source.
    // Active playback and local drafts must remain untouched until the user
    // switches playlists or saves their changes.
    pub(crate) fn finish_inactive_playlist_move(
        &mut self,
        request: &super::collections::transfer::MoveRequest,
    ) -> io::Result<bool> {
        if !self.writable || self.temporary || self.dirty {
            return Ok(false);
        }
        let Some(storage) = self.storage.as_mut() else { return Ok(false) };
        if storage.collection.id != request.source {
            return Ok(false);
        }
        let index = self.slots.iter().position(|slot| slot.id == request.snapshot.id);
        if let Some(index) = index {
            let slot = &self.slots[index];
            if index == self.active
                || self.slots.len() <= 1
                || slot.dirty
                || slot.snapshot() != &slot.saved
                || !slot.stored.as_ref().is_some_and(|stored| stored.manifest == request.snapshot)
            {
                return Ok(false);
            }
        }
        match request.cleanup_source_playlist(&storage.paths, &mut storage.collection) {
            Ok(true) => {
                if let Some(index) = index {
                    if storage.collection.playlist_order.contains(&request.snapshot.id) {
                        return Err(io::Error::other("Completed move still belongs to source collection"));
                    }
                    self.slots.remove(index);
                    if index < self.active {
                        self.active -= 1;
                    }
                }
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(err) => {
                // The membership update may have reached disk before an I/O
                // error. Do not let a stale workspace overwrite it.
                self.writable = false;
                Err(err)
            }
        }
    }

    /// A native watcher gates the existing UI tick; only unresolved requests
    /// (or a platform without a native watcher) use the timer fallback.
    pub(crate) fn transfer_wakeup(&mut self, fallback_due: bool) -> bool {
        if !self.writable || self.temporary {
            return false;
        }
        let Some(storage) = self.storage.as_mut() else { return false };
        if storage.watch.is_none() {
            storage.watch = Some(super::collections::watch::TransferWatch::new(
                &storage.paths.collection_dir(storage.collection.id),
            ));
        }
        storage.watch.as_mut().unwrap().ready(fallback_due) || (storage.retry_transfers && fallback_due)
    }

    pub(crate) fn retry_transfers(&mut self, retry: bool) {
        if let Some(storage) = self.storage.as_mut() {
            storage.retry_transfers = retry;
        }
    }

    pub(crate) fn has_pending_copies(&self) -> io::Result<bool> {
        let Some(storage) = &self.storage else { return Ok(false) };
        let inbox = storage.paths.collection_dir(storage.collection.id).join("inbox");
        let entries = match fs::read_dir(inbox) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        for entry in entries {
            let entry = entry?;
            if entry.path().extension().is_some_and(|ext| ext == "json" || ext == "items")
                && entry.file_type()?.is_file()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Process both sides of immutable move requests. Only a saved source with
    /// matching content and a durable destination receipt can be removed.
    /// Runtime changes are returned even if a later request encounters an error.
    pub(crate) fn poll_moves(&mut self, live: &Snapshot, progress: &mut MoveProgress) -> io::Result<()> {
        use super::collections::transfer::{self, MoveTarget};
        if !self.writable || self.temporary {
            return Ok(());
        }
        let Some(storage) = self.storage.as_ref() else { return Ok(()) };
        let incoming = transfer::requests(&storage.paths, storage.collection.id, true)?;
        let outgoing = transfer::requests(&storage.paths, storage.collection.id, false)?;
        if incoming.is_empty() && outgoing.is_empty() {
            return Ok(());
        }
        if self.dirty {
            progress.retry = true;
            return Ok(());
        }
        let disk = Self::read_collection(&storage.paths, storage.collection.id)?;
        if disk != storage.collection || !disk.playlist_order.iter().copied().eq(self.slots.iter().map(|slot| slot.id))
        {
            self.writable = false;
            return Err(io::Error::other("Collection changed on disk; reopen it before processing transfers"));
        }
        let mut live = live.clone();
        for request in incoming {
            let storage = self.storage.as_mut().unwrap();
            if request.rejected(&storage.paths)?
                || (request.cancellation_requested(&storage.paths)?
                    && !request.import_started(&storage.paths)?
                    && !request.has_receipt(&storage.paths)?)
            {
                match request.reject(&storage.paths) {
                    Ok(true) => {
                        progress.notice = Some(format!(
                            "Move from \"{}\" cancelled. The original items were kept.",
                            request.snapshot.name
                        ));
                    }
                    Ok(false) => {}
                    Err(err) => {
                        log::warn!("Could not confirm cancellation {}: {err}", request.id);
                        progress.retry = true;
                    }
                }
                continue;
            }
            // A committed receipt may precede the wake-up (e.g. after a crash).
            // Do not re-import or require the original playlist to remain intact.
            if request.has_receipt(&storage.paths)? {
                if let Err(err) = request.notify_source(&storage.paths) {
                    log::warn!("Could not deliver move receipt {}: {err}", request.id);
                    progress.retry = true;
                    progress.notice = Some(format!(
                        "Received \"{}\", but could not notify the source collection yet.",
                        request.snapshot.name
                    ));
                }
                continue;
            }
            if request.destination_waits_for_previous_move(&storage.paths)? {
                progress.notice = Some(format!(
                    "Waiting to receive \"{}\" until an earlier move in this collection has finished.",
                    request.snapshot.name
                ));
                progress.retry = true;
                continue;
            }
            let id = match request.target {
                MoveTarget::Collection => request.snapshot.id,
                MoveTarget::Playlist(id) => id,
            };
            let started = request.import_started(&storage.paths)?;
            let slot_index = self.slots.iter().position(|slot| slot.id == id);
            if request.target == MoveTarget::Collection
                && self.slots.iter().any(|slot| slot.id != id && slot.name.eq_ignore_ascii_case(&request.snapshot.name))
            {
                progress.notice = Some(format!(
                    "Could not receive playlist \"{}\" because a playlist with that name already exists here.",
                    request.snapshot.name
                ));
                progress.retry = true;
                continue;
            }
            if matches!(request.target, MoveTarget::Playlist(_)) && slot_index.is_none() {
                progress.notice = Some(format!(
                    "Could not receive items from \"{}\" because the destination playlist no longer exists.",
                    request.snapshot.name
                ));
                continue;
            }
            if !started
                && ((request.target == MoveTarget::Collection
                    && (slot_index.is_some()
                        || storage.paths.playlist_manifest(storage.collection.id, id).try_exists()?
                        || storage.paths.playlist_m3u8(storage.collection.id, id).try_exists()?))
                    || (matches!(request.target, MoveTarget::Playlist(_))
                        && slot_index.is_some_and(|index| {
                            request.selected.iter().any(|id| self.slots[index].saved.ids.contains(id))
                        })))
            {
                progress.notice = Some(format!(
                    "Could not receive \"{}\" because this collection already contains the playlist or items being moved.",
                    request.snapshot.name
                ));
                continue;
            }
            if let Some(index) = slot_index {
                let slot = &self.slots[index];
                if slot.dirty {
                    progress.retry = true;
                    continue;
                }
                let loaded = PlaylistStore::new(&storage.paths, storage.collection.id, id).load();
                if !loaded.as_ref().is_ok_and(|loaded| {
                    !loaded.externally_edited
                        && slot.stored.as_ref().is_some_and(|stored| stored.same_disk_version(loaded))
                }) {
                    progress.notice = Some(format!(
                        "Could not receive items in \"{}\" because the playlist has changed on disk. Reopen the collection.",
                        slot.name
                    ));
                    continue;
                }
                if matches!(request.target, MoveTarget::Playlist(_)) {
                    let draft = if index == self.active { &live } else { slot.snapshot() };
                    if request.selected.iter().any(|id| draft.ids.contains(id) && !slot.saved.ids.contains(id)) {
                        progress.notice = Some(format!(
                            "Could not receive items in \"{}\" because an incoming item is already in its unsaved changes.",
                            slot.name
                        ));
                        continue;
                    }
                }
            }
            let loaded = match request.import_destination(&storage.paths, &mut storage.collection) {
                Ok(Some(loaded)) => loaded,
                Ok(None) => continue,
                Err(err) => {
                    // A write may have reached disk before a later error. Do
                    // not allow a stale workspace to overwrite recovered data.
                    self.writable = false;
                    return Err(err);
                }
            };
            if let Some(index) = slot_index {
                let slot = &mut self.slots[index];
                let previous: std::collections::HashSet<_> = slot.saved.ids.iter().copied().collect();
                let appended: Vec<_> =
                    loaded.manifest.entries.iter().filter(|entry| !previous.contains(&entry.id)).cloned().collect();
                if index == self.active {
                    live.append_entries(&appended);
                    slot.draft = Some(live.clone());
                    progress.active_added.extend(appended);
                } else if let Some(draft) = &mut slot.draft {
                    draft.append_entries(&appended);
                }
                slot.saved = Snapshot::from_manifest(&loaded.manifest);
                slot.stored = Some(loaded);
            } else {
                let manifest = &loaded.manifest;
                self.slots.push(Slot {
                    name: manifest.name.clone(),
                    autosave: manifest.autosave,
                    saved: Snapshot::from_manifest(manifest),
                    id: manifest.id,
                    stored: Some(loaded),
                    dirty: false,
                    draft: None,
                });
            }
            progress.received += 1;
            if let Err(err) = request.notify_source(&storage.paths) {
                log::warn!("Could not deliver move receipt {}: {err}", request.id);
                progress.retry = true;
                progress.notice = Some(format!(
                    "Received \"{}\", but could not notify the source collection yet.",
                    request.snapshot.name
                ));
            }
        }
        for request in outgoing {
            let storage = self.storage.as_ref().unwrap();
            if request.rejected(&storage.paths)? {
                continue;
            }
            if !request.has_receipt(&storage.paths)? {
                // Publication is idempotent and can resume after a crash. A
                // successful publication waits for the destination's wake-up,
                // not for another scan of the remote receipt every two seconds.
                if let Err(err) = request.ensure_published(&storage.paths) {
                    log::warn!("Could not send move request {}: {err}", request.id);
                    progress.notice = Some(format!(
                        "Could not send the move request for \"{}\". The original playlist is still here.",
                        request.snapshot.name
                    ));
                    progress.retry = true;
                }
                continue;
            }
            let index = self.slots.iter().position(|slot| slot.id == request.snapshot.id);
            if let Some(index) = index {
                let slot = &self.slots[index];
                let draft = if index == self.active { &live } else { slot.snapshot() };
                if slot.dirty || !draft.same_contents(&slot.saved) {
                    progress.notice = Some(format!(
                        "Could not finish moving \"{}\" because it has unsaved changes. The original playlist was kept.",
                        slot.name
                    ));
                    progress.retry = true;
                    continue;
                }
                let expected_after = if matches!(request.target, MoveTarget::Playlist(_)) {
                    Some(request.expected_source_after_items()?)
                } else {
                    None
                };
                if !slot.stored.as_ref().is_some_and(|stored| {
                    stored.manifest == request.snapshot || expected_after.as_ref() == Some(&stored.manifest)
                }) {
                    progress.notice = Some(format!(
                        "Could not finish moving \"{}\" because the source playlist changed after the request. The original playlist was kept.",
                        slot.name
                    ));
                    continue;
                }
                let loaded = PlaylistStore::new(&storage.paths, storage.collection.id, slot.id).load();
                if !loaded.as_ref().is_ok_and(|loaded| {
                    !loaded.externally_edited
                        && slot.stored.as_ref().is_some_and(|stored| stored.same_disk_version(loaded))
                }) {
                    progress.notice = Some(format!(
                        "Could not finish moving \"{}\" because the source playlist changed on disk. The original playlist was kept.",
                        slot.name
                    ));
                    continue;
                }
            }
            match request.target {
                MoveTarget::Collection => {
                    if index == Some(self.active) || (index.is_some() && self.slots.len() <= 1) {
                        progress.notice = Some(format!(
                            "Playlist \"{}\" was received. Switch to another playlist to finish moving it.",
                            request.snapshot.name
                        ));
                        progress.retry = true;
                        continue;
                    }
                    if let Some(index) = index {
                        if self.slots[index].snapshot() != &self.slots[index].saved {
                            progress.notice = Some(format!(
                                "Could not finish moving \"{}\" because its playback state has unsaved changes.",
                                request.snapshot.name
                            ));
                            progress.retry = true;
                            continue;
                        }
                    }
                    if self.finish_inactive_playlist_move(&request)? {
                        progress.finished += 1;
                    } else {
                        progress.retry = true;
                    }
                }
                MoveTarget::Playlist(_) => {
                    let Some(index) = index else {
                        progress.notice = Some(format!(
                            "Could not finish moving items because the source playlist \"{}\" no longer exists.",
                            request.snapshot.name
                        ));
                        continue;
                    };
                    let draft = if index == self.active { &live } else { self.slots[index].snapshot() };
                    if index == self.active
                        && draft.current.and_then(|i| draft.ids.get(i)).is_some_and(|id| request.selected.contains(id))
                    {
                        progress.notice = Some(format!(
                            "Could not finish moving items from \"{}\" because one is currently open for playback. Open another item first.",
                            request.snapshot.name
                        ));
                        progress.retry = true;
                        continue;
                    }
                    let result = request.cleanup_source_items(&storage.paths).and_then(|done| {
                        if !done {
                            return Ok(None);
                        }
                        PlaylistStore::new(&storage.paths, storage.collection.id, request.snapshot.id).load().map(Some)
                    });
                    let loaded = match result {
                        Ok(Some(loaded)) => loaded,
                        Ok(None) => continue,
                        Err(err) => {
                            self.writable = false;
                            return Err(err);
                        }
                    };
                    let slot = &mut self.slots[index];
                    let removed: Vec<_> =
                        slot.saved.ids.iter().copied().filter(|id| request.selected.contains(id)).collect();
                    if index == self.active {
                        live.remove_entries(&request.selected);
                        slot.draft = Some(live.clone());
                        progress.active_removed.extend(removed);
                    } else if let Some(draft) = &mut slot.draft {
                        draft.remove_entries(&request.selected);
                    }
                    slot.saved = Snapshot::from_manifest(&loaded.manifest);
                    slot.stored = Some(loaded);
                    progress.finished += 1;
                }
            }
        }
        Ok(())
    }

    /// Inspect pending moves without importing, acknowledging or removing anything.
    pub fn move_pending_summary(&self, inbound: bool) -> io::Result<Vec<(String, String, uuid::Uuid, bool)>> {
        use super::collections::transfer::{self, MoveTarget};
        let Some(storage) = self.storage.as_ref().filter(|_| !self.temporary) else { return Ok(Vec::new()) };
        let paths = &storage.paths;
        let mut rows = Vec::new();
        for request in transfer::requests(paths, storage.collection.id, inbound)? {
            let other = if inbound { request.source } else { request.destination };
            let other_name =
                Self::read_collection(paths, other).map(|c| c.name).unwrap_or_else(|_| "Unavailable collection".into());
            let direction = if inbound { "from" } else { "to" };
            let label = match request.target {
                MoveTarget::Collection => {
                    format!("Move playlist \"{}\" {direction} {other_name}", request.snapshot.name)
                }
                MoveTarget::Playlist(_) => format!(
                    "Move {} item(s) {direction} {other_name} (source: {})",
                    request.selected.len(),
                    request.snapshot.name
                ),
            };
            let receipt = match request.has_receipt(paths) {
                Ok(receipt) => receipt,
                Err(err) => {
                    log::warn!("Could not inspect move confirmation {}: {err}", request.id);
                    rows.push((
                        label,
                        "Could not verify that the destination saved the move. The original items will be kept.".into(),
                        request.id,
                        false,
                    ));
                    continue;
                }
            };
            let started = request.import_started(paths)?;
            let cancelling = request.cancellation_requested(paths)?;
            let rejected = request.rejected(paths)?;
            let can_cancel = self.writable && !receipt && !started && !rejected && (inbound || !cancelling);
            let status = if rejected {
                "Move rejected; waiting to notify the source collection"
            } else if cancelling && !receipt && !started {
                "Cancellation requested; waiting for the destination to confirm"
            } else if !self.writable {
                "Waiting for this collection to be opened for editing"
            } else if self.dirty {
                "Waiting for collection changes to be saved"
            } else if inbound {
                if receipt {
                    "Items saved; waiting to notify the source collection"
                } else if request.target == MoveTarget::Collection
                    && self.slots.iter().any(|slot| {
                        slot.id != request.snapshot.id && slot.name.eq_ignore_ascii_case(&request.snapshot.name)
                    })
                {
                    "A playlist with this name already exists here"
                } else if let MoveTarget::Playlist(id) = request.target {
                    match self.slots.iter().find(|slot| slot.id == id) {
                        None => "The destination playlist no longer exists",
                        Some(slot) if slot.dirty => "Waiting for destination playlist changes to be saved",
                        Some(_) => "Waiting for automatic processing",
                    }
                } else {
                    "Waiting for automatic processing"
                }
            } else if !receipt {
                if !paths.collection_manifest(request.destination).is_file() {
                    "The destination collection is unavailable; the original items are kept"
                } else {
                    "Waiting for the destination to save the move"
                }
            } else if let Some(index) = self.slots.iter().position(|slot| slot.id == request.snapshot.id) {
                let slot = &self.slots[index];
                if slot.dirty || !slot.snapshot().same_contents(&slot.saved) {
                    "The destination saved the move, but the original playlist has unsaved changes"
                } else if !slot.stored.as_ref().is_some_and(|stored| {
                    stored.manifest == request.snapshot
                        || request.expected_source_after_items().is_ok_and(|after| stored.manifest == after)
                }) {
                    "The original playlist changed after the move was requested; it has been kept"
                } else if index == self.active && request.target == MoveTarget::Collection {
                    "The destination saved the playlist; switch to another playlist to finish moving it"
                } else if index == self.active
                    && slot
                        .snapshot()
                        .current
                        .and_then(|i| slot.snapshot().ids.get(i))
                        .is_some_and(|id| request.selected.contains(id))
                {
                    "One of the items is open for playback; open another item to finish moving it"
                } else {
                    "The destination saved the move; waiting to remove the original items"
                }
            } else {
                "The destination saved the move; waiting to verify source removal"
            };
            rows.push((label, status.into(), request.id, can_cancel));
        }
        Ok(rows)
    }

    pub fn cancel_pending_move(&mut self, inbound: bool, id: uuid::Uuid) -> io::Result<bool> {
        if !self.writable || self.temporary {
            return Ok(false);
        }
        let Some(storage) = self.storage.as_mut() else { return Ok(false) };
        let Some(request) = super::collections::transfer::requests(&storage.paths, storage.collection.id, inbound)?
            .into_iter()
            .find(|request| request.id == id)
        else {
            return Ok(false);
        };
        if inbound { request.reject(&storage.paths) } else { request.request_cancel(&storage.paths) }
    }

    pub fn inbox_pending_summary(&self) -> io::Result<Vec<(String, String)>> {
        let Some(storage) = self.storage.as_ref().filter(|_| !self.temporary) else { return Ok(Vec::new()) };
        let ready = if !self.writable {
            "Waiting for this collection to be opened for editing"
        } else if self.dirty {
            "Waiting for collection changes to be saved"
        } else {
            "Waiting for automatic processing"
        };
        super::collections::inbox::describe_pending(
            &storage.paths,
            storage.collection.id,
            |target| {
                let Some(slot) = self.slots.iter().find(|slot| slot.id == target) else {
                    return (target.to_string(), "The destination playlist is no longer in this collection".into());
                };
                let status = if !self.writable || self.dirty {
                    ready.into()
                } else if slot.dirty {
                    "Waiting for playlist changes to be saved".into()
                } else {
                    match PlaylistStore::new(&storage.paths, storage.collection.id, target).load() {
                        Ok(loaded)
                            if !loaded.externally_edited
                                && slot.stored.as_ref().is_some_and(|stored| stored.same_disk_version(&loaded)) =>
                        {
                            ready.into()
                        }
                        Ok(_) => "Destination playlist changed on disk".into(),
                        Err(err) => {
                            log::warn!("Could not read destination playlist {target}: {err}");
                            "Could not read the saved destination playlist".into()
                        }
                    }
                };
                (slot.name.clone(), status)
            },
            ready,
        )
    }

    /// Import new playlists without replacing the live workspace or its drafts.
    /// Existing slot indices remain stable. Only the collection owner may write.
    pub fn poll_inbox(&mut self, active_added: &mut Vec<PlaylistEntry>) -> io::Result<usize> {
        if !self.writable || self.temporary || self.dirty {
            return Ok(0);
        }
        let Some(storage) = self.storage.as_mut() else { return Ok(0) };
        let inbox = storage.paths.collection_dir(storage.collection.id).join("inbox");
        let entries = match fs::read_dir(inbox) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(err),
        };
        let mut pending = false;
        for entry in entries {
            let entry = entry?;
            if entry.path().extension().is_some_and(|ext| ext == "json" || ext == "items")
                && entry.file_type()?.is_file()
            {
                pending = true;
                break;
            }
        }
        if !pending {
            return Ok(0);
        }
        let result = (|| {
            let disk: CollectionManifest =
                serde_json::from_slice(&fs::read(storage.paths.collection_manifest(storage.collection.id))?)?;
            if disk != storage.collection {
                return Err(io::Error::other("Collection changed on disk; reopen before importing"));
            }
            let old_len = self.slots.len();
            if !disk.playlist_order.iter().copied().eq(self.slots.iter().map(|slot| slot.id)) {
                return Err(io::Error::other("Playlist order changed; reopen before importing"));
            }
            super::collections::inbox::import_pending(&storage.paths, &mut storage.collection)?;
            let mut added = Vec::new();
            for &id in &storage.collection.playlist_order[old_len..] {
                let loaded = PlaylistStore::new(&storage.paths, storage.collection.id, id).load()?;
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
                if loaded.externally_edited {
                    return Err(io::Error::other("Imported playlist changed on disk"));
                }
                added.push(Slot {
                    name: manifest.name.clone(),
                    autosave: manifest.autosave,
                    saved,
                    id,
                    stored: Some(loaded),
                    dirty: false,
                    draft: None,
                });
            }
            let count = added.len();
            self.slots.extend(added);
            let active = self.active;
            let slots = std::cell::RefCell::new(&mut self.slots);
            let imported = super::collections::inbox::import_items(
                &storage.paths,
                &storage.collection,
                |loaded| {
                    slots.borrow().iter().any(|slot| {
                        slot.id == loaded.manifest.id
                            && !slot.dirty
                            && slot.stored.as_ref().is_some_and(|stored| stored.same_disk_version(loaded))
                    })
                },
                |loaded| {
                    let manifest = &loaded.manifest;
                    if let Some((index, slot)) =
                        slots.borrow_mut().iter_mut().enumerate().find(|(_, slot)| slot.id == manifest.id)
                    {
                        let previous: std::collections::HashSet<_> = slot.saved.ids.iter().copied().collect();
                        let appended: Vec<_> =
                            manifest.entries.iter().filter(|e| !previous.contains(&e.id)).cloned().collect();
                        if let Some(draft) = &mut slot.draft {
                            for entry in &appended {
                                let index = draft.sources.len();
                                draft.sources.push(entry.source.clone());
                                draft.ids.push(entry.id);
                                if let Some(title) = &entry.custom_title {
                                    draft.custom_titles.insert(index, title.clone());
                                }
                            }
                        }
                        if index == active {
                            active_added.extend(appended);
                        }
                        slot.saved = Snapshot {
                            sources: manifest.entries.iter().map(|e| e.source.clone()).collect(),
                            ids: manifest.entries.iter().map(|e| e.id).collect(),
                            custom_titles: manifest
                                .entries
                                .iter()
                                .enumerate()
                                .filter_map(|(i, e)| e.custom_title.clone().map(|t| (i, t)))
                                .collect(),
                            current: manifest.current.and_then(|id| manifest.entries.iter().position(|e| e.id == id)),
                            position: manifest.position,
                        };
                        slot.stored = Some(loaded);
                    }
                },
            )?;
            Ok(count + imported)
        })();
        if result.is_err() {
            // An import may have committed the collection before failing to load
            // its slots. Never let a stale workspace overwrite that manifest.
            self.writable = false;
        }
        result
    }

    pub fn inbox_playlist_choices(config: &Path, target: CollectionId) -> io::Result<Vec<(PlaylistId, String)>> {
        let (paths, index, _lock) = Self::open_index(config, false)?;
        if !index.collection_order.contains(&target) {
            return Err(io::Error::other("Destination collection no longer exists"));
        }
        let collection = Self::read_collection(&paths, target)?;
        collection
            .playlist_order
            .into_iter()
            .map(|id| {
                let manifest: PlaylistManifest =
                    serde_json::from_slice(&fs::read(paths.playlist_manifest(target, id))?)?;
                manifest.validate().map_err(io::Error::other)?;
                if manifest.id != id {
                    return Err(io::Error::other("Invalid destination playlist ID"));
                }
                Ok((id, manifest.name))
            })
            .collect()
    }

    /// Stage a cross-instance move without changing either playlist. The source
    /// owner must later finish it only after a matching durable receipt.
    pub(crate) fn queue_move(
        &mut self,
        destination: CollectionId,
        target: super::collections::transfer::MoveTarget,
        snapshot: &Snapshot,
        indices: &[usize],
    ) -> io::Result<()> {
        use super::collections::transfer::{MoveRequest, MoveTarget};
        if !self.writable || self.temporary {
            return Err(io::Error::other("The source collection must be open for editing"));
        }
        if matches!(target, MoveTarget::Collection) && self.slots.len() < 2 {
            return Err(io::Error::other("Cannot move the last playlist in a collection"));
        }
        let saved = &self.slots[self.active].saved;
        if snapshot.sources != saved.sources
            || snapshot.ids != saved.ids
            || snapshot.custom_titles != saved.custom_titles
        {
            return Err(io::Error::other("Save the source playlist before moving"));
        }
        if matches!(target, MoveTarget::Playlist(_))
            && (indices.is_empty()
                || indices.iter().any(|&i| i >= snapshot.ids.len())
                || indices.windows(2).any(|w| w[0] >= w[1]))
        {
            return Err(io::Error::other("The selected items have changed; select them again"));
        }
        let storage = self.storage.as_ref().ok_or_else(|| io::Error::other("The source collection is unavailable"))?;
        if destination == storage.collection.id {
            return Err(io::Error::other("Choose another collection"));
        }
        let dest = Self::read_collection(&storage.paths, destination)?;
        if let MoveTarget::Playlist(id) = target {
            if !dest.playlist_order.contains(&id) {
                return Err(io::Error::other("The destination playlist no longer exists"));
            }
        }
        let selected: Vec<_> = if matches!(target, MoveTarget::Collection) {
            Vec::new()
        } else {
            indices.iter().map(|&i| snapshot.ids[i]).collect()
        };
        self.check_move_source(
            self.slots[self.active].id,
            if matches!(target, MoveTarget::Collection) { None } else { Some(&selected) },
        )?;
        self.capture(snapshot.clone(), true, true);
        self.flush()?;
        let storage = self.storage.as_ref().unwrap();
        let loaded = self.slots[self.active]
            .stored
            .as_ref()
            .ok_or_else(|| io::Error::other("Save the source playlist before moving"))?;
        let request = MoveRequest::new(storage.collection.id, destination, target, loaded.manifest.clone(), selected)?;
        request.stage(&storage.paths, loaded)?;
        // If publication fails, the durable source intent remains available for retry.
        request.publish(&storage.paths)
    }

    pub fn queue_item_copy(
        config: &Path,
        collection: CollectionId,
        target: PlaylistId,
        source: &Snapshot,
        indices: &[usize],
    ) -> io::Result<()> {
        if indices.is_empty()
            || indices.iter().any(|&i| i >= source.sources.len())
            || indices.windows(2).any(|w| w[0] >= w[1])
        {
            return Err(io::Error::other("Invalid playlist selection"));
        }
        let (paths, index, _lock) = Self::open_index(config, false)?;
        if !index.collection_order.contains(&collection)
            || !Self::read_collection(&paths, collection)?.playlist_order.contains(&target)
        {
            return Err(io::Error::other("Destination playlist no longer exists"));
        }
        let entries = indices
            .iter()
            .map(|&i| PlaylistEntry {
                id: PlaylistItemId::new(),
                source: source.sources[i].clone(),
                custom_title: source.custom_titles.get(&i).cloned(),
            })
            .collect();
        super::collections::inbox::enqueue_items(&paths, collection, target, entries)
    }

    pub fn queue_playlist_copy(
        config: &Path,
        target: CollectionId,
        name: &str,
        autosave: bool,
        mut snapshot: Snapshot,
    ) -> io::Result<()> {
        if snapshot.ids.len() != snapshot.sources.len() {
            return Err(io::Error::other("Invalid source playlist"));
        }
        snapshot.normalize();
        let (paths, index, _index_lock) = Self::open_index(config, false)?;
        if !index.collection_order.contains(&target) {
            return Err(io::Error::other("Destination collection no longer exists"));
        }
        let mut manifest = PlaylistManifest::new(name.to_owned());
        manifest.autosave = autosave;
        manifest.entries = snapshot
            .sources
            .into_iter()
            .enumerate()
            .map(|(i, source)| PlaylistEntry {
                id: PlaylistItemId::new(),
                source,
                custom_title: snapshot.custom_titles.get(&i).cloned(),
            })
            .collect();
        manifest.current = snapshot.current.and_then(|i| manifest.entries.get(i).map(|entry| entry.id));
        manifest.position = snapshot.position;
        super::collections::inbox::enqueue(&paths, target, &manifest)
    }

    /// Copy the live contents of a playlist into another, exclusively owned
    /// collection. The destination's active playlist is left unchanged.
    pub fn copy_playlist(&mut self, name: &str, autosave: bool, mut snapshot: Snapshot) -> io::Result<String> {
        if !self.writable || snapshot.ids.len() != snapshot.sources.len() {
            return Err(io::Error::other("Destination is not writable or source playlist is invalid"));
        }
        snapshot.normalize();
        self.flush()?;
        let name = self.suggested_name(name);
        let mut slot = Slot::new(name.clone(), autosave);
        snapshot.ids = snapshot.sources.iter().map(|_| PlaylistItemId::new()).collect();
        slot.saved = snapshot;
        self.slots.push(slot);
        self.dirty = true;
        self.flush()?;
        Ok(name)
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
    fn temporary_promotion_preserves_playlist_and_item_ids() {
        let root = std::env::temp_dir().join(format!("mavplayer-promote-{}", uuid::Uuid::new_v4()));
        let mut session = Library::new_session(&root).unwrap();
        let original = snapshot("first.mp4", 12.5);
        session.capture(original.clone(), true, false);
        let playlist_id = session.slots[0].id;
        session.promote_session(&root, "Saved session").unwrap();
        assert!(!session.is_temporary());
        assert_eq!(session.snapshot(), &original);
        assert_eq!(session.slots[0].id, playlist_id);
        assert!(Library::list_sessions(&root).unwrap().is_empty());
        let id = session.collection_id().unwrap();
        drop(session);
        let saved = Library::open_selected(&root, Some(id), false).unwrap();
        assert_eq!(saved.snapshot(), &original);
        drop(saved);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn kept_session_can_be_recovered_after_normal_exit() {
        let root = std::env::temp_dir().join(format!("mavplayer-keep-{}", uuid::Uuid::new_v4()));
        let mut session = Library::new_session(&root).unwrap();
        let id = session.storage.as_ref().unwrap().collection.id;
        let original = snapshot("kept.mp4", 10.0);
        session.capture(original.clone(), true, false);
        session.keep_session = true;
        assert!(session.can_leave());
        session.finish_temporary().unwrap();
        drop(session);
        let mut recovered = Library::recover_session(&root, id).unwrap();
        assert_eq!(recovered.snapshot(), &original);
        recovered.close_temporary().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collection_management_keeps_ids_and_removed_files() {
        let root = std::env::temp_dir().join(format!("mavplayer-manage-{}", uuid::Uuid::new_v4()));
        let mut first = Library::open(&root).unwrap();
        let id = first.collection_id().unwrap();
        let second_id = Library::create_collection(&root, "Second").unwrap();
        assert!(first.rename_collection(&root, "second").is_err());
        first.rename_collection(&root, "Renamed").unwrap();
        assert_eq!(first.collection_id(), Some(id));
        assert_eq!(first.collection_label(), "Renamed");
        first.edit_collection_index(&root, Some(1)).unwrap();
        assert_eq!(Library::collection_choices(&root).unwrap()[0].0, second_id);
        assert_eq!(first.collection_management_state(&root).unwrap(), (true, false, true, true));
        let second = Library::open_selected(&root, Some(second_id), false).unwrap();
        second.edit_collection_index(&root, None).unwrap();
        assert!(first.remove_collection(&root).is_err());
        drop(second);
        let next = first.remove_collection(&root).unwrap();
        assert_eq!(next.collection_id(), Some(second_id));
        assert_eq!(Library::collection_choices(&root).unwrap().len(), 1);
        assert!(CollectionPaths::new(&root).collection_manifest(id).exists());
        assert!(next.remove_collection(&root).is_err());
        drop(first);
        drop(next);
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
    fn copy_items_preserves_order_titles_and_destination_position() {
        let root = std::env::temp_dir().join(format!("mavplayer-copy-items-{}", uuid::Uuid::new_v4()));
        let mut library = Library::open(&root).unwrap();
        let mut source = snapshot("duplicate.mp4", 12.5);
        source.sources.push(source.sources[0].clone());
        source.ids.push(PlaylistItemId::new());
        source.custom_titles.insert(0, "First occurrence".into());
        source.custom_titles.insert(1, "Second occurrence".into());
        library.capture(source.clone(), true, true);
        library.create("Destination".into());
        let destination = snapshot("existing.mp4", 42.0);
        library.capture(destination.clone(), true, true);
        assert!(library.switch(0));
        assert_eq!(library.copy_items(1, &source, &[0, 1]).unwrap(), 2);
        assert_eq!(library.active, 0);
        assert_eq!(library.snapshot(), &source);
        assert!(library.copy_items(0, &source, &[0]).is_err());
        assert!(library.switch(1));
        let copied = library.snapshot().clone();
        assert_eq!(copied.sources, [destination.sources.clone(), source.sources.clone()].concat());
        assert_eq!(copied.ids[0], destination.ids[0]);
        assert_ne!(copied.ids[1], copied.ids[2]);
        assert!(copied.ids[1..].iter().all(|id| !source.ids.contains(id)));
        assert_eq!(copied.custom_titles.get(&1).map(String::as_str), Some("First occurrence"));
        assert_eq!(copied.custom_titles.get(&2).map(String::as_str), Some("Second occurrence"));
        assert_eq!(copied.current, destination.current);
        assert_eq!(copied.position, destination.position);
        library.flush().unwrap();
        drop(library);
        let restored = Library::open(&root).unwrap();
        assert_eq!(restored.snapshot(), &copied);
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_items_does_not_commit_unsaved_destination_draft() {
        let root = std::env::temp_dir().join(format!("mavplayer-copy-draft-{}", uuid::Uuid::new_v4()));
        let mut library = Library::open(&root).unwrap();
        let source = snapshot("source.mp4", 0.0);
        library.capture(source.clone(), true, true);
        library.create("Destination".into());
        let saved = snapshot("saved.mp4", 0.0);
        library.capture(saved.clone(), true, true);
        library.set_autosave(false);
        let draft = snapshot("unsaved.mp4", 0.0);
        library.capture(draft.clone(), true, false);
        assert!(library.switch(0));
        assert!(library.copy_items(1, &source, &[0]).is_err());
        assert_eq!(library.slots[1].snapshot(), &draft);
        library.flush().unwrap();
        drop(library);
        let mut restored = Library::open(&root).unwrap();
        assert!(restored.switch(1));
        assert_eq!(restored.snapshot(), &saved);
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inbox_pending_summary_is_read_only() {
        let root = std::env::temp_dir().join(format!("mavplayer-inbox-summary-{}", uuid::Uuid::new_v4()));
        let mut target = Library::open(&root).unwrap();
        let collection = target.collection_id().unwrap();
        let destination = target.slots[0].id;
        let incoming = snapshot("incoming.mp4", 0.0);
        Library::queue_item_copy(&root, collection, destination, &incoming, &[0]).unwrap();
        assert_eq!(target.inbox_pending_summary().unwrap().len(), 1);
        assert_eq!(target.inbox_pending_summary().unwrap().len(), 1);
        assert!(target.snapshot().sources.is_empty());
        assert_eq!(target.poll_inbox(&mut Vec::new()).unwrap(), 1);
        assert!(target.inbox_pending_summary().unwrap().is_empty());
        drop(target);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inbox_item_copy_appends_active_and_replays_without_duplicates() {
        let root = std::env::temp_dir().join(format!("mavplayer-item-inbox-{}", uuid::Uuid::new_v4()));
        let mut target = Library::open(&root).unwrap();
        let collection = target.collection_id().unwrap();
        let destination = target.slots[0].id;
        let mut original = snapshot("existing.mp4", 12.0);
        target.capture(original.clone(), true, true);
        target.flush().unwrap();
        let mut incoming = snapshot("incoming.mp4", 0.0);
        incoming.custom_titles.insert(0, "Incoming title".into());
        Library::queue_item_copy(&root, collection, destination, &incoming, &[0]).unwrap();
        let mut added = Vec::new();
        assert_eq!(target.poll_inbox(&mut added).unwrap(), 1);
        assert_eq!(added.len(), 1);
        assert_eq!(target.snapshot().ids[1], added[0].id);
        assert_eq!(target.poll_inbox(&mut Vec::new()).unwrap(), 0);
        let copy_id = target.slots[0].saved.ids[1];
        assert_ne!(copy_id, incoming.ids[0]);
        original.sources.push(incoming.sources[0].clone());
        original.ids.push(copy_id);
        original.custom_titles.insert(1, "Incoming title".into());
        assert_eq!(target.slots[0].saved, original);
        assert_eq!(target.snapshot(), &original);
        let inbox = CollectionPaths::new(&root).collection_dir(collection).join("inbox");
        let receipt = fs::read_dir(&inbox)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|ext| ext == "items-done"))
            .unwrap();
        fs::rename(&receipt, receipt.with_extension("items")).unwrap();
        let mut replayed = Vec::new();
        assert_eq!(target.poll_inbox(&mut replayed).unwrap(), 1);
        assert!(replayed.is_empty());
        assert_eq!(target.slots[0].saved, original);
        drop(target);
        let target = Library::open(&root).unwrap();
        assert_eq!(target.slots[0].saved, original);
        drop(target);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inbox_item_copy_preserves_unsaved_active_draft() {
        let root = std::env::temp_dir().join(format!("mavplayer-inbox-draft-{}", uuid::Uuid::new_v4()));
        let mut target = Library::open(&root).unwrap();
        let collection = target.collection_id().unwrap();
        let destination = target.slots[0].id;
        let saved = snapshot("saved.mp4", 12.0);
        target.set_autosave(false);
        target.capture(saved.clone(), true, true);
        target.flush().unwrap();
        let mut draft = snapshot("local-unsaved.mp4", 21.0);
        draft.custom_titles.insert(0, "Local edit".into());
        target.capture(draft.clone(), false, false);
        let incoming = snapshot("incoming.mp4", 0.0);
        Library::queue_item_copy(&root, collection, destination, &incoming, &[0]).unwrap();
        let mut added = Vec::new();
        assert_eq!(target.poll_inbox(&mut added).unwrap(), 1);
        assert_eq!(added.len(), 1);
        draft.sources.push(added[0].source.clone());
        draft.ids.push(added[0].id);
        assert_eq!(target.snapshot(), &draft);
        let mut expected = saved;
        expected.sources.push(added[0].source.clone());
        expected.ids.push(added[0].id);
        assert_eq!(target.slots[0].saved, expected);
        target.flush().unwrap();
        drop(target);
        let target = Library::open(&root).unwrap();
        assert_eq!(target.snapshot(), &expected);
        drop(target);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inbox_live_import_preserves_active_draft() {
        let root = std::env::temp_dir().join(format!("mavplayer-live-inbox-{}", uuid::Uuid::new_v4()));
        let mut target = Library::open(&root).unwrap();
        let id = target.collection_id().unwrap();
        target.set_autosave(false);
        target.flush().unwrap();
        let draft = snapshot("draft.mp4", 12.0);
        target.capture(draft.clone(), false, false);
        Library::queue_playlist_copy(&root, id, "Incoming", true, snapshot("incoming.mp4", 0.0)).unwrap();
        assert_eq!(target.poll_inbox(&mut Vec::new()).unwrap(), 1);
        assert_eq!(target.active, 0);
        assert_eq!(target.snapshot(), &draft);
        assert_eq!(target.slots.len(), 2);
        assert_eq!(target.poll_inbox(&mut Vec::new()).unwrap(), 0);
        target.flush().unwrap();
        drop(target);
        let target = Library::open(&root).unwrap();
        assert_eq!(target.slots.len(), 2);
        assert!(target.snapshot().sources.is_empty());
        drop(target);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn move_protocol_queue_preserves_source_and_rejects_duplicate() {
        use super::super::collections::transfer::{MoveRequest, MoveTarget};
        let root = std::env::temp_dir().join(format!("mavplayer-move-queue-{}", uuid::Uuid::new_v4()));
        let mut source = Library::open(&root).unwrap();
        let source_id = source.collection_id().unwrap();
        let destination = Library::create_collection(&root, "Destination").unwrap();
        let target = Library::open_selected(&root, Some(destination), false).unwrap();
        let original = snapshot("source.mp4", 10.0);
        source.capture(original.clone(), true, true);
        source.flush().unwrap();
        let choice = MoveTarget::Playlist(target.slots[0].id);
        source.queue_move(destination, choice, &original, &[0]).unwrap();
        assert_eq!(source.snapshot(), &original);
        assert!(source.queue_move(destination, choice, &original, &[0]).is_err());
        let paths = CollectionPaths::new(&root);
        let outbox = paths.collection_dir(source_id).join("outbox");
        let path = fs::read_dir(outbox)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|ext| ext == "move"))
            .unwrap();
        let bytes = fs::read(&path).unwrap();
        let request: MoveRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(request.selected, original.ids);
        assert_eq!(
            fs::read(paths.collection_dir(destination).join("inbox").join(path.file_name().unwrap())).unwrap(),
            bytes
        );
        assert!(target.snapshot().sources.is_empty());
        let outgoing = source.move_pending_summary(false).unwrap();
        assert_eq!(outgoing.len(), 1);
        assert!(outgoing[0].0.contains("Destination"));
        assert!(outgoing[0].1.contains("Waiting for the destination"));
        assert_eq!(target.move_pending_summary(true).unwrap().len(), 1);
        assert_eq!(source.move_pending_summary(false).unwrap(), outgoing);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(source.snapshot(), &original);
        assert!(target.snapshot().sources.is_empty());
        drop(target);
        drop(source);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn move_protocol_live_item_transfer_preserves_drafts_and_current_item() {
        use super::super::collections::transfer::MoveTarget;
        let root = std::env::temp_dir().join(format!("mavplayer-live-move-{}", uuid::Uuid::new_v4()));
        let mut source = Library::open(&root).unwrap();
        let source_id = source.collection_id().unwrap();
        let destination = Library::create_collection(&root, "Destination").unwrap();
        let mut target = Library::open_selected(&root, Some(destination), false).unwrap();
        let mut original = snapshot("playing.mp4", 10.0);
        let moving = snapshot("moving.mp4", 0.0);
        original.sources.push(moving.sources[0].clone());
        original.ids.push(moving.ids[0]);
        original.custom_titles.insert(1, "Moved title".into());
        source.capture(original.clone(), true, true);
        source.flush().unwrap();
        let saved = snapshot("destination.mp4", 20.0);
        target.set_autosave(false);
        target.capture(saved.clone(), true, true);
        target.flush().unwrap();
        let mut draft = snapshot("unsaved.mp4", 30.0);
        target.capture(draft.clone(), false, false);
        source.queue_move(destination, MoveTarget::Playlist(target.slots[0].id), &original, &[1]).unwrap();
        let mut before = MoveProgress::default();
        source.poll_moves(&original, &mut before).unwrap();
        assert_eq!(before.finished, 0);
        assert_eq!(source.snapshot(), &original);
        let mut received = MoveProgress::default();
        target.poll_moves(&draft, &mut received).unwrap();
        assert_eq!(received.received, 1);
        assert_eq!(received.active_added.len(), 1);
        assert_eq!(received.active_added[0].id, moving.ids[0]);
        assert_eq!(received.active_added[0].custom_title.as_deref(), Some("Moved title"));
        draft.append_entries(&received.active_added);
        assert_eq!(target.snapshot(), &draft);
        assert_eq!(target.slots[0].saved.ids, vec![saved.ids[0], moving.ids[0]]);
        // A newly received identity cannot be moved again until the previous
        // source has finished, even when the recipient is already saved.
        assert!(target.check_move_source(target.slots[0].id, Some(&moving.ids)).is_err());
        let paths = CollectionPaths::new(&root);
        let outbox = paths.collection_dir(source_id).join("outbox");
        let request_path = fs::read_dir(&outbox)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "move"))
            .unwrap();
        assert!(request_path.with_extension("move-ack").is_file());
        assert!(!request_path.with_extension("source-done").exists());
        let mut done = MoveProgress::default();
        source.poll_moves(&original, &mut done).unwrap();
        assert_eq!(done.finished, 1);
        assert_eq!(done.active_removed, moving.ids);
        assert_eq!(source.snapshot().ids, vec![original.ids[0]]);
        assert_eq!(source.snapshot().current, Some(0));
        assert_eq!(source.snapshot().position, Some(10.0));
        assert!(request_path.with_extension("source-done").is_file());
        assert!(target.check_move_source(target.slots[0].id, Some(&moving.ids)).is_ok());
        let mut replay = MoveProgress::default();
        let live = source.snapshot().clone();
        source.poll_moves(&live, &mut replay).unwrap();
        assert_eq!(replay.finished, 0);
        target.poll_moves(&draft, &mut replay).unwrap();
        assert_eq!(replay.received, 0);
        assert_eq!(target.snapshot(), &draft);
        drop(target);
        drop(source);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn move_protocol_live_playlist_roundtrip_waits_for_source_switch() {
        use super::super::collections::transfer::MoveTarget;
        let root = std::env::temp_dir().join(format!("mavplayer-live-playlist-move-{}", uuid::Uuid::new_v4()));
        let mut source = Library::open(&root).unwrap();
        let source_id = source.collection_id().unwrap();
        let destination = Library::create_collection(&root, "Destination").unwrap();
        let mut target = Library::open_selected(&root, Some(destination), false).unwrap();
        source.create("Movies".into());
        let original = snapshot("movie.mp4", 5.0);
        source.capture(original.clone(), true, true);
        source.flush().unwrap();
        let id = source.slots[source.active].id;
        source.queue_move(destination, MoveTarget::Collection, &original, &[]).unwrap();
        let mut received = MoveProgress::default();
        let live = target.snapshot().clone();
        target.poll_moves(&live, &mut received).unwrap();
        assert_eq!(received.received, 1);
        assert_eq!(target.slots[1].id, id);
        assert_eq!(target.slots[1].saved, original);
        assert!(target.check_move_source(id, None).is_err());
        let mut waiting = MoveProgress::default();
        source.poll_moves(&original, &mut waiting).unwrap();
        assert_eq!(waiting.finished, 0);
        assert!(waiting.retry);
        assert!(source.switch(0));
        source.flush().unwrap();
        let mut done = MoveProgress::default();
        let live = source.snapshot().clone();
        source.poll_moves(&live, &mut done).unwrap();
        assert_eq!(done.finished, 1);
        assert_eq!(source.slots.len(), 1);
        let paths = CollectionPaths::new(&root);
        assert!(!paths.playlist_manifest(source_id, id).exists());
        assert!(!paths.playlist_m3u8(source_id, id).exists());
        assert!(target.switch(1));
        target.flush().unwrap();
        target.queue_move(source_id, MoveTarget::Collection, &original, &[]).unwrap();
        let mut returned = MoveProgress::default();
        let live = source.snapshot().clone();
        source.poll_moves(&live, &mut returned).unwrap();
        assert_eq!(returned.received, 1);
        assert_eq!(source.slots[1].id, id);
        assert_eq!(source.slots[1].saved, original);
        assert!(target.switch(0));
        target.flush().unwrap();
        let mut final_cleanup = MoveProgress::default();
        let live = target.snapshot().clone();
        target.poll_moves(&live, &mut final_cleanup).unwrap();
        assert_eq!(final_cleanup.finished, 1);
        assert_eq!(target.slots.len(), 1);
        assert!(!paths.playlist_manifest(destination, id).exists());
        assert!(paths.playlist_manifest(source_id, id).is_file());
        drop(target);
        drop(source);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn move_protocol_live_changed_source_is_not_removed() {
        use super::super::collections::transfer::MoveTarget;
        let root = std::env::temp_dir().join(format!("mavplayer-live-move-conflict-{}", uuid::Uuid::new_v4()));
        let mut source = Library::open(&root).unwrap();
        let destination = Library::create_collection(&root, "Destination").unwrap();
        let mut target = Library::open_selected(&root, Some(destination), false).unwrap();
        let mut original = snapshot("movie.mp4", 5.0);
        original.current = None;
        original.position = None;
        source.capture(original.clone(), true, true);
        source.flush().unwrap();
        source.queue_move(destination, MoveTarget::Playlist(target.slots[0].id), &original, &[0]).unwrap();
        let mut changed = original.clone();
        changed.custom_titles.insert(0, "New title".into());
        source.capture(changed.clone(), true, true);
        source.flush().unwrap();
        let mut received = MoveProgress::default();
        let live = target.snapshot().clone();
        target.poll_moves(&live, &mut received).unwrap();
        assert_eq!(received.received, 1);
        let mut done = MoveProgress::default();
        source.poll_moves(&changed, &mut done).unwrap();
        assert_eq!(done.finished, 0);
        assert!(done.notice.as_deref().is_some_and(|notice| notice.contains("source playlist changed")));
        assert_eq!(source.snapshot(), &changed);
        assert!(source.writable);
        drop(target);
        drop(source);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inbox_copy_while_locked_and_replay_after_commit() {
        let root = std::env::temp_dir().join(format!("mavplayer-inbox-{}", uuid::Uuid::new_v4()));
        let source = Library::open(&root).unwrap();
        let target_id = Library::create_collection(&root, "Destination").unwrap();
        let target = Library::open_selected(&root, Some(target_id), false).unwrap();
        let mut original = snapshot("source.mp4", 10.0);
        original.custom_titles.insert(0, "Custom title".into());
        Library::queue_playlist_copy(&root, target_id, "Default playlist", true, original.clone()).unwrap();
        assert_eq!(target.slots.len(), 1);
        let reader = Library::open_selected(&root, Some(target_id), true).unwrap();
        assert_eq!(reader.slots.len(), 1);
        drop(reader);
        drop(target);
        let mut target = Library::open_selected(&root, Some(target_id), false).unwrap();
        assert_eq!(target.slots.len(), 2);
        assert_eq!(target.active, 0);
        assert_eq!(target.slots[1].name, "Default playlist 2");
        let copied_id = target.slots[1].id;
        assert!(target.switch(1));
        assert_eq!(target.snapshot().sources, original.sources);
        assert_eq!(target.snapshot().custom_titles, original.custom_titles);
        assert_ne!(target.snapshot().ids, original.ids);
        drop(target);
        let inbox = CollectionPaths::new(&root).collection_dir(target_id).join("inbox");
        let request = inbox.join(format!("{copied_id}.json"));
        let receipt = request.with_extension("done");
        // Recreate the boundary after collection commit but before receipt.
        fs::rename(&receipt, &request).unwrap();
        let target = Library::open_selected(&root, Some(target_id), false).unwrap();
        assert_eq!(target.slots.len(), 2);
        assert!(receipt.exists());
        drop(target);
        fs::copy(&receipt, &request).unwrap();
        let target = Library::open_selected(&root, Some(target_id), false).unwrap();
        assert_eq!(target.slots.len(), 2);
        assert!(!request.exists());
        drop(target);
        drop(source);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_playlist_to_other_collection_preserves_source_and_allocates_new_ids() {
        let root = std::env::temp_dir().join(format!("mavplayer-copy-playlist-{}", uuid::Uuid::new_v4()));
        let mut source = Library::open(&root).unwrap();
        let mut original = snapshot("source.mp4", 10.0);
        original.custom_titles.insert(0, "Custom title".into());
        source.capture(original.clone(), true, true);
        source.flush().unwrap();
        let original_id = source.slots[0].id;
        let target_id = Library::create_collection(&root, "Destination").unwrap();
        let mut target = Library::open_selected(&root, Some(target_id), false).unwrap();
        assert!(Library::open_selected(&root, Some(target_id), false).is_err());
        let name = target.copy_playlist(source.name(), source.autosave(), original.clone()).unwrap();
        assert_eq!(name, "Default playlist 2");
        assert_eq!(target.active, 0);
        assert_eq!(source.snapshot(), &original);
        assert_ne!(target.slots[1].id, original_id);
        let copied_id = target.slots[1].id;
        drop(target);
        let mut restored = Library::open_selected(&root, Some(target_id), false).unwrap();
        assert_eq!(restored.active, 0);
        assert_eq!(restored.slots[1].id, copied_id);
        assert!(restored.switch(1));
        let copied = restored.snapshot();
        assert_ne!(copied.ids, original.ids);
        assert_eq!(copied.sources, original.sources);
        assert_eq!(copied.custom_titles, original.custom_titles);
        assert_eq!(copied.current, original.current);
        assert_eq!(copied.position, original.position);
        drop(restored);
        drop(source);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn move_playlist_between_collections_preserves_ids_and_can_move_back() {
        let root = std::env::temp_dir().join(format!("mavplayer-move-playlist-{}", uuid::Uuid::new_v4()));
        let mut source = Library::open(&root).unwrap();
        let source_id = source.collection_id().unwrap();
        source.create("Transfer".into());
        let original = snapshot("transfer.mp4", 12.5);
        source.capture(original.clone(), true, true);
        source.flush().unwrap();
        let playlist_id = source.slots[source.active].id;
        let target_id = Library::create_collection(&root, "Destination").unwrap();
        let mut target = Library::open_selected(&root, Some(target_id), false).unwrap();
        assert_eq!(source.move_playlist_to(&mut target, &original).unwrap(), "Transfer");
        assert_eq!(source.slots.len(), 1);
        assert_eq!(target.active, 0);
        drop(source);
        drop(target);
        let mut source = Library::open_selected(&root, Some(source_id), false).unwrap();
        let mut target = Library::open_selected(&root, Some(target_id), false).unwrap();
        assert!(target.switch(1));
        assert_eq!(target.slots[1].id, playlist_id);
        assert_eq!(target.snapshot(), &original);
        assert_eq!(target.move_playlist_to(&mut source, &original).unwrap(), "Transfer");
        assert_eq!(source.slots[1].id, playlist_id);
        assert_eq!(source.slots[1].snapshot(), &original);
        drop(source);
        drop(target);
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
