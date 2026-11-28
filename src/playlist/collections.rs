//! Persistent identities and versioned manifests for local media collections.
//! Storage, migration and cache management are deliberately separate from this model.

pub(crate) mod inbox;
pub(crate) mod storage;
pub(crate) mod transfer;
pub(crate) mod watch;

use crate::engine::MediaSource;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) const FORMAT_VERSION: u32 = 1;

// A persistent ID identifies an occurrence, not a media file or a runtime queue
// index. The distinct types also prevent mixing collection and playlist IDs.
macro_rules! persistent_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub(crate) struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

persistent_id!(CollectionId);
persistent_id!(PlaylistId);
persistent_id!(PlaylistItemId);

impl std::str::FromStr for CollectionId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

fn unique<T: Eq + Hash>(ids: impl IntoIterator<Item = T>) -> bool {
    let mut seen = HashSet::new();
    ids.into_iter().all(|id| seen.insert(id))
}

fn check_version(version: u32) -> Result<(), &'static str> {
    if version == FORMAT_VERSION { Ok(()) } else { Err("Unsupported manifest version") }
}

/// Global order and the collection to try opening on startup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CollectionIndex {
    pub version: u32,
    pub collection_order: Vec<CollectionId>,
    pub default_collection: Option<CollectionId>,
}

impl CollectionIndex {
    pub fn new() -> Self {
        Self { version: FORMAT_VERSION, collection_order: Vec::new(), default_collection: None }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        check_version(self.version)?;
        if self.collection_order.iter().any(|id| id.0.is_nil()) || !unique(&self.collection_order) {
            return Err("Invalid or duplicate collection ID");
        }
        if self.default_collection.is_some_and(|id| !self.collection_order.contains(&id)) {
            return Err("Default collection is not in the index");
        }
        Ok(())
    }
}

/// A collection is an ordered list of playlists and its active playlist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CollectionManifest {
    pub version: u32,
    pub id: CollectionId,
    pub name: String,
    pub playlist_order: Vec<PlaylistId>,
    pub active_playlist: Option<PlaylistId>,
}

impl CollectionManifest {
    pub fn new(name: String) -> Self {
        Self {
            version: FORMAT_VERSION,
            id: CollectionId::new(),
            name,
            playlist_order: Vec::new(),
            active_playlist: None,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        check_version(self.version)?;
        if self.id.0.is_nil() || self.name.trim().is_empty() {
            return Err("Invalid collection ID or name");
        }
        let ids: HashSet<_> = self.playlist_order.iter().copied().collect();
        if ids.len() != self.playlist_order.len() || ids.iter().any(|id| id.0.is_nil()) {
            return Err("Invalid or duplicate playlist ID");
        }
        if self.active_playlist.is_some_and(|id| !ids.contains(&id)) {
            return Err("Active playlist is not in the collection");
        }
        Ok(())
    }
}

/// Durable user data for one appearance of a source in a playlist. Probed
/// metadata and thumbnail data do not belong here: they are rebuildable caches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlaylistEntry {
    pub id: PlaylistItemId,
    pub source: MediaSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
}

impl PlaylistEntry {
    pub fn new(source: MediaSource) -> Self {
        Self { id: PlaylistItemId::new(), source, custom_title: None }
    }
}

/// JSON companion to an ordinary UTF-8 M3U playlist. Source references are
/// also kept here so a future loader can detect edits to the M3U and reconcile
/// its entries rather than silently attaching user data to the wrong item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlaylistManifest {
    pub version: u32,
    pub id: PlaylistId,
    pub name: String,
    pub autosave: bool,
    #[serde(default)]
    pub revision: u64,
    pub entries: Vec<PlaylistEntry>,
    pub current: Option<PlaylistItemId>,
    pub position: Option<f64>,
}

