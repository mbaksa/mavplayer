//! The play queue: ordering, shuffle/repeat, folder expansion and
//! M3U / PLS import & export.

pub(crate) mod session;

use crate::engine::MediaSource;
use crate::engine::disc;
use crate::settings::RepeatMode;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "webm", "wmv", "flv", "mpg", "mpeg", "m2v", "ts", "m2ts", "mts", "vob", "ogv",
    "3gp", "3g2", "asf", "divx", "f4v", "rm", "rmvb", "mxf", "y4m", "dv", "nut", "ivf", "hevc", "h264", "264", "265",
    "av1", "obu", "evo", "mk3d", "bik", "gif", "wtv", "dvr-ms",
];
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "wma", "ape", "wv", "mka", "ac3", "eac3", "dts", "aiff",
    "aif", "alac", "mpc", "tta", "amr", "au", "caf", "spx", "mid", "midi", "mod", "xm", "it", "s3m", "dsf", "dff",
    "w64", "voc", "mp2", "m4b", "thd", "mlp", "tak", "shn",
];
pub const PLAYLIST_EXTS: &[&str] = &["m3u", "m3u8", "pls"];

pub fn is_media_path(p: &Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    VIDEO_EXTS.contains(&ext.as_str()) || AUDIO_EXTS.contains(&ext.as_str()) || ext == "iso"
}

#[derive(Clone, Debug)]
pub struct PlaylistItem {
    pub id: u64,
    pub source: MediaSource,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration: Option<f64>,
    pub video_stream: Option<usize>,
    pub probed: bool,
    pub failed: bool,
}

impl PlaylistItem {
    pub fn display_title(&self) -> String {
        match (&self.title, &self.artist) {
            (Some(t), Some(a)) => format!("{a} – {t}"),
            (Some(t), None) => t.clone(),
            _ => self.source.display_name(),
        }
    }
}

#[derive(Default)]
pub struct Selection {
    ids: HashSet<u64>,
    anchor: Option<u64>,
}

impl Selection {
    pub fn clear(&mut self) { self.ids.clear(); self.anchor = None; }
    pub fn contains(&self, id: u64) -> bool { self.ids.contains(&id) }
    pub fn ids(&self, items: &[PlaylistItem]) -> Vec<u64> {
        items.iter().filter(|it| self.contains(it.id)).map(|it| it.id).collect()
    }
    pub fn single(&mut self, id: u64) {
        self.ids.clear();
        self.ids.insert(id);
        self.anchor = Some(id);
    }
    pub fn select_if_empty(&mut self, items: &[PlaylistItem], index: usize) {
        if !items.iter().any(|it| self.contains(it.id)) && let Some(it) = items.get(index) { self.single(it.id); }
    }
    pub fn click(&mut self, items: &[PlaylistItem], index: usize, control: bool, shift: bool) {
        let Some(item) = items.get(index) else { return; };
        if shift {
            let anchor = self.anchor.and_then(|id| items.iter().position(|it| it.id == id)).unwrap_or(index);
            if !control { self.ids.clear(); }
            self.ids.extend(items[anchor.min(index)..=anchor.max(index)].iter().map(|it| it.id));
            self.anchor = Some(items[anchor].id);
        } else if control {
            if !self.ids.remove(&item.id) { self.ids.insert(item.id); }
            self.anchor = Some(item.id);
        } else {
            self.single(item.id);
        }
    }
    pub fn remove(&mut self, ids: &[u64]) {
        self.ids.retain(|id| !ids.contains(id));
        if self.anchor.is_some_and(|id| ids.contains(&id)) { self.anchor = None; }
    }
}

pub struct Playlist {
    pub items: Vec<PlaylistItem>,
    pub current: Option<usize>,
    pub repeat: RepeatMode,
    shuffle: bool,
    order: Vec<usize>,
    next_id: u64,
    rng: u64,
}

