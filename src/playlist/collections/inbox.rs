//! Durable, immutable copy requests. Only the collection owner imports them.
use super::storage::{PlaylistStore, sync_directory, write_atomic};
use super::{CollectionId, CollectionManifest, CollectionPaths, PlaylistManifest};
use std::{fs, io};

pub(crate) fn enqueue(paths: &CollectionPaths, target: CollectionId, manifest: &PlaylistManifest) -> io::Result<()> {
    manifest.validate().map_err(io::Error::other)?;
    super::storage::render_m3u(manifest)?;
    let root = paths.collection_dir(target);
    let inbox = root.join("inbox");
    fs::create_dir_all(&inbox)?;
    sync_directory(&root)?;
    write_atomic(&inbox.join(format!("{}.json", manifest.id)), &serde_json::to_vec(manifest)?)
}

/// Must be called under the destination's exclusive lock, before loading slots.
/// The freshly allocated playlist ID doubles as the request's idempotency key.
pub(crate) fn import_pending(paths: &CollectionPaths, collection: &mut CollectionManifest) -> io::Result<()> {
    let inbox = paths.collection_dir(collection.id).join("inbox");
    let entries = match fs::read_dir(&inbox) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let mut requests = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|ext| ext == "json") {
            requests.push(entry.path());
        }
    }
    requests.sort();
    // Finish an interrupted import before introducing new name collisions.
    requests.sort_by_key(|path| {
        let stem = path.file_stem().unwrap_or_default();
        let mut filename = stem.to_os_string();
        filename.push(".json");
        !paths.playlists_dir(collection.id).join(filename).exists()
    });
    for path in requests {
        let mut manifest: PlaylistManifest = match serde_json::from_slice::<PlaylistManifest>(&fs::read(&path)?) {
            Ok(m)
                if m.validate().is_ok()
                    && m.revision == 0
                    && path.file_stem().is_some_and(|stem| stem == std::ffi::OsStr::new(&m.id.to_string())) =>
            {
                m
            }
            _ => {
                eprintln!("Invalid import request retained: {}", path.display());
                continue;
            }
        };
        let receipt = path.with_extension("done");
        if receipt.try_exists()? {
            // A previously completed request must never be imported again.
            if fs::read(&receipt)? != fs::read(&path)? {
                return Err(io::Error::other("Import request conflicts with its receipt"));
            }
            fs::remove_file(&path)?;
            sync_directory(&inbox)?;
            continue;
        }
        let store = PlaylistStore::new(paths, collection.id, manifest.id);
        store.recover()?;
        let already_linked = collection.playlist_order.contains(&manifest.id);
        let mut names = Vec::new();
        for &id in &collection.playlist_order {
            if id != manifest.id {
                names.push(PlaylistStore::new(paths, collection.id, id).load()?.manifest.name);
            }
        }
        let base = manifest.name.clone();
        if names.iter().any(|name| name.eq_ignore_ascii_case(&base)) {
            manifest.name = (2..)
                .map(|i| format!("{base} {i}"))
                .find(|candidate| !names.iter().any(|name| name.eq_ignore_ascii_case(candidate)))
                .unwrap();
        }
        if already_linked
            || paths.playlist_manifest(collection.id, manifest.id).try_exists()?
            || paths.playlist_m3u8(collection.id, manifest.id).try_exists()?
        {
            let loaded = store.load()?;
            manifest.revision = 1;
            if loaded.externally_edited || loaded.manifest != manifest {
                return Err(io::Error::other("Import request conflicts with existing playlist"));
            }
        } else {
            store.create(manifest.clone())?;
        }
        if !already_linked {
            let mut next = collection.clone();
            next.playlist_order.push(manifest.id);
            write_atomic(&paths.collection_manifest(collection.id), &serde_json::to_vec_pretty(&next)?)?;
            *collection = next;
        }
        // Keep a durable receipt, even if the user later deletes the playlist.
        fs::rename(&path, &receipt)?;
        sync_directory(&inbox)?;
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ItemRequest {
    version: u32,
    target: super::PlaylistId,
    entries: Vec<super::PlaylistEntry>,
}

pub(crate) fn enqueue_items(
    paths: &CollectionPaths,
    collection: CollectionId,
    target: super::PlaylistId,
    entries: Vec<super::PlaylistEntry>,
) -> io::Result<()> {
    let mut check = PlaylistManifest::new("Inbox items".into());
    check.entries = entries.clone();
    check.validate().map_err(io::Error::other)?;
    super::storage::render_m3u(&check)?;
    let root = paths.collection_dir(collection);
    let inbox = root.join("inbox");
    fs::create_dir_all(&inbox)?;
    sync_directory(&root)?;
    let request = ItemRequest { version: 1, target, entries };
    write_atomic(&inbox.join(format!("{}.items", uuid::Uuid::new_v4())), &serde_json::to_vec(&request)?)
}

/// The owner supplies eligible slots and receives the committed manifest before
/// any further request is processed. Active or unsaved slots can be deferred.
pub(crate) fn import_items(
    paths: &CollectionPaths,
    collection: &CollectionManifest,
    mut eligible: impl FnMut(&super::storage::LoadedPlaylist) -> bool,
    mut committed: impl FnMut(super::storage::LoadedPlaylist),
) -> io::Result<usize> {
    let inbox = paths.collection_dir(collection.id).join("inbox");
    let entries = match fs::read_dir(&inbox) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut requests = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|ext| ext == "items") {
            requests.push(entry.path());
        }
    }
    requests.sort();
    let mut count = 0;
    for path in requests {
        let bytes = fs::read(&path)?;
        let receipt = path.with_extension("items-done");
        if receipt.try_exists()? {
            if fs::read(&receipt)? != bytes {
                return Err(io::Error::other("Item request conflicts with receipt"));
            }
            fs::remove_file(&path)?;
            sync_directory(&inbox)?;
            continue;
        }
        let Ok(request) = serde_json::from_slice::<ItemRequest>(&bytes) else { continue };
        let mut check = PlaylistManifest::new("Inbox items".into());
        check.entries = request.entries.clone();
        if request.version != 1
            || request.entries.is_empty()
            || check.validate().is_err()
            || super::storage::render_m3u(&check).is_err()
        {
            continue;
        }
        if !collection.playlist_order.contains(&request.target) {
            continue;
        }
        let store = PlaylistStore::new(paths, collection.id, request.target);
        store.recover()?;
        let mut loaded = store.load()?;
        if loaded.externally_edited || !eligible(&loaded) {
            continue;
        }
        let existing: std::collections::HashMap<_, _> = loaded.manifest.entries.iter().map(|e| (e.id, e)).collect();
        let present = request.entries.iter().filter(|e| existing.contains_key(&e.id)).count();
        if present == 0 {
            loaded.manifest.entries.extend(request.entries);
            store.save(&mut loaded)?;
        } else if present != request.entries.len()
            || request.entries.iter().any(|e| existing.get(&e.id).copied() != Some(e))
        {
            return Err(io::Error::other("Item import conflicts with existing entries"));
        }
        fs::rename(&path, &receipt)?;
        sync_directory(&inbox)?;
        committed(loaded);
        count += 1;
    }
    Ok(count)
}

