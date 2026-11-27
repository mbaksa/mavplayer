//! Explicit, non-destructive migration rehearsal. The existing session remains
//! authoritative until a later activation step checks this snapshot again.

use super::storage::{PlaylistStore, sync_directory, write_atomic};
use super::{CollectionIndex, CollectionManifest, CollectionPaths, PlaylistEntry, PlaylistManifest};
use crate::engine::MediaSource;
use crate::playlist::session::Library;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;
use uuid::Uuid;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_legacy(bytes: &[u8]) -> io::Result<Library> {
    let library: Library = serde_json::from_slice(bytes)?;
    if library.version != 1 || library.slots.is_empty() || library.active >= library.slots.len() {
        return Err(invalid("Invalid legacy playlist library"));
    }
    for slot in &library.slots {
        let saved = &slot.saved;
        if slot.name.trim().is_empty()
            || saved.custom_titles.keys().any(|&i| i >= saved.sources.len())
            || saved.current.is_some_and(|i| i >= saved.sources.len())
            || saved.position.is_some_and(|p| saved.current.is_none() || !p.is_finite() || p < 0.0)
        {
            return Err(invalid("Invalid legacy playlist data; source left untouched"));
        }
        if saved.sources.iter().any(|source| matches!(source, MediaSource::File(path) if !path.is_absolute())) {
            return Err(invalid("Legacy library contains relative media paths; resolve them before migration"));
        }
    }
    Ok(library)
}

fn verify(root: &Path, library: &Library) -> io::Result<()> {
    let paths = CollectionPaths::new(root);
    let index: CollectionIndex = serde_json::from_slice(&fs::read(paths.index())?)?;
    index.validate().map_err(invalid)?;
    let id = index.default_collection.ok_or_else(|| invalid("Missing default collection"))?;
    if index.collection_order != [id] {
        return Err(invalid("Unexpected migration collection order"));
    }
    let collection: CollectionManifest = serde_json::from_slice(&fs::read(paths.collection_manifest(id))?)?;
    collection.validate().map_err(invalid)?;
    if collection.id != id
        || collection.playlist_order.len() != library.slots.len()
        || collection.active_playlist != Some(collection.playlist_order[library.active])
    {
        return Err(invalid("Migrated collection differs from legacy library"));
    }
    for (&playlist, slot) in collection.playlist_order.iter().zip(&library.slots) {
        let loaded = PlaylistStore::new(&paths, id, playlist).load()?;
        let manifest = loaded.manifest;
        let saved = &slot.saved;
        if loaded.externally_edited
            || manifest.name != slot.name
            || manifest.autosave != slot.autosave
            || manifest.position != saved.position
            || manifest.entries.len() != saved.sources.len()
            || manifest.current != saved.current.map(|i| manifest.entries[i].id)
            || manifest.entries.iter().enumerate().any(|(i, entry)| {
                entry.source != saved.sources[i] || entry.custom_title.as_ref() != saved.custom_titles.get(&i)
            })
        {
            return Err(invalid("Migrated playlist differs from legacy data"));
        }
    }
    Ok(())
}

fn prepare(config: &Path) -> io::Result<()> {
    let legacy = config.join("playlists.json");
    let destination = config.join("collections-migration");
    let bytes = fs::read(&legacy)?;
    let library = read_legacy(&bytes)?;
    if config.join("collections").try_exists()? {
        return Err(invalid("Active collections already exist; refusing migration"));
    }
    if destination.try_exists()? {
        if fs::read(destination.join("playlists.original.json"))? != bytes {
            return Err(invalid("Legacy library changed since rehearsal; move the staged directory aside and retry"));
        }
        return verify(&destination, &library);
    }
    let stage = config.join(format!(".collections-migration-{}", Uuid::new_v4()));
    fs::create_dir(&stage)?;
    let result = (|| {
        write_atomic(&stage.join("playlists.original.json"), &bytes)?;
        let paths = CollectionPaths::new(&stage);
        let mut collection = CollectionManifest::new("Default collection".into());
        for slot in &library.slots {
            let saved = &slot.saved;
            let mut playlist = PlaylistManifest::new(slot.name.clone());
            playlist.autosave = slot.autosave;
            playlist.entries = saved.sources.iter().enumerate().map(|(i, source)| {
                let mut entry = PlaylistEntry::new(source.clone());
                entry.custom_title = saved.custom_titles.get(&i).cloned();
                entry
            }).collect();
            playlist.current = saved.current.map(|i| playlist.entries[i].id);
            playlist.position = saved.position;
            collection.playlist_order.push(playlist.id);
            PlaylistStore::new(&paths, collection.id, playlist.id).create(playlist)?;
        }
        collection.active_playlist = Some(collection.playlist_order[library.active]);
        collection.validate().map_err(invalid)?;
        write_atomic(&paths.collection_manifest(collection.id), &serde_json::to_vec_pretty(&collection)?)?;
        let mut index = CollectionIndex::new();
        index.collection_order.push(collection.id);
        index.default_collection = Some(collection.id);
        write_atomic(&paths.index(), &serde_json::to_vec_pretty(&index)?)?;
        verify(&stage, &library)?;
        if fs::read(&legacy)? != bytes || destination.try_exists()? || config.join("collections").try_exists()? {
            return Err(invalid("Library changed during migration; refusing publication"));
        }
        fs::rename(&stage, &destination)?;
        sync_directory(config)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

pub(crate) fn run() -> io::Result<()> {
    let config = crate::settings::config_dir();
    let lock = OpenOptions::new().write(true).create(true).truncate(false).open(config.join("collections-migration.lock"))?;
    lock.try_lock().map_err(|error| io::Error::other(format!("Migration already running or lock unavailable: {error}")))?;
    if !crate::instance::listen(std::time::Duration::ZERO, |_| {}) {
        return Err(invalid("Close all MavPlayer windows and the tray process before migration"));
    }
    let result = prepare(&config);
    crate::instance::release();
    result?;
    println!("Migration verified: {}", config.join("collections-migration").display());
    println!("Legacy playlists remain active and unchanged.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_preserves_duplicates_and_is_repeatable() {
        let root = std::env::temp_dir().join(format!("mavplayer-migration-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let source = MediaSource::File(root.join("video.mp4"));
        let legacy = serde_json::json!({"version":1,"active":0,"slots":[{
            "name":"Movies","autosave":true,"saved":{"sources":[source.clone(),source],
            "custom_titles":{"0":"First","1":"Second"},"current":1,"position":42.0}
        }]});
        let bytes = serde_json::to_vec(&legacy).unwrap();
        fs::write(root.join("playlists.json"), &bytes).unwrap();
        prepare(&root).unwrap();
        let index = root.join("collections-migration/collections/index.json");
        let before = fs::read(&index).unwrap();
        prepare(&root).unwrap();
        assert_eq!(fs::read(index).unwrap(), before);
        assert_eq!(fs::read(root.join("playlists.json")).unwrap(), bytes);
        fs::write(root.join("playlists.json"), b"{}").unwrap();
        assert!(prepare(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