impl Playlist {
    pub fn new(repeat: RepeatMode, shuffle: bool) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(42);
        Self { items: vec![], current: None, repeat, shuffle, order: vec![], next_id: 1, rng: seed | 1 }
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545F4914F6CDD1D)
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        self.rebuild_order();
    }

    fn rebuild_order(&mut self) {
        let n = self.items.len();
        self.order = (0..n).collect();
        if self.shuffle {
            for i in (1..n).rev() {
                let j = (self.rand() % (i as u64 + 1)) as usize;
                self.order.swap(i, j);
            }
            // Keep the current item first so "next" continues through the rest.
            if let Some(c) = self.current
                && let Some(pos) = self.order.iter().position(|&x| x == c)
            {
                self.order.remove(pos);
                self.order.insert(0, c);
            }
        }
    }

    pub fn add(&mut self, sources: Vec<MediaSource>) -> std::ops::Range<usize> {
        self.insert(self.items.len(), sources)
    }

    /// Inserts `sources` before index `at` (clamped to the end) and returns
    /// the indices they now occupy.
    pub fn insert(&mut self, at: usize, sources: Vec<MediaSource>) -> std::ops::Range<usize> {
        let start = at.min(self.items.len());
        let new: Vec<PlaylistItem> = sources
            .into_iter()
            .map(|s| {
                let id = self.next_id;
                self.next_id += 1;
                PlaylistItem { id, source: s, title: None, artist: None, duration: None, video_stream: None, probed: false, failed: false }
            })
            .collect();
        let n = new.len();
        let end = start + n;
        self.items.splice(start..start, new);
        let shift = |i: usize| if i >= start { i + n } else { i };
        self.current = self.current.map(shift);
        if self.shuffle {
            for x in self.order.iter_mut() {
                *x = shift(*x);
            }
            // Insert the new items at random positions after the current one.
            let cur_pos =
                self.current.and_then(|c| self.order.iter().position(|&x| x == c)).map(|p| p + 1).unwrap_or(0);
            for i in start..end {
                let span = (self.order.len() - cur_pos + 1) as u64;
                let at = cur_pos + (self.rand() % span) as usize;
                self.order.insert(at.min(self.order.len()), i);
            }
        } else {
            self.order = (0..self.items.len()).collect();
        }
        start..end
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.order.clear();
        self.current = None;
    }

    pub fn remove(&mut self, idx: usize) {
        if idx >= self.items.len() {
            return;
        }
        self.items.remove(idx);
        self.current = match self.current {
            Some(c) if c == idx => None,
            Some(c) if c > idx => Some(c - 1),
            c => c,
        };
        self.order.retain(|&x| x != idx);
        for x in self.order.iter_mut() {
            if *x > idx {
                *x -= 1;
            }
        }
    }

    /// Move selected rows one step without changing their relative order.
    pub fn can_move_selected(&self, ids: &[u64], up: bool) -> bool {
        self.items.iter().enumerate().any(|(i, item)| ids.contains(&item.id) &&
            if up { i > 0 && !ids.contains(&self.items[i - 1].id) }
            else { i + 1 < self.items.len() && !ids.contains(&self.items[i + 1].id) })
    }

    pub fn move_selected(&mut self, ids: &[u64], up: bool) {
        if up {
            for i in 1..self.items.len() {
                if ids.contains(&self.items[i].id) && !ids.contains(&self.items[i - 1].id) { self.move_item(i, i - 1); }
            }
        } else {
            for i in (0..self.items.len().saturating_sub(1)).rev() {
                if ids.contains(&self.items[i].id) && !ids.contains(&self.items[i + 1].id) { self.move_item(i, i + 1); }
            }
        }
    }

    /// Drop a selected group at an insertion point in the original list.
    /// Translate the grabbed row's displacement to the leading edge of the group.
    pub fn group_drop_gap(&self, ids: &[u64], anchor: u64, target_row: usize) -> Option<usize> {
        let from = self.items.iter().position(|it| it.id == anchor)?;
        let first = self.items.iter().position(|it| ids.contains(&it.id))?;
        let last = self.items.iter().rposition(|it| ids.contains(&it.id))?;
        let target_row = target_row.min(self.items.len().saturating_sub(1));
        match target_row.cmp(&from) {
            std::cmp::Ordering::Less if first > 0 => Some(first.saturating_sub(from - target_row)),
            std::cmp::Ordering::Greater if last + 1 < self.items.len() => Some((last + 1).saturating_add(target_row - from).min(self.items.len())),
            _ => None,
        }
    }

    pub fn drop_selected(&mut self, ids: &[u64], at: usize) {
        let at = at.min(self.items.len());
        let before = self.items[..at].iter().filter(|it| !ids.contains(&it.id)).count();
        let mut order: Vec<_> = self.items.iter().filter(|it| !ids.contains(&it.id)).map(|it| it.id).collect();
        order.splice(before..before, self.items.iter().filter(|it| ids.contains(&it.id)).map(|it| it.id));
        for (to, id) in order.into_iter().enumerate() {
            if self.items[to].id != id && let Some(from) = self.items.iter().position(|it| it.id == id) { self.move_item(from, to); }
        }
    }

    pub fn move_item(&mut self, from: usize, to: usize) {
        if from >= self.items.len() || to >= self.items.len() || from == to {
            return;
        }
        let item = self.items.remove(from);
        self.items.insert(to, item);
        let map = |i: usize| -> usize {
            if i == from {
                to
            } else if from < to && i > from && i <= to {
                i - 1
            } else if from > to && i >= to && i < from {
                i + 1
            } else {
                i
            }
        };
        self.current = self.current.map(map);
        if self.shuffle {
            for x in self.order.iter_mut() {
                *x = map(*x);
            }
        } else {
            self.order = (0..self.items.len()).collect();
        }
    }

    /// Index of the item after the current one. `user` = explicit "next".
    pub fn next_index(&self, user: bool) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        if self.repeat == RepeatMode::One && !user {
            return self.current;
        }
        let pos = self.current.and_then(|c| self.order.iter().position(|&x| x == c));
        match pos {
            None => self.order.first().copied(),
            Some(p) if p + 1 < self.order.len() => Some(self.order[p + 1]),
            Some(_) if self.repeat != RepeatMode::Off => self.order.first().copied(),
            Some(_) => None,
        }
    }

    pub fn prev_index(&self) -> Option<usize> {
        let pos = self.current.and_then(|c| self.order.iter().position(|&x| x == c))?;
        if pos > 0 {
            Some(self.order[pos - 1])
        } else if self.repeat != RepeatMode::Off {
            self.order.last().copied()
        } else {
            Some(self.order[0])
        }
    }

    pub fn save_m3u(&self, path: &Path) -> std::io::Result<()> {
        // Titles come from file metadata: a line break in one would start
        // a bogus entry when the playlist is read back.
        let one_line = |t: &str| t.replace(['\r', '\n'], " ");
        let mut s = String::from("#EXTM3U\n");
        for it in &self.items {
            let entry = match &it.source {
                MediaSource::File(p) => p.display().to_string(),
                MediaSource::Url(u) => u.clone(),
                MediaSource::Dvd { path, title, .. } => format!("dvd://{title}/{path}"),
                MediaSource::Cd { device, track } => format!("cdda://{track}/{device}"),
            };
            if entry.contains(['\r', '\n']) {
                continue; // can't be represented in M3U
            }
            let dur = it.duration.map(|d| d.round() as i64).unwrap_or(-1);
            s.push_str(&format!("#EXTINF:{dur},{}\n{entry}\n", one_line(&it.display_title())));
        }
        std::fs::write(path, s)
    }
}

fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = x.to_lowercase().cmp(y.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

fn scan_dir(dir: &Path, out: &mut Vec<MediaSource>, depth: u32, seen: &mut HashSet<PathBuf>) {
    // A symlink back up the tree would otherwise add everything again at
    // every level down to the depth limit.
    if !seen.insert(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())) {
        return;
    }
    if let Some(dvd) = disc::is_disc_source(dir) {
        out.push(dvd);
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    for p in entries {
        if p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false) {
            continue;
        }
        if p.is_dir() {
            if depth < 8 {
                scan_dir(&p, out, depth + 1, seen);
            }
        } else if is_media_path(&p) {
            out.push(source_for_path(&p));
        } else if crate::engine::cdda::is_cue(&p)
            && let Some(cd) = disc::is_disc_source(&p)
        {
            out.push(cd);
        }
    }
}

pub fn source_for_path(p: &Path) -> MediaSource {
    disc::is_disc_source(p).unwrap_or_else(|| MediaSource::File(p.to_path_buf()))
}

/// Parses a string the user typed or passed on the command line.
pub fn source_for_arg(arg: &str) -> MediaSource {
    let lower = arg.to_ascii_lowercase();
    if lower.starts_with("dvd://") {
        let rest = &arg[6..];
        let (title, path) = match rest.split_once('/') {
            Some((t, p)) if t.chars().all(|c| c.is_ascii_digit()) => (t.parse().unwrap_or(0), p.to_string()),
            _ if rest.chars().all(|c| c.is_ascii_digit()) => (rest.parse().unwrap_or(0), default_dvd_device()),
            _ => (0, rest.to_string()),
        };
        let path = if path.is_empty() {
            default_dvd_device()
        } else if cfg!(unix) && !path.starts_with('/') {
            format!("/{path}")
        } else {
            path
        };
        return MediaSource::Dvd { path, title, angle: 1 };
    }
    if lower.starts_with("cdda://") {
        return cdda_source(&arg[7..]);
    }
    if let Some(p) = file_uri_path(arg) {
        return source_for_path(&p);
    }
    if lower.contains("://") {
        return MediaSource::Url(arg.to_string());
    }
    source_for_path(Path::new(arg))
}