/// Read-only snapshot for the collection settings panel. Never recover or
/// acknowledge requests from the inspector, including in read-only instances.
pub(crate) fn describe_pending(
    paths: &CollectionPaths,
    collection: CollectionId,
    mut item_status: impl FnMut(super::PlaylistId) -> (String, String),
    ready_status: &str,
) -> io::Result<Vec<(String, String)>> {
    let inbox = paths.collection_dir(collection).join("inbox");
    let entries = match fs::read_dir(inbox) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut requests = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|ext| ext == "json" || ext == "items") {
            requests.push(entry.path());
        }
    }
    requests.sort();
    let mut result = Vec::new();
    for path in requests {
        let id = path.file_stem().unwrap_or_default().to_string_lossy();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                result.push((format!("Request {id}"), format!("Cannot read request: {err}")));
                continue;
            }
        };
        let items = path.extension().is_some_and(|ext| ext == "items");
        let receipt = path.with_extension(if items { "items-done" } else { "done" });
        let (label, mut status) = if items {
            match serde_json::from_slice::<ItemRequest>(&bytes) {
                Ok(request) => {
                    let mut check = PlaylistManifest::new("Inbox items".into());
                    check.entries = request.entries;
                    if request.version != 1
                        || check.entries.is_empty()
                        || check.validate().is_err()
                        || super::storage::render_m3u(&check).is_err()
                    {
                        (format!("Item request {id}"), "Invalid request".into())
                    } else {
                        let (name, status) = item_status(request.target);
                        (format!("Copy {} item(s) to {name}", check.entries.len()), status)
                    }
                }
                Err(_) => (format!("Item request {id}"), "Invalid request".into()),
            }
        } else {
            match serde_json::from_slice::<PlaylistManifest>(&bytes) {
                Ok(manifest)
                    if manifest.validate().is_ok() && manifest.revision == 0 && id == manifest.id.to_string() =>
                {
                    (
                        format!("Copy playlist: {} ({} items)", manifest.name, manifest.entries.len()),
                        ready_status.into(),
                    )
                }
                _ => (format!("Playlist request {id}"), "Invalid request".into()),
            }
        };
        match fs::read(&receipt) {
            Ok(previous) if previous == bytes => status = "Already imported; acknowledgment cleanup pending".into(),
            Ok(_) => status = "Request conflicts with existing receipt".into(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => status = format!("Cannot read receipt: {err}"),
        }
        result.push((label, status));
    }
    Ok(result)
}