impl PlaylistManifest {
    pub fn new(name: String) -> Self {
        Self {
            version: FORMAT_VERSION,
            id: PlaylistId::new(),
            name,
            autosave: false,
            revision: 0,
            entries: Vec::new(),
            current: None,
            position: None,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        check_version(self.version)?;
        if self.id.0.is_nil() || self.name.trim().is_empty() {
            return Err("Invalid playlist ID or name");
        }
        if self.entries.iter().any(|entry| entry.id.0.is_nil()) || !unique(self.entries.iter().map(|entry| entry.id)) {
            return Err("Invalid or duplicate playlist item ID");
        }
        if self.current.is_some_and(|id| !self.entries.iter().any(|entry| entry.id == id)) {
            return Err("Current item is not in the playlist");
        }
        if self.position.is_some_and(|position| self.current.is_none() || !position.is_finite() || position < 0.0) {
            return Err("Invalid playback position");
        }
        Ok(())
    }
}

/// Pure path construction; nothing is created on disk until the storage layer
/// explicitly writes a manifest. User-visible names are never path components.
pub(crate) struct CollectionPaths {
    root: PathBuf,
}

impl CollectionPaths {
    pub fn new(config_dir: &Path) -> Self {
        Self { root: config_dir.join("collections") }
    }

    pub fn sessions(config_dir: &Path) -> Self {
        Self { root: config_dir.join("sessions") }
    }

    pub fn index(&self) -> PathBuf {
        self.root.join("index.json")
    }

    pub fn collection_dir(&self, collection: CollectionId) -> PathBuf {
        self.root.join(collection.to_string())
    }

    pub fn collection_manifest(&self, collection: CollectionId) -> PathBuf {
        self.collection_dir(collection).join("collection.json")
    }

    pub fn playlists_dir(&self, collection: CollectionId) -> PathBuf {
        self.collection_dir(collection).join("playlists")
    }

    pub fn playlist_m3u8(&self, collection: CollectionId, playlist: PlaylistId) -> PathBuf {
        self.playlists_dir(collection).join(format!("{playlist}.m3u8"))
    }

