//! Named playlist workspaces. The active draft is independent of the last
//! explicitly saved (or autosaved) snapshot of each workspace.

use super::collections::storage::{LoadedPlaylist, PlaylistStore, sync_directory, write_atomic};
use super::collections::{
    CollectionIndex, CollectionManifest, CollectionPaths, PlaylistEntry, PlaylistId, PlaylistItemId, PlaylistManifest,
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

pub(crate) struct Library {
    storage: Option<CollectionStorage>,
    pub active: usize,
    pub slots: Vec<Slot>,
    pub writable: bool,
    dirty: bool,
}

impl Library {
    fn empty(writable: bool) -> Self {
        Self {
            storage: None,
            active: 0,
            slots: vec![Slot::new("Default playlist".into(), false)],
            writable,
            dirty: false,
        }
    }

    pub fn load(primary: bool) -> Self {
        if !primary {
            return Self::empty(false);
        }
        match Self::open(&config_dir()) {
            Ok(library) => library,
            Err(err) => {
                eprintln!("Could not open collection; using a temporary playlist: {err}");
                Self::empty(false)
            }
        }
    }

    fn open(config: &Path) -> io::Result<Self> {
        let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message);
        fs::create_dir_all(config)?;
        let paths = CollectionPaths::new(config);
        // Serialize first-run initialization as well as subsequent writes.
        let index_lock =
            OpenOptions::new().write(true).create(true).truncate(false).open(config.join("collections.lock"))?;
        index_lock.try_lock().map_err(|err| io::Error::other(format!("Collection index lock unavailable: {err}")))?;
        if !paths.index().try_exists()? {
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
        let id = index.default_collection.ok_or_else(|| invalid("Missing default collection"))?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(paths.collection_dir(id).join("collection.lock"))?;
        lock.try_lock().map_err(|err| io::Error::other(format!("Collection lock unavailable: {err}")))?;
        drop(index_lock);
        let collection: CollectionManifest = serde_json::from_slice(&fs::read(paths.collection_manifest(id))?)?;
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
            store.recover()?;
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
                autosave: manifest.autosave,
                saved,
                id,
                dirty: loaded.externally_edited,
                stored: Some(loaded),
                draft: None,
            });
        }
        let dirty = slots.iter().any(|slot| slot.dirty);
        Ok(Self {
            storage: Some(CollectionStorage { paths, collection, _lock: lock }),
            active,
            slots,
            writable: true,
            dirty,
        })
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
        if (manual || (slot.autosave && (exact || snapshot.needs_autosave(&slot.saved)))) && slot.saved != snapshot {
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
