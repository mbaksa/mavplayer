//! Named playlist workspaces. The active draft is independent of the last
//! explicitly saved (or autosaved) snapshot of each workspace.

use crate::engine::MediaSource;
use crate::settings::{config_dir, read_json, write_json};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub sources: Vec<MediaSource>,
    pub current: Option<usize>,
    pub position: Option<f64>,
}

impl Snapshot {
    fn normalize(&mut self) {
        self.current = self.current.filter(|&i| i < self.sources.len());
        self.position = self.position.filter(|p| self.current.is_some() && p.is_finite() && *p >= 0.0);
    }

    fn needs_autosave(&self, saved: &Self) -> bool {
        self.sources != saved.sources || self.current != saved.current || match (self.position, saved.position) {
            (Some(a), Some(b)) => (a - b).abs() >= 5.0,
            (a, b) => a != b,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Slot {
    pub name: String,
    pub autosave: bool,
    #[serde(default = "open_by_default")]
    pub open: bool,
    saved: Snapshot,
    #[serde(skip)]
    draft: Option<Snapshot>,
}

fn open_by_default() -> bool { true }

impl Slot {
    fn new(name: String, autosave: bool) -> Self {
        Self { name, autosave, open: true, saved: Snapshot::default(), draft: None }
    }

    fn snapshot(&self) -> &Snapshot { self.draft.as_ref().unwrap_or(&self.saved) }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Library {
    version: u32,
    pub active: usize,
    pub slots: Vec<Slot>,
    #[serde(skip)]
    pub writable: bool,
    #[serde(skip)]
    dirty: bool,
}

impl Library {
    fn empty(writable: bool) -> Self {
        Self { version: 1, active: 0, slots: vec![Slot::new("Default playlist".into(), false)], writable, dirty: false }
    }

    pub fn load(primary: bool) -> Self {
        if !primary { return Self::empty(false); }
        let path = config_dir().join("playlists.json");
        if !path.exists() { return Self::empty(true); }
        let Some(mut library) = read_json::<Self>(&path) else { return Self::empty(false); };
        // Never overwrite an unknown version or a damaged library.
        if library.version != 1 || library.slots.is_empty() { return Self::empty(false); }
        library.active = library.active.min(library.slots.len() - 1);
        for slot in &mut library.slots { slot.saved.normalize(); }
        library.slots[library.active].open = true;
        library.writable = true;
        library
    }

    pub fn snapshot(&self) -> &Snapshot { self.slots[self.active].snapshot() }
    pub fn name(&self) -> &str { &self.slots[self.active].name }
    pub fn autosave(&self) -> bool { self.slots[self.active].autosave }

    pub fn capture(&mut self, mut snapshot: Snapshot, exact: bool, manual: bool) {
        snapshot.normalize();
        let slot = &mut self.slots[self.active];
        if (manual || (slot.autosave && (exact || snapshot.needs_autosave(&slot.saved)))) && slot.saved != snapshot {
            slot.saved = snapshot.clone();
            self.dirty = true;
        }
        slot.draft = Some(snapshot);
    }

    pub fn set_autosave(&mut self, enabled: bool) {
        if self.slots[self.active].autosave != enabled {
            self.slots[self.active].autosave = enabled;
            self.dirty = true;
        }
    }

    pub fn switch(&mut self, index: usize) -> bool {
        if index == self.active || index >= self.slots.len() { return false; }
        self.active = index;
        self.slots[index].open = true;
        self.dirty = true;
        true
    }

    pub fn can_close(&self) -> bool {
        self.active != 0 || self.slots.iter().enumerate().any(|(i, s)| i != self.active && s.open)
    }

    pub fn close_active(&mut self) -> bool {
        if !self.can_close() { return false; }
        self.slots[self.active].open = false;
        self.active = self.slots.iter().position(|s| s.open).unwrap_or(0);
        self.slots[self.active].open = true;
        self.dirty = true;
        true
    }

    pub fn valid_name(&self, name: &str, rename: bool) -> bool {
        !name.trim().is_empty() && !self.slots.iter().enumerate().any(|(i, s)| (!rename || i != self.active) && s.name.eq_ignore_ascii_case(name.trim()))
    }

    pub fn suggested_name(&self, base: &str) -> String {
        if self.valid_name(base, false) { return base.into(); }
        (2..).map(|i| format!("{base} {i}")).find(|name| self.valid_name(name, false)).unwrap()
    }

    pub fn create(&mut self, name: String) {
        self.slots.push(Slot::new(name, self.writable));
        self.active = self.slots.len() - 1;
        self.dirty = true;
    }

    pub fn rename(&mut self, name: String) {
        self.slots[self.active].name = name;
        self.dirty = true;
    }

    /// Explicitly launched media must not replace an already saved playlist.
    /// This temporary workspace is not written unless the user saves or edits
    /// the library (e.g. switches to another playlist or enables autosave).
    pub fn start_launch_playlist(&mut self) {
        if self.snapshot().sources.is_empty() { return; }
        let name = self.suggested_name("Launch playlist");
        self.slots.push(Slot::new(name, false));
        self.active = self.slots.len() - 1;
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        if !self.writable || !self.dirty { return Ok(()); }
        write_json(&config_dir().join("playlists.json"), self)?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &str, position: f64) -> Snapshot {
        Snapshot { sources: vec![MediaSource::File(name.into())], current: Some(0), position: Some(position) }
    }

    #[test]
    fn playlist_workspaces_preserve_independent_drafts_and_saved_positions() {
        let mut lib = Library::empty(true);
        lib.set_autosave(true);
        lib.capture(snapshot("first.mp4", 12.5), true, false);
        lib.create("Second".into());
        lib.capture(snapshot("first.mp4", 70.0), true, false);
        lib.set_autosave(false);
        lib.capture(snapshot("unsaved.mp4", 23.0), true, false);
        assert!(lib.switch(0));
        assert_eq!(lib.snapshot(), &snapshot("first.mp4", 12.5));
        assert!(lib.switch(1));
        assert_eq!(lib.snapshot(), &snapshot("unsaved.mp4", 23.0));
        let mut restored: Library = serde_json::from_str(&serde_json::to_string(&lib).unwrap()).unwrap();
        assert_eq!(restored.active, 1);
        assert_eq!(restored.snapshot(), &snapshot("first.mp4", 70.0));
        assert!(!restored.autosave());
        assert!(restored.switch(0));
        assert_eq!(restored.snapshot(), &snapshot("first.mp4", 12.5));
    }

    #[test]
    fn closing_playlist_preserves_saved_contents() {
        let mut lib = Library::empty(true);
        lib.create("Second".into());
        lib.capture(snapshot("movie.mp4", 42.), true, false);
        assert!(lib.close_active());
        assert_eq!(lib.active, 0);
        assert!(!lib.slots[1].open);
        let mut restored: Library = serde_json::from_str(&serde_json::to_string(&lib).unwrap()).unwrap();
        assert!(!restored.slots[1].open);
        assert!(restored.switch(1));
        assert_eq!(restored.snapshot(), &snapshot("movie.mp4", 42.));
    }

    #[test]
    fn launching_media_does_not_overwrite_saved_playlist() {
        let mut lib = Library::empty(true);
        lib.capture(snapshot("saved.mp4", 10.0), true, true);
        lib.dirty = false;
        lib.start_launch_playlist();
        assert!(!lib.autosave());
        lib.capture(snapshot("launch.mp4", 0.0), true, false);
        assert!(!lib.dirty);
        assert!(lib.switch(0));
        assert_eq!(lib.snapshot(), &snapshot("saved.mp4", 10.0));
    }
}