    pub fn playlist_manifest(&self, collection: CollectionId, playlist: PlaylistId) -> PathBuf {
        self.playlists_dir(collection).join(format!("{playlist}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_and_playlist_manifests_roundtrip() {
        let mut collection = CollectionManifest::new("My videos".into());
        let mut playlist = PlaylistManifest::new("Film tests".into());
        let source = MediaSource::File("/videos/film.mkv".into());
        let mut first = PlaylistEntry::new(source.clone());
        first.custom_title = Some("Version A".into());
        let mut second = PlaylistEntry::new(source);
        second.custom_title = Some("Version B".into());
        assert_ne!(first.id, second.id);
        playlist.current = Some(first.id);
        playlist.position = Some(25.0);
        playlist.entries = vec![first, second];
        collection.playlist_order.push(playlist.id);
        collection.active_playlist = Some(playlist.id);
        let mut index = CollectionIndex::new();
        index.collection_order.push(collection.id);
        index.default_collection = Some(collection.id);
        assert_eq!(playlist.validate(), Ok(()));
        assert_eq!(collection.validate(), Ok(()));
        assert_eq!(index.validate(), Ok(()));

        let restored: PlaylistManifest = serde_json::from_str(&serde_json::to_string(&playlist).unwrap()).unwrap();
        assert_eq!(restored, playlist);
        let restored: CollectionManifest = serde_json::from_str(&serde_json::to_string(&collection).unwrap()).unwrap();
        assert_eq!(restored, collection);
        let restored: CollectionIndex = serde_json::from_str(&serde_json::to_string(&index).unwrap()).unwrap();
        assert_eq!(restored, index);
        assert_eq!(playlist.entries[0].source, playlist.entries[1].source);
    }

    #[test]
    fn item_identity_and_playback_position_survive_reordering() {
        let mut playlist = PlaylistManifest::new("Playlist".into());
        playlist.entries.push(PlaylistEntry::new(MediaSource::File("a.mp4".into())));
        playlist.entries.push(PlaylistEntry::new(MediaSource::File("b.mp4".into())));
        let id = playlist.entries[0].id;
        playlist.entries[0].custom_title = Some("My title".into());
        playlist.current = Some(id);
        playlist.position = Some(7.5);
        playlist.entries.swap(0, 1);
        assert_eq!(playlist.entries[1].id, id);
        assert_eq!(playlist.entries[1].custom_title.as_deref(), Some("My title"));
        assert_eq!(playlist.current, Some(id));
        assert_eq!(playlist.position, Some(7.5));
        assert_eq!(playlist.validate(), Ok(()));
    }

    #[test]
    fn invalid_manifests_are_rejected() {
        let mut index = CollectionIndex::new();
        let id = CollectionId::new();
        index.default_collection = Some(id);
        assert!(index.validate().is_err());
        index.collection_order = vec![id, id];
        assert!(index.validate().is_err());
        index.collection_order.pop();
        assert_eq!(index.validate(), Ok(()));
        index.version += 1;
        assert!(index.validate().is_err());

        let mut collection = CollectionManifest::new("Collection".into());
        let id = PlaylistId::new();
        collection.playlist_order = vec![id, id];
        assert!(collection.validate().is_err());
        collection.playlist_order.pop();
        collection.active_playlist = Some(PlaylistId::new());
        assert!(collection.validate().is_err());
        collection.active_playlist = Some(id);
        assert_eq!(collection.validate(), Ok(()));

        let mut playlist = PlaylistManifest::new("Playlist".into());
        let entry = PlaylistEntry::new(MediaSource::File("a.mp4".into()));
        playlist.entries = vec![entry.clone(), entry.clone()];
        assert!(playlist.validate().is_err());
        playlist.entries.pop();
        playlist.position = Some(1.0);
        assert!(playlist.validate().is_err());
        playlist.current = Some(PlaylistItemId::new());
        assert!(playlist.validate().is_err());
        playlist.current = Some(entry.id);
        playlist.position = Some(f64::NAN);
        assert!(playlist.validate().is_err());
        playlist.position = Some(-1.0);
        assert!(playlist.validate().is_err());
        playlist.position = Some(0.0);
        assert_eq!(playlist.validate(), Ok(()));
    }

    #[test]
    fn malformed_and_nil_ids_are_rejected() {
        assert!(serde_json::from_str::<CollectionId>(r#""../other""#).is_err());
        let nil: CollectionId = serde_json::from_str(r#""00000000-0000-0000-0000-000000000000""#).unwrap();
        let mut index = CollectionIndex::new();
        index.collection_order.push(nil);
        assert!(index.validate().is_err());
    }

    #[test]
    fn paths_depend_on_ids_not_display_names() {
        let paths = CollectionPaths::new(Path::new("config"));
        let mut collection = CollectionManifest::new("../../Videos".into());
        let mut playlist = PlaylistManifest::new("A/B".into());
        let root = Path::new("config").join("collections");
        let dir = root.join(collection.id.to_string());
        let playlists = dir.join("playlists");
        assert_eq!(paths.index(), root.join("index.json"));
        assert_eq!(paths.collection_dir(collection.id), dir);
        assert_eq!(paths.collection_manifest(collection.id), dir.join("collection.json"));
        assert_eq!(paths.playlists_dir(collection.id), playlists);
        let m3u = paths.playlist_m3u8(collection.id, playlist.id);
        let json = paths.playlist_manifest(collection.id, playlist.id);
        assert_eq!(m3u, playlists.join(format!("{}.m3u8", playlist.id)));
        assert_eq!(json, playlists.join(format!("{}.json", playlist.id)));
        collection.name = "New name".into();
        playlist.name = "Another name".into();
        assert_eq!(paths.playlist_m3u8(collection.id, playlist.id), m3u);
        assert_eq!(paths.playlist_manifest(collection.id, playlist.id), json);
    }
}