/// `cdda://[TRACK][/DEVICE]` (track 0 or none: the whole disc), plus the
/// desktop's `cdda://sr0/…` form (GIO), which names the drive first.
fn cdda_source(rest: &str) -> MediaSource {
    let rest = crate::engine::percent_decode(rest);
    let (head, tail) = rest.split_once('/').unwrap_or((&rest, ""));
    let is_drive_name = |h: &str| h.starts_with("sr") && h.len() > 2 && h[2..].chars().all(|c| c.is_ascii_digit());
    let (track, device) = if head.chars().all(|c| c.is_ascii_digit()) {
        (head.parse().unwrap_or(0), tail.to_string())
    } else if is_drive_name(head) {
        // "Track 3.wav" → 3
        let n: String = tail.chars().filter(|c| c.is_ascii_digit()).collect();
        (n.parse().unwrap_or(0), format!("/dev/{head}"))
    } else {
        (0, rest.to_string())
    };
    let device = if device.is_empty() {
        default_dvd_device()
    } else if cfg!(unix) && !device.starts_with('/') {
        format!("/{device}")
    } else {
        device
    };
    MediaSource::Cd { device, track }
}

pub fn default_dvd_device() -> String {
    disc::optical_drives().first().map(|d| d.path.clone()).unwrap_or_else(|| {
        if cfg!(windows) {
            "D:".into()
        } else if cfg!(target_os = "macos") {
            "/dev/rdisk1".into()
        } else {
            "/dev/sr0".into()
        }
    })
}

/// The local path in a `file://` URI (`file:///x`, `file://localhost/x`),
/// percent-decoded and without any query or fragment.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.get(..7).filter(|p| p.eq_ignore_ascii_case("file://")).map(|_| &uri[7..])?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let rest = match rest.get(..9) {
        Some(h) if h.eq_ignore_ascii_case("localhost") => &rest[9..],
        _ => rest,
    };
    let path = crate::engine::types::percent_decode(rest);
    // file:///C:/Videos/x.mkv on Windows.
    #[cfg(windows)]
    let path = match path.as_bytes() {
        [b'/', d, b':', ..] if d.is_ascii_alphabetic() => path[1..].to_string(),
        _ => path,
    };
    Some(PathBuf::from(path))
}

fn is_hls(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|s| s.contains("#EXT-X-")).unwrap_or(false)
}

fn parse_playlist_file(path: &Path) -> Vec<MediaSource> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    let base = path.parent().unwrap_or(Path::new("."));
    let resolve = |entry: &str| -> MediaSource {
        let e = entry.trim();
        if e.contains("://") {
            source_for_arg(e)
        } else {
            let p = Path::new(e);
            source_for_path(&if p.is_absolute() { p.to_path_buf() } else { base.join(p) })
        }
    };
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if ext == "pls" {
        text.lines()
            .filter_map(|l| l.trim().split_once('='))
            .filter(|(k, _)| k.to_ascii_lowercase().starts_with("file"))
            .map(|(_, v)| resolve(v))
            .collect()
    } else {
        text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(resolve).collect()
    }
}

