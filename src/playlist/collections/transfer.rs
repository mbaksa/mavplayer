//! Cross-instance move protocol. Source and destination owners perform separate
//! steps under their respective exclusive collection locks. Durable receipts
//! are delivered to the source Outbox to wake its directory watcher.

use super::storage::{LoadedPlaylist, PlaylistStore, render_m3u, sync_directory, write_atomic};
use super::{CollectionId, CollectionPaths, PlaylistItemId, PlaylistManifest};
use serde::{Deserialize, Serialize};
use std::{fs, io, path::PathBuf};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum MoveTarget {
    Playlist(super::PlaylistId),
    Collection,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct MoveRequest {
    version: u32,
    pub id: Uuid,
    pub source: CollectionId,
    pub destination: CollectionId,
    pub target: MoveTarget,
    pub snapshot: PlaylistManifest,
    pub selected: Vec<PlaylistItemId>,
}

fn conflict(message: &str) -> io::Error {
    io::Error::other(message)
}

impl MoveRequest {
    pub(crate) fn new(
        source: CollectionId,
        destination: CollectionId,
        target: MoveTarget,
        snapshot: PlaylistManifest,
        selected: Vec<PlaylistItemId>,
    ) -> io::Result<Self> {
        let request = Self { version: 1, id: Uuid::new_v4(), source, destination, target, snapshot, selected };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> io::Result<()> {
        self.snapshot.validate().map_err(io::Error::other)?;
        render_m3u(&self.snapshot)?;
        if self.version != 1
            || self.id.is_nil()
            || self.source == self.destination
            || self.source.0.is_nil()
            || self.destination.0.is_nil()
            || self.snapshot.revision == 0
        {
            return Err(conflict("Invalid move request"));
        }
        match self.target {
            MoveTarget::Collection if self.selected.is_empty() => Ok(()),
            MoveTarget::Playlist(id) if !id.0.is_nil() && id != self.snapshot.id && !self.selected.is_empty() => {
                let selected: std::collections::HashSet<_> = self.selected.iter().copied().collect();
                let ordered: Vec<_> =
                    self.snapshot.entries.iter().filter(|e| selected.contains(&e.id)).map(|e| e.id).collect();
                if ordered != self.selected {
                    return Err(conflict("Invalid move selection or order"));
                }
                Ok(())
            }
            _ => Err(conflict("Invalid move target or selection")),
        }
    }

    fn outbox(&self, paths: &CollectionPaths) -> PathBuf {
        paths.collection_dir(self.source).join("outbox").join(format!("{}.move", self.id))
    }

    fn inbox(&self, paths: &CollectionPaths) -> PathBuf {
        paths.collection_dir(self.destination).join("inbox").join(format!("{}.move", self.id))
    }

    /// Called by the source owner. Persist the intent before publishing it.
    /// A retry uses the same ID; it must never replace a different request.
    pub(crate) fn stage(&self, paths: &CollectionPaths, source: &LoadedPlaylist) -> io::Result<()> {
        self.validate()?;
        let disk = PlaylistStore::new(paths, self.source, self.snapshot.id).load()?;
        if source.externally_edited
            || disk.externally_edited
            || source.manifest != self.snapshot
            || disk.manifest != self.snapshot
            || !disk.same_disk_version(source)
        {
            return Err(conflict("Source playlist changed before move"));
        }
        persist(paths.collection_dir(self.source), self.outbox(paths), &serde_json::to_vec(self)?)
    }

    /// Called by the source owner, including after a crash between stage and
    /// publication. The destination's normal copy importer ignores .move files.
    pub(crate) fn publish(&self, paths: &CollectionPaths) -> io::Result<()> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if fs::read(self.outbox(paths))? != bytes {
            return Err(conflict("Move intent changed"));
        }
        if self.rejected(paths)? {
            return Ok(());
        }
        let root = paths.collection_dir(self.destination);
        if !paths.collection_manifest(self.destination).is_file() {
            return Err(conflict("Destination collection missing"));
        }
        let receipt = self.inbox(paths).with_extension("move-done");
        match fs::read(receipt) {
            Ok(previous) if previous == bytes => return Ok(()),
            Ok(_) => return Err(conflict("Move receipt conflicts with request")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        persist(root, self.inbox(paths), &bytes)
    }

    /// Validate the authoritative destination receipt, not just its existence.
    pub(crate) fn has_receipt(&self, paths: &CollectionPaths) -> io::Result<bool> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        match fs::read(self.inbox(paths).with_extension("move-done")) {
            Ok(receipt) if receipt == bytes => {
                if self.rejected(paths)? {
                    return Err(conflict("Move has conflicting import and rejection records"));
                }
                Ok(true)
            }
            Ok(_) => Err(conflict("Move receipt conflicts with request")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    pub(crate) fn source_completed(&self, paths: &CollectionPaths) -> io::Result<bool> {
        let bytes = serde_json::to_vec(self)?;
        match fs::read(self.outbox(paths).with_extension("source-done")) {
            Ok(done) if done == bytes => Ok(true),
            Ok(_) => Err(conflict("Move completion conflicts with request")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Wake the source owner only after its original intent and our durable
    /// receipt match. Keep the receipt on the destination for idempotent retry.
    /// A crash before publishing the wake-up leaves the Inbox request intact.
    pub(crate) fn notify_source(&self, paths: &CollectionPaths) -> io::Result<()> {
        let bytes = serde_json::to_vec(self)?;
        if !self.has_receipt(paths)? || fs::read(self.outbox(paths))? != bytes {
            return Err(conflict("Cannot confirm the original move request"));
        }
        if !paths.collection_manifest(self.source).is_file() {
            return Err(conflict("Source collection no longer exists"));
        }
        persist(paths.collection_dir(self.source), self.outbox(paths).with_extension("move-ack"), &bytes)?;
        let inbox = self.inbox(paths);
        let intent = inbox.with_extension("move-import");
        if self.import_started(paths)? {
            fs::remove_file(intent)?;
        }
        match fs::read(&inbox) {
            Ok(actual) if actual == bytes => fs::remove_file(&inbox)?,
            Ok(_) => return Err(conflict("Move request changed")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        sync_directory(inbox.parent().unwrap())
    }

    /// The published request must have a matching durable source intent.
    pub(crate) fn verify_source_intent(&self, paths: &CollectionPaths) -> io::Result<()> {
        self.validate()?;
        if fs::read(self.outbox(paths))? != serde_json::to_vec(self)? {
            return Err(conflict("Original move request changed"));
        }
        Ok(())
    }

    /// Recover publication after a crash between staging and sending. Avoid
    /// rewriting or syncing an already published request on every check.
    pub(crate) fn ensure_published(&self, paths: &CollectionPaths) -> io::Result<()> {
        self.verify_source_intent(paths)?;
        let bytes = serde_json::to_vec(self)?;
        match fs::read(self.inbox(paths)) {
            Ok(actual) if actual == bytes => Ok(()),
            Ok(_) => Err(conflict("Published move request changed")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => self.publish(paths),
            Err(err) => Err(err),
        }
    }

    pub(crate) fn expected_source_after_items(&self) -> io::Result<PlaylistManifest> {
        if !matches!(self.target, MoveTarget::Playlist(_)) {
            return Err(conflict("Expected an item move"));
        }
        let mut expected = self.snapshot.clone();
        expected.entries.retain(|entry| !self.selected.contains(&entry.id));
        if expected.current.is_some_and(|id| self.selected.contains(&id)) {
            expected.current = None;
            expected.position = None;
        }
        expected.revision = expected.revision.checked_add(1).ok_or_else(|| conflict("Playlist revision overflow"))?;
        Ok(expected)
    }

    pub(crate) fn import_started(&self, paths: &CollectionPaths) -> io::Result<bool> {
        let bytes = serde_json::to_vec(self)?;
        match fs::read(self.inbox(paths).with_extension("move-import")) {
            Ok(intent) if intent == bytes => Ok(true),
            Ok(_) => Err(conflict("Move import intent conflicts with request")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn begin_import(&self, paths: &CollectionPaths) -> io::Result<()> {
        persist(
            paths.collection_dir(self.destination),
            self.inbox(paths).with_extension("move-import"),
            &serde_json::to_vec(self)?,
        )
    }

    pub(crate) fn destination_waits_for_previous_move(&self, paths: &CollectionPaths) -> io::Result<bool> {
        let target = match self.target {
            MoveTarget::Collection => self.snapshot.id,
            MoveTarget::Playlist(id) => id,
        };
        let incoming: Vec<_> = match self.target {
            MoveTarget::Collection => self.snapshot.entries.iter().map(|entry| entry.id).collect(),
            MoveTarget::Playlist(_) => self.selected.clone(),
        };
        pending_source_overlaps(paths, self.destination, target, &incoming)
    }

    fn marker_matches(&self, path: PathBuf) -> io::Result<bool> {
        match fs::read(path) {
            Ok(bytes) if bytes == serde_json::to_vec(self)? => Ok(true),
            Ok(_) => Err(conflict("Move cancellation record conflicts with request")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    pub(crate) fn cancellation_requested(&self, paths: &CollectionPaths) -> io::Result<bool> {
        self.marker_matches(self.inbox(paths).with_extension("move-cancel"))
    }

    pub(crate) fn rejected(&self, paths: &CollectionPaths) -> io::Result<bool> {
        self.marker_matches(self.inbox(paths).with_extension("move-rejected"))
    }

    fn rejection_delivered(&self, paths: &CollectionPaths) -> io::Result<bool> {
        Ok(self.marker_matches(self.outbox(paths).with_extension("move-rejected"))? && self.rejected(paths)?)
    }

    // Source owner only. Publishing a cancellation is a request, not a decision.
    pub(crate) fn request_cancel(&self, paths: &CollectionPaths) -> io::Result<bool> {
        self.verify_source_intent(paths)?;
        if self.has_receipt(paths)? || self.import_started(paths)? {
            return Ok(false);
        }
        if self.rejected(paths)? {
            return Ok(true);
        }
        self.ensure_published(paths)?;
        persist(
            paths.collection_dir(self.destination),
            self.inbox(paths).with_extension("move-cancel"),
            &serde_json::to_vec(self)?,
        )?;
        Ok(true)
    }

    // Destination owner only. Never reject a request after its import intent
    // was recorded, even if a crash happened before any media was written.
    pub(crate) fn reject(&self, paths: &CollectionPaths) -> io::Result<bool> {
        self.verify_source_intent(paths)?;
        let bytes = serde_json::to_vec(self)?;
        if fs::read(self.inbox(paths))? != bytes {
            return Err(conflict("Move request changed"));
        }
        if self.has_receipt(paths)? || self.import_started(paths)? {
            return Ok(false);
        }
        persist(paths.collection_dir(self.destination), self.inbox(paths).with_extension("move-rejected"), &bytes)?;
        // Retry delivery on destination recovery if this second write fails.
        persist(paths.collection_dir(self.source), self.outbox(paths).with_extension("move-rejected"), &bytes)?;
        Ok(true)
    }

    /// Destination owner only, under its exclusive lock. The session must
    /// reconcile runtime drafts before exposing the committed state to the UI.
    /// A matching receipt is terminal, even if the destination later changes.
    pub(crate) fn import_destination(
        &self,
        paths: &CollectionPaths,
        collection: &mut super::CollectionManifest,
    ) -> io::Result<Option<LoadedPlaylist>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if collection.id != self.destination {
            return Err(conflict("Wrong destination collection"));
        }
        let path = self.inbox(paths);
        match fs::read(path.with_extension("move-done")) {
            Ok(receipt) if receipt == bytes => return Ok(None),
            Ok(_) => return Err(conflict("Move receipt conflicts with request")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        if fs::read(&path)? != bytes {
            return Err(conflict("Move request changed"));
        }
        self.verify_source_intent(paths)?;
        if self.rejected(paths)? {
            return Ok(None);
        }
        if self.cancellation_requested(paths)? && !self.import_started(paths)? {
            self.reject(paths)?;
            return Ok(None);
        }
        if self.destination_waits_for_previous_move(paths)? {
            return Err(conflict("The destination is waiting for an earlier move to finish"));
        }
        let started = self.import_started(paths)?;
        let manifest_path = paths.collection_manifest(collection.id);
        let disk: super::CollectionManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        disk.validate().map_err(io::Error::other)?;
        if disk != *collection {
            return Err(conflict("Destination collection changed on disk"));
        }
        let id = match self.target {
            MoveTarget::Playlist(id) => id,
            MoveTarget::Collection => self.snapshot.id,
        };
        let store = PlaylistStore::new(paths, collection.id, id);
        store.recover()?;
        let loaded = match self.target {
            MoveTarget::Collection => {
                let mut expected = self.snapshot.clone();
                // Preserve the name as well as IDs. Conflicting names require
                // explicit resolution rather than changing the immutable intent.
                for &other in &collection.playlist_order {
                    if other != id
                        && PlaylistStore::new(paths, collection.id, other)
                            .load()?
                            .manifest
                            .name
                            .eq_ignore_ascii_case(&expected.name)
                    {
                        return Err(conflict("Move destination already has a playlist with this name"));
                    }
                }
                expected.revision = 1;
                let loaded = if collection.playlist_order.contains(&id)
                    || paths.playlist_manifest(collection.id, id).try_exists()?
                    || paths.playlist_m3u8(collection.id, id).try_exists()?
                {
                    if !started {
                        return Err(conflict("The destination already contains this playlist"));
                    }
                    let loaded = store.load()?;
                    if loaded.externally_edited || loaded.manifest != expected {
                        return Err(conflict("Move conflicts with existing playlist"));
                    }
                    loaded
                } else {
                    self.begin_import(paths)?;
                    store.create(expected)?
                };
                if !collection.playlist_order.contains(&id) {
                    let mut next = collection.clone();
                    next.playlist_order.push(id);
                    write_atomic(&manifest_path, &serde_json::to_vec_pretty(&next)?)?;
                    *collection = next;
                }
                loaded
            }
            MoveTarget::Playlist(_) => {
                if !collection.playlist_order.contains(&id) {
                    return Err(conflict("Move destination playlist missing"));
                }
                let mut loaded = store.load()?;
                if loaded.externally_edited {
                    return Err(conflict("Move destination changed on disk"));
                }
                let entries: Vec<_> =
                    self.snapshot.entries.iter().filter(|e| self.selected.contains(&e.id)).cloned().collect();
                let existing: std::collections::HashMap<_, _> =
                    loaded.manifest.entries.iter().map(|e| (e.id, e)).collect();
                let present = entries.iter().filter(|e| existing.contains_key(&e.id)).count();
                if present == 0 {
                    self.begin_import(paths)?;
                    loaded.manifest.entries.extend(entries);
                    store.save(&mut loaded)?;
                } else if !started
                    || present != entries.len()
                    || entries.iter().any(|e| existing.get(&e.id).copied() != Some(e))
                {
                    return Err(conflict("Move conflicts with existing entries"));
                }
                loaded
            }
        };
        self.acknowledge(paths)?;
        Ok(Some(loaded))
    }

    /// Destination owner only, after durable import. Check actual disk state,
    /// not a runtime draft. The receipt contains the complete immutable intent.
    pub(crate) fn acknowledge(&self, paths: &CollectionPaths) -> io::Result<()> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        let request = self.inbox(paths);
        if fs::read(&request)? != bytes {
            return Err(conflict("Move request changed"));
        }
        if !self.import_started(paths)? {
            return Err(conflict("Move import has not started"));
        }
        let id = match self.target {
            MoveTarget::Playlist(id) => id,
            MoveTarget::Collection => self.snapshot.id,
        };
        let collection: super::CollectionManifest =
            serde_json::from_slice(&fs::read(paths.collection_manifest(self.destination))?)?;
        collection.validate().map_err(io::Error::other)?;
        if collection.id != self.destination || !collection.playlist_order.contains(&id) {
            return Err(conflict("Move destination not linked"));
        }
        let disk = PlaylistStore::new(paths, self.destination, id).load()?;
        if disk.externally_edited {
            return Err(conflict("Move destination changed on disk"));
        }
        let matches = match self.target {
            MoveTarget::Collection => {
                let mut expected = self.snapshot.clone();
                expected.revision = disk.manifest.revision;
                expected == disk.manifest
            }
            MoveTarget::Playlist(_) => self
                .snapshot
                .entries
                .iter()
                .filter(|e| self.selected.contains(&e.id))
                .all(|e| disk.manifest.entries.iter().any(|actual| actual == e)),
        };
        if !matches {
            return Err(conflict("Move destination does not contain expected data"));
        }
        sync_directory(&paths.playlists_dir(self.destination))?;
        sync_directory(&paths.collection_dir(self.destination))?;
        persist(paths.collection_dir(self.destination), request.with_extension("move-done"), &bytes)
    }

    /// Complete an item move under the source collection's exclusive ownership.
    /// Call only after checking that no live draft or playback uses the source.
    /// The playlist store's pending journal protects the two-file commit.
    /// An exact post-move revision is accepted on retry, never a newer edit.
    pub(crate) fn cleanup_source_items(&self, paths: &CollectionPaths) -> io::Result<bool> {
        self.validate()?;
        if !matches!(self.target, MoveTarget::Playlist(_)) {
            return Err(conflict("Expected an item move"));
        }
        let bytes = serde_json::to_vec(self)?;
        let outbox = self.outbox(paths);
        if fs::read(&outbox)? != bytes {
            return Err(conflict("Move intent changed"));
        }
        if !self.has_receipt(paths)? {
            return Ok(false);
        }
        let done = outbox.with_extension("source-done");
        match fs::read(&done) {
            Ok(previous) if previous == bytes => return Ok(true),
            Ok(_) => return Err(conflict("Move completion conflicts with request")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let collection: super::CollectionManifest =
            serde_json::from_slice(&fs::read(paths.collection_manifest(self.source))?)?;
        collection.validate().map_err(io::Error::other)?;
        if collection.id != self.source || !collection.playlist_order.contains(&self.snapshot.id) {
            return Err(conflict("Source playlist is no longer in the collection"));
        }
        let store = PlaylistStore::new(paths, self.source, self.snapshot.id);
        store.recover()?;
        let mut loaded = store.load()?;
        let expected = self.expected_source_after_items()?;
        if loaded.externally_edited {
            return Err(conflict("Source playlist changed on disk"));
        }
        if loaded.manifest == self.snapshot {
            loaded.manifest = expected.clone();
            store.save(&mut loaded)?;
        }
        if loaded.manifest != expected {
            return Err(conflict("Source playlist changed after the move was requested"));
        }
        sync_directory(&paths.playlists_dir(self.source))?;
        persist(paths.collection_dir(self.source), done, &bytes)?;
        Ok(true)
    }

    /// Remove collection membership only after a matching durable receipt.
    /// Delete only the exact playlist pair recorded before unlinking.
    /// A durable cleanup intent permits retry after either file is removed.
    /// Caller holds the source lock and has checked the live workspace.
    pub(crate) fn cleanup_source_playlist(
        &self,
        paths: &CollectionPaths,
        collection: &mut super::CollectionManifest,
    ) -> io::Result<bool> {
        self.validate()?;
        if self.target != MoveTarget::Collection || collection.id != self.source {
            return Err(conflict("Wrong source collection or move target"));
        }
        let bytes = serde_json::to_vec(self)?;
        let outbox = self.outbox(paths);
        if fs::read(&outbox)? != bytes {
            return Err(conflict("Move intent changed"));
        }
        if !self.has_receipt(paths)? {
            return Ok(false);
        }
        let done = outbox.with_extension("source-done");
        match fs::read(&done) {
            Ok(previous) if previous == bytes => {
                let intent = outbox.with_extension("source-cleanup");
                match fs::remove_file(&intent) {
                    Ok(()) => sync_directory(intent.parent().unwrap())?,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
                return Ok(true);
            }
            Ok(_) => return Err(conflict("Move completion conflicts with request")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let path = paths.collection_manifest(self.source);
        let disk: super::CollectionManifest = serde_json::from_slice(&fs::read(&path)?)?;
        disk.validate().map_err(io::Error::other)?;
        if disk != *collection {
            return Err(conflict("Source collection changed on disk"));
        }
        let id = self.snapshot.id;
        let json = paths.playlist_manifest(self.source, id);
        let m3u = paths.playlist_m3u8(self.source, id);
        let intent = outbox.with_extension("source-cleanup");
        let pair: (Vec<u8>, Vec<u8>) = match fs::read(&intent) {
            Ok(data) => serde_json::from_slice(&data)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let store = PlaylistStore::new(paths, self.source, id);
                store.recover()?;
                let loaded = store.load()?;
                if loaded.externally_edited || loaded.manifest != self.snapshot {
                    return Err(conflict("Source playlist changed after the move was requested"));
                }
                if disk.playlist_order.contains(&id)
                    && (disk.playlist_order.len() <= 1 || disk.active_playlist == Some(id))
                {
                    return Ok(false);
                }
                let pair = (fs::read(&json)?, fs::read(&m3u)?);
                persist(paths.collection_dir(self.source), intent.clone(), &serde_json::to_vec(&pair)?)?;
                pair
            }
            Err(err) => return Err(err),
        };
        let recorded: PlaylistManifest = serde_json::from_slice(&pair.0)?;
        if recorded != self.snapshot || json.with_extension("pending.json").try_exists()? {
            return Err(conflict("Source playlist changed after the move was requested"));
        }
        // Validate both files before changing membership or deleting either.
        for (file, expected) in [(&json, &pair.0), (&m3u, &pair.1)] {
            match fs::read(file) {
                Ok(actual) if actual == *expected => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound && !disk.playlist_order.contains(&id) => {}
                Err(err) => return Err(err),
                Ok(_) => return Err(conflict("Source playlist changed after the move was requested")),
            }
        }
        if disk.playlist_order.contains(&id) {
            if disk.playlist_order.len() <= 1 || disk.active_playlist == Some(id) {
                return Ok(false);
            }
            let mut next = disk;
            next.playlist_order.retain(|entry| *entry != id);
            next.validate().map_err(io::Error::other)?;
            write_atomic(&path, &serde_json::to_vec_pretty(&next)?)?;
            *collection = next;
        }
        // Make the unlink durable before deleting the pair. Missing files
        // are expected only after membership has been removed.
        sync_directory(&paths.collection_dir(self.source))?;
        for (file, expected) in [(&m3u, &pair.1), (&json, &pair.0)] {
            match fs::read(file) {
                Ok(actual) if actual == *expected => fs::remove_file(file)?,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
                Ok(_) => return Err(conflict("Source playlist changed after the move was requested")),
            }
        }
        sync_directory(&paths.playlists_dir(self.source))?;
        persist(paths.collection_dir(self.source), done, &bytes)?;
        fs::remove_file(&intent)?;
        sync_directory(intent.parent().unwrap())?;
        Ok(true)
    }

    /// Source owner only. A matching receipt is necessary, but not sufficient:
    /// session integration must also check runtime state and transact deletion.
    #[cfg(test)]
    pub(crate) fn acknowledged_source_unchanged(&self, paths: &CollectionPaths) -> io::Result<bool> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if fs::read(self.outbox(paths))? != bytes {
            return Err(conflict("Move intent changed"));
        }
        match fs::read(self.inbox(paths).with_extension("move-done")) {
            Ok(receipt) if receipt == bytes => {}
            Ok(_) => return Err(conflict("Move receipt conflicts with request")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        }
        let disk = PlaylistStore::new(paths, self.source, self.snapshot.id).load()?;
        Ok(!disk.externally_edited && disk.manifest == self.snapshot)
    }
}

/// Do not accept a returning identity while its previous outgoing move is
/// still pending, even if the original pair has already been unlinked.
pub(crate) fn pending_source_overlaps(
    paths: &CollectionPaths,
    collection: CollectionId,
    playlist: super::PlaylistId,
    ids: &[PlaylistItemId],
) -> io::Result<bool> {
    let incoming: std::collections::HashSet<_> = ids.iter().copied().collect();
    Ok(requests(paths, collection, false)?.iter().any(|request| {
        request.snapshot.id == playlist
            || match request.target {
                MoveTarget::Collection => request.snapshot.entries.iter().any(|entry| incoming.contains(&entry.id)),
                MoveTarget::Playlist(_) => request.selected.iter().any(|id| incoming.contains(id)),
            }
    }))
}

/// Recently received identities may not be moved again until the previous
/// source has completed removal. This prevents a round-trip from mistaking
/// the original, still-existing playlist for a successful return import.
pub(crate) fn unfinished_import(
    paths: &CollectionPaths,
    collection: CollectionId,
    playlist: super::PlaylistId,
    selected: Option<&[PlaylistItemId]>,
) -> io::Result<bool> {
    let inbox = paths.collection_dir(collection).join("inbox");
    let entries = match fs::read_dir(inbox) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("move-done") {
            continue;
        }
        let bytes = fs::read(&path)?;
        let request: MoveRequest = serde_json::from_slice(&bytes)?;
        request.validate()?;
        if request.destination != collection
            || request.inbox(paths).with_extension("move-done") != path
            || serde_json::to_vec(&request)? != bytes
        {
            return Err(conflict("Invalid move confirmation"));
        }
        let relevant = match request.target {
            MoveTarget::Collection => request.snapshot.id == playlist,
            MoveTarget::Playlist(id) => {
                id == playlist
                    && selected.is_none_or(|selected| request.selected.iter().any(|id| selected.contains(id)))
            }
        };
        if relevant && !request.source_completed(paths)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Read immutable, correctly addressed request files. Invalid requests stay
/// on disk for diagnostics; they cannot become destructive operations.
pub(crate) fn requests(
    paths: &CollectionPaths,
    collection: CollectionId,
    inbound: bool,
) -> io::Result<Vec<MoveRequest>> {
    let root = paths.collection_dir(collection).join(if inbound { "inbox" } else { "outbox" });
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("move") || !entry.file_type()?.is_file() {
            continue;
        }
        // Completed source requests are immutable history. The caller still
        // verifies a matching completion before skipping any loaded request.
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let request: MoveRequest = match serde_json::from_slice(&bytes) {
            Ok(request) => request,
            Err(err) => {
                log::warn!("Invalid transfer request {}: {err}", path.display());
                continue;
            }
        };
        let expected = if inbound { request.inbox(paths) } else { request.outbox(paths) };
        if expected != path
            || (if inbound { request.destination } else { request.source }) != collection
            || request.validate().is_err()
            || serde_json::to_vec(&request)? != bytes
        {
            log::warn!("Invalid transfer request retained: {}", path.display());
            continue;
        }
        if request.rejection_delivered(paths)? || (!inbound && request.source_completed(paths)?) {
            continue;
        }
        result.push(request);
    }
    result.sort_by_key(|request| request.id);
    Ok(result)
}

// One writer per intent/receipt, enforced by the caller's collection lock.
fn persist(root: PathBuf, path: PathBuf, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| conflict("Missing transfer directory"))?;
    fs::create_dir_all(parent)?;
    sync_directory(&root)?;
    match fs::read(&path) {
        Ok(existing) if existing == bytes => sync_directory(parent),
        Ok(_) => Err(conflict("Transfer ID already exists with different contents")),
        Err(err) if err.kind() == io::ErrorKind::NotFound => write_atomic(&path, bytes),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CollectionManifest, PlaylistEntry};
    use super::*;
    use crate::engine::MediaSource;

    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn move_protocol_cancellation_preserves_source_and_replays_notification() {
        let temp = Temp(std::env::temp_dir().join(format!("mav-cancel-{}", Uuid::new_v4())));
        let paths = CollectionPaths::new(&temp.0);
        let mut source = CollectionManifest::new("Source".into());
        let mut destination = CollectionManifest::new("Destination".into());
        let playlist = PlaylistManifest::new("Videos".into());
        source.playlist_order.push(playlist.id);
        let store = PlaylistStore::new(&paths, source.id, playlist.id);
        let loaded = store.create(playlist).unwrap();
        for collection in [&source, &destination] {
            write_atomic(&paths.collection_manifest(collection.id), &serde_json::to_vec(collection).unwrap()).unwrap();
        }
        let request =
            MoveRequest::new(source.id, destination.id, MoveTarget::Collection, loaded.manifest.clone(), vec![])
                .unwrap();
        request.stage(&paths, &loaded).unwrap();
        assert!(request.request_cancel(&paths).unwrap());
        assert_eq!(requests(&paths, source.id, false).unwrap().len(), 1);
        assert!(request.import_destination(&paths, &mut destination).unwrap().is_none());
        assert!(request.rejected(&paths).unwrap());
        assert!(!request.has_receipt(&paths).unwrap());
        assert!(requests(&paths, source.id, false).unwrap().is_empty());
        assert!(requests(&paths, destination.id, true).unwrap().is_empty());
        // Simulate interruption before delivery of the rejection to the source.
        fs::remove_file(request.outbox(&paths).with_extension("move-rejected")).unwrap();
        assert_eq!(requests(&paths, destination.id, true).unwrap().len(), 1);
        assert!(request.reject(&paths).unwrap());
        assert!(requests(&paths, source.id, false).unwrap().is_empty());
        assert!(request.import_destination(&paths, &mut destination).unwrap().is_none());
        assert_eq!(store.load().unwrap().manifest, loaded.manifest);
        assert!(destination.playlist_order.is_empty());
        // A started import is not cancellable, even before writing the pair.
        let next = MoveRequest::new(source.id, destination.id, MoveTarget::Collection, loaded.manifest.clone(), vec![])
            .unwrap();
        next.stage(&paths, &loaded).unwrap();
        next.publish(&paths).unwrap();
        next.begin_import(&paths).unwrap();
        assert!(!next.request_cancel(&paths).unwrap());
        assert!(!next.reject(&paths).unwrap());
        assert!(next.import_destination(&paths, &mut destination).unwrap().is_some());
        assert!(next.has_receipt(&paths).unwrap());
    }

    #[test]
    fn move_protocol_requires_receipt_and_unchanged_source() {
        let temp = Temp(std::env::temp_dir().join(format!("mav-move-{}", Uuid::new_v4())));
        let paths = CollectionPaths::new(&temp.0);
        let mut source = CollectionManifest::new("Source".into());
        let mut destination = CollectionManifest::new("Destination".into());
        let mut playlist = PlaylistManifest::new("Videos".into());
        playlist.entries.push(PlaylistEntry::new(MediaSource::File(
            if cfg!(windows) { "C:\\videos\\a.mp4" } else { "/videos/a.mp4" }.into(),
        )));
        source.playlist_order.push(playlist.id);
        let store = PlaylistStore::new(&paths, source.id, playlist.id);
        let mut loaded = store.create(playlist).unwrap();
        for collection in [&source, &destination] {
            write_atomic(&paths.collection_manifest(collection.id), &serde_json::to_vec(collection).unwrap()).unwrap();
        }
        let request =
            MoveRequest::new(source.id, destination.id, MoveTarget::Collection, loaded.manifest.clone(), vec![])
                .unwrap();
        request.stage(&paths, &loaded).unwrap();
        assert!(!request.acknowledged_source_unchanged(&paths).unwrap());
        request.publish(&paths).unwrap();
        request.publish(&paths).unwrap();
        assert!(request.acknowledge(&paths).is_err());
        // Simulate a crash after the playlist pair was committed but before linking.
        request.begin_import(&paths).unwrap();
        PlaylistStore::new(&paths, destination.id, loaded.manifest.id).create(loaded.manifest.clone()).unwrap();
        let imported = request.import_destination(&paths, &mut destination).unwrap().unwrap();
        assert_eq!(imported.manifest.entries, loaded.manifest.entries);
        assert_eq!(destination.playlist_order, vec![loaded.manifest.id]);
        assert!(request.import_destination(&paths, &mut destination).unwrap().is_none());
        request.acknowledge(&paths).unwrap();
        assert!(request.acknowledged_source_unchanged(&paths).unwrap());
        // A collection must retain at least one playlist.
        assert!(!request.cleanup_source_playlist(&paths, &mut source).unwrap());
        loaded.manifest.name = "Changed".into();
        store.save(&mut loaded).unwrap();
        assert!(!request.acknowledged_source_unchanged(&paths).unwrap());
        assert!(request.cleanup_source_playlist(&paths, &mut source).is_err());
        assert_eq!(store.load().unwrap().manifest, loaded.manifest);
        assert!(source.playlist_order.contains(&loaded.manifest.id));
    }

    #[test]
    fn move_protocol_playlist_cleanup_replays_after_unlink() {
        let temp = Temp(std::env::temp_dir().join(format!("mav-move-{}", Uuid::new_v4())));
        let paths = CollectionPaths::new(&temp.0);
        let mut source = CollectionManifest::new("Source".into());
        let mut destination = CollectionManifest::new("Destination".into());
        let mut playlist = PlaylistManifest::new("Videos".into());
        playlist.entries.push(PlaylistEntry::new(MediaSource::File(
            if cfg!(windows) { "C:\\videos\\a.mp4" } else { "/videos/a.mp4" }.into(),
        )));
        source.playlist_order.push(playlist.id);
        let store = PlaylistStore::new(&paths, source.id, playlist.id);
        let loaded = store.create(playlist).unwrap();
        for collection in [&source, &destination] {
            write_atomic(&paths.collection_manifest(collection.id), &serde_json::to_vec(collection).unwrap()).unwrap();
        }
        let request =
            MoveRequest::new(source.id, destination.id, MoveTarget::Collection, loaded.manifest.clone(), vec![])
                .unwrap();
        request.stage(&paths, &loaded).unwrap();
        assert!(!request.acknowledged_source_unchanged(&paths).unwrap());
        request.publish(&paths).unwrap();
        request.publish(&paths).unwrap();
        assert!(request.acknowledge(&paths).is_err());
        // Simulate a crash after the playlist pair was committed but before linking.
        request.begin_import(&paths).unwrap();
        PlaylistStore::new(&paths, destination.id, loaded.manifest.id).create(loaded.manifest.clone()).unwrap();
        let imported = request.import_destination(&paths, &mut destination).unwrap().unwrap();
        assert_eq!(imported.manifest.entries, loaded.manifest.entries);
        assert_eq!(destination.playlist_order, vec![loaded.manifest.id]);
        assert!(request.import_destination(&paths, &mut destination).unwrap().is_none());
        request.acknowledge(&paths).unwrap();
        assert!(request.acknowledged_source_unchanged(&paths).unwrap());
        // A collection must retain at least one playlist.
        assert!(!request.cleanup_source_playlist(&paths, &mut source).unwrap());
        let other = PlaylistManifest::new("Remaining".into());
        let other_id = other.id;
        PlaylistStore::new(&paths, source.id, other_id).create(other).unwrap();
        source.playlist_order.push(other_id);
        source.active_playlist = Some(other_id);
        write_atomic(&paths.collection_manifest(source.id), &serde_json::to_vec(&source).unwrap()).unwrap();
        let json = paths.playlist_manifest(source.id, loaded.manifest.id);
        let m3u = paths.playlist_m3u8(source.id, loaded.manifest.id);
        let pair = (fs::read(&json).unwrap(), fs::read(&m3u).unwrap());
        persist(
            paths.collection_dir(source.id),
            request.outbox(&paths).with_extension("source-cleanup"),
            &serde_json::to_vec(&pair).unwrap(),
        )
        .unwrap();
        // Simulate durable unlink followed by deletion of only the M3U8.
        source.playlist_order.retain(|id| *id != loaded.manifest.id);
        write_atomic(&paths.collection_manifest(source.id), &serde_json::to_vec(&source).unwrap()).unwrap();
        fs::remove_file(&m3u).unwrap();
        // A newer JSON must not be deleted on recovery.
        fs::write(&json, b"changed").unwrap();
        assert!(request.cleanup_source_playlist(&paths, &mut source).is_err());
        assert_eq!(fs::read(&json).unwrap(), b"changed");
        fs::write(&json, &pair.0).unwrap();
        assert!(request.cleanup_source_playlist(&paths, &mut source).unwrap());
        assert!(!json.exists() && !m3u.exists());
        assert!(request.cleanup_source_playlist(&paths, &mut source).unwrap());
        assert_eq!(source.playlist_order, vec![other_id]);
        // The UUID can be used again after a return move. An old receipt
        // must never delete that new pair.
        store.create(loaded.manifest.clone()).unwrap();
        source.playlist_order.push(loaded.manifest.id);
        write_atomic(&paths.collection_manifest(source.id), &serde_json::to_vec(&source).unwrap()).unwrap();
        assert!(request.cleanup_source_playlist(&paths, &mut source).unwrap());
        assert!(json.exists() && m3u.exists());
    }

    #[test]
    fn move_protocol_item_import_replays_after_commit() {
        let temp = Temp(std::env::temp_dir().join(format!("mav-move-items-{}", Uuid::new_v4())));
        let paths = CollectionPaths::new(&temp.0);
        let mut source = CollectionManifest::new("Source".into());
        let mut destination = CollectionManifest::new("Destination".into());
        let mut playlist = PlaylistManifest::new("Videos".into());
        let mut item = PlaylistEntry::new(MediaSource::File(
            if cfg!(windows) { "C:\\videos\\a.mp4" } else { "/videos/a.mp4" }.into(),
        ));
        item.custom_title = Some("Custom title".into());
        playlist.entries.push(item.clone());
        source.playlist_order.push(playlist.id);
        let source_loaded = PlaylistStore::new(&paths, source.id, playlist.id).create(playlist).unwrap();
        let target = PlaylistManifest::new("Target".into());
        destination.playlist_order.push(target.id);
        let store = PlaylistStore::new(&paths, destination.id, target.id);
        let mut target_loaded = store.create(target).unwrap();
        for collection in [&source, &destination] {
            write_atomic(&paths.collection_manifest(collection.id), &serde_json::to_vec(collection).unwrap()).unwrap();
        }
        let request = MoveRequest::new(
            source.id,
            destination.id,
            MoveTarget::Playlist(target_loaded.manifest.id),
            source_loaded.manifest.clone(),
            vec![item.id],
        )
        .unwrap();
        request.stage(&paths, &source_loaded).unwrap();
        request.publish(&paths).unwrap();
        // Identical data without an import intent is a collision, not recovery.
        target_loaded.manifest.entries.push(item.clone());
        store.save(&mut target_loaded).unwrap();
        assert!(request.import_destination(&paths, &mut destination).is_err());
        assert!(!request.has_receipt(&paths).unwrap());
        target_loaded.manifest.entries.clear();
        store.save(&mut target_loaded).unwrap();
        // Destination committed, but crashed before issuing the receipt.
        request.begin_import(&paths).unwrap();
        target_loaded.manifest.entries.push(item.clone());
        store.save(&mut target_loaded).unwrap();
        assert!(!request.acknowledged_source_unchanged(&paths).unwrap());
        let imported = request.import_destination(&paths, &mut destination).unwrap().unwrap();
        assert_eq!(imported.manifest.entries, vec![item]);
        assert_eq!(imported.manifest.revision, target_loaded.manifest.revision);
        assert!(request.import_destination(&paths, &mut destination).unwrap().is_none());
        assert!(request.acknowledged_source_unchanged(&paths).unwrap());
        assert_eq!(
            PlaylistStore::new(&paths, source.id, source_loaded.manifest.id).load().unwrap().manifest,
            source_loaded.manifest
        );
        // A late cancellation must never undo a committed import.
        assert!(!request.request_cancel(&paths).unwrap());
        assert!(!request.reject(&paths).unwrap());
        // Contradictory terminal records must fail closed, not delete data.
        let rejection = request.inbox(&paths).with_extension("move-rejected");
        persist(paths.collection_dir(destination.id), rejection.clone(), &serde_json::to_vec(&request).unwrap())
            .unwrap();
        assert!(request.cleanup_source_items(&paths).is_err());
        assert_eq!(
            PlaylistStore::new(&paths, source.id, source_loaded.manifest.id).load().unwrap().manifest,
            source_loaded.manifest
        );
        fs::remove_file(rejection).unwrap();
        // A crash before notification must leave the request discoverable.
        assert_eq!(requests(&paths, destination.id, true).unwrap(), vec![request.clone()]);
        request.notify_source(&paths).unwrap();
        assert!(requests(&paths, destination.id, true).unwrap().is_empty());
        // The authoritative receipt survives a lost wake-up marker.
        fs::remove_file(request.outbox(&paths).with_extension("move-ack")).unwrap();
        request.ensure_published(&paths).unwrap();
        assert!(request.has_receipt(&paths).unwrap());
        request.notify_source(&paths).unwrap();
        assert!(request.outbox(&paths).with_extension("move-ack").is_file());
        assert!(request.cleanup_source_items(&paths).unwrap());
        let source_store = PlaylistStore::new(&paths, source.id, source_loaded.manifest.id);
        let cleaned = source_store.load().unwrap();
        assert!(cleaned.manifest.entries.is_empty());
        // Simulate interruption after the source commit, before completion.
        fs::remove_file(request.outbox(&paths).with_extension("source-done")).unwrap();
        assert!(request.cleanup_source_items(&paths).unwrap());
        assert!(request.cleanup_source_items(&paths).unwrap());
        assert_eq!(source_store.load().unwrap().manifest, cleaned.manifest);
    }
}