/// Expands files, folders and playlist files into playable sources.
pub fn expand_paths(paths: &[PathBuf]) -> Vec<MediaSource> {
    let mut out = Vec::new();
    for p in paths {
        let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if p.is_dir() {
            scan_dir(p, &mut out, 0, &mut HashSet::new());
        } else if PLAYLIST_EXTS.contains(&ext.as_str()) && !(ext == "m3u8" && is_hls(p)) {
            out.extend(parse_playlist_file(p));
        } else {
            out.push(source_for_path(p));
        }
    }
    out
}

/// Playlist additions accept media, folders and playlist files, but not subtitles
/// or unrelated documents. Opening a subtitle on the video remains separate.
pub fn expand_playlist_paths(paths: &[PathBuf]) -> Vec<MediaSource> {
    expand_paths(paths).into_iter().filter(|source| match source {
        MediaSource::File(path) => is_media_path(path) || path.extension().is_some_and(|e| e.eq_ignore_ascii_case("m3u8")),
        _ => true,
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pl(names: &[&str]) -> Playlist {
        let mut p = Playlist::new(RepeatMode::Off, false);
        p.add(names.iter().map(|n| MediaSource::File(PathBuf::from(n))).collect());
        p
    }

    fn names(p: &Playlist) -> Vec<String> {
        p.items.iter().map(|i| i.source.display_name()).collect()
    }

    #[test]
    fn playlist_multi_selection_survives_group_move() {
        let mut p = pl(&["a", "b", "c", "d", "e"]);
        let mut selection = Selection::default();
        selection.click(&p.items, 1, false, false);
        selection.click(&p.items, 2, false, true);
        selection.click(&p.items, 4, true, false);
        let ids = selection.ids(&p.items);
        assert_eq!(ids, vec![p.items[1].id, p.items[2].id, p.items[4].id]);
        p.current = Some(2);
        p.move_selected(&ids, true);
        assert_eq!(names(&p), ["b", "c", "a", "e", "d"]);
        assert_eq!(p.current, Some(1));
        assert_eq!(selection.ids(&p.items), ids);
        p.move_selected(&ids, false);
        assert_eq!(names(&p), ["a", "b", "c", "d", "e"]);
        assert_eq!(p.current, Some(2));
        selection.click(&p.items, 1, true, false);
        assert!(!selection.contains(p.items[1].id));
    }

    #[test]
    fn playlist_group_drag_moves_relative_to_grabbed_row() {
        for grabbed in 4..8 {
            let mut p = pl(&["a", "b", "c", "d", "e", "f", "g", "h"]);
            p.current = Some(7);
            let ids: Vec<_> = p.items[4..].iter().map(|it| it.id).collect();
            let anchor = p.items[grabbed].id;
            assert_eq!(p.group_drop_gap(&ids, anchor, grabbed), None);
            let at = p.group_drop_gap(&ids, anchor, grabbed - 1).unwrap();
            p.drop_selected(&ids, at);
            assert_eq!(names(&p), ["a", "b", "c", "e", "f", "g", "h", "d"]);
            assert_eq!(p.current, Some(6));
            let at = p.group_drop_gap(&ids, anchor, grabbed).unwrap();
            p.drop_selected(&ids, at);
            assert_eq!(names(&p), ["a", "b", "c", "d", "e", "f", "g", "h"]);
            assert_eq!(p.current, Some(7));
        }
    }

    #[test]
    fn playlist_group_drop_keeps_relative_order_and_current() {
        let mut p = pl(&["a", "b", "c", "d", "e"]);
        p.set_shuffle(true);
        p.current = Some(3);
        let ids = [p.items[1].id, p.items[3].id];
        let play_order: Vec<_> = p.order.iter().map(|&i| p.items[i].id).collect();
        p.drop_selected(&ids, 5);
        assert_eq!(names(&p), ["a", "c", "e", "b", "d"]);
        assert_eq!(p.current, Some(4));
        assert_eq!(p.order.iter().map(|&i| p.items[i].id).collect::<Vec<_>>(), play_order);
    }

    #[test]
    fn playlist_additions_reject_subtitle_and_document_files() {
        let paths = ["movie.mp4", "subtitle.SRT", "notes.txt", "audio.flac", "subtitle.ass"].map(PathBuf::from);
        let sources = expand_playlist_paths(&paths);
        assert_eq!(sources, vec![MediaSource::File("movie.mp4".into()), MediaSource::File("audio.flac".into())]);
    }

    #[test]
    fn cdda_urls() {
        let cd = |a: &str| match source_for_arg(a) {
            MediaSource::Cd { device, track } => (device, track),
            other => panic!("{other:?}"),
        };
        assert_eq!(cd("cdda://3/dev/sr1"), ("/dev/sr1".into(), 3));
        assert_eq!(cd("cdda:///dev/sr1"), ("/dev/sr1".into(), 0));
        assert_eq!(cd("cdda://sr0/"), ("/dev/sr0".into(), 0));
        assert_eq!(cd("cdda://sr0/Track%207.wav"), ("/dev/sr0".into(), 7));
        assert_eq!(cd("cdda://2").1, 2);
    }

    #[test]
    fn insert_in_the_middle_keeps_current() {
        let mut p = pl(&["a", "b", "c"]);
        p.current = Some(2);
        let r = p.insert(1, vec![MediaSource::File("x".into()), MediaSource::File("y".into())]);
        assert_eq!(r, 1..3);
        assert_eq!(names(&p), ["a", "x", "y", "b", "c"]);
        assert_eq!(p.current, Some(4));
        assert_eq!(p.next_index(true), None);
        p.current = Some(0);
        assert_eq!(p.next_index(true), Some(1));
    }

    #[test]
    fn insert_past_the_end_appends() {
        let mut p = pl(&["a"]);
        assert_eq!(p.insert(9, vec![MediaSource::File("b".into())]), 1..2);
        assert_eq!(names(&p), ["a", "b"]);
    }

    #[test]
    fn shuffled_insert_keeps_order_consistent() {
        let mut p = pl(&["a", "b", "c", "d"]);
        p.set_shuffle(true);
        p.current = Some(3);
        p.insert(0, vec![MediaSource::File("x".into()), MediaSource::File("y".into())]);
        assert_eq!(p.current, Some(5));
        let mut order = p.order.clone();
        order.sort();
        assert_eq!(order, (0..6).collect::<Vec<_>>());
    }

    #[test]
    fn file_uris() {
        let path = |s: &str| match source_for_arg(s) {
            MediaSource::File(p) => p,
            other => panic!("{other:?}"),
        };
        assert_eq!(path("file:///tmp/a%20b.mkv"), PathBuf::from("/tmp/a b.mkv"));
        assert_eq!(path("FILE:///tmp/x.mkv"), PathBuf::from("/tmp/x.mkv"));
        assert_eq!(path("file://localhost/tmp/x.mkv"), PathBuf::from("/tmp/x.mkv"));
        assert_eq!(path("file:///tmp/%E2%9C%93.mkv"), PathBuf::from("/tmp/✓.mkv"));
        // A stray % before a multi-byte character used to panic.
        assert_eq!(path("file:///tmp/%aé%"), PathBuf::from("/tmp/%aé%"));
        assert_eq!(MediaSource::Url("http://h/%zé%4".into()).display_name(), "%zé%4");
    }

    #[test]
    fn m3u_export_keeps_one_entry_per_line() {
        let mut p = pl(&["/m/a.mkv"]);
        p.items[0].title = Some("evil\n/etc/passwd".into());
        let f = std::env::temp_dir().join(format!("mavplayer-test-{}.m3u", std::process::id()));
        p.save_m3u(&f).unwrap();
        let back = parse_playlist_file(&f);
        let _ = std::fs::remove_file(&f);
        assert_eq!(back, vec![MediaSource::File("/m/a.mkv".into())]);
    }

    #[cfg(unix)]
    #[test]
    fn folder_scan_survives_symlink_loops() {
        let d = std::env::temp_dir().join(format!("mavplayer-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/x.mkv"), b"").unwrap();
        std::os::unix::fs::symlink(&d, d.join("sub/loop")).unwrap();
        let found = expand_paths(std::slice::from_ref(&d));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn move_item_follows_current() {
        let mut p = pl(&["a", "b", "c", "d"]);
        p.current = Some(1);
        p.move_item(1, 3);
        assert_eq!(names(&p), ["a", "c", "d", "b"]);
        assert_eq!(p.current, Some(3));
        p.move_item(3, 0);
        assert_eq!(names(&p), ["b", "a", "c", "d"]);
        assert_eq!(p.current, Some(0));
    }
}
