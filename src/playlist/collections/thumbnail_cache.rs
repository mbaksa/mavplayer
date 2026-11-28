//! Rebuildable, self-contained playlist JPEG pack. Only metadata is scanned
//! on open; JPEG payloads are read on demand. A partial trailing record is
//! discarded on reopening, leaving previously committed records intact.
use super::{PlaylistId, PlaylistItemId};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const MAGIC: &[u8; 8] = b"MAVPLJ02";
const MAX_META: usize = 64 * 1024;
const MAX_JPEG: usize = 2 * 1024 * 1024;
const MAX_RECORDS: usize = 1_000_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Selection {
    pub path: PathBuf,
    pub stream: usize,
    pub count: u32,
    pub target: f64,
}

#[derive(Serialize, Deserialize)]
struct Record {
    item: PlaylistItemId,
    selection: Selection,
    time: f64,
}
struct Entry {
    record: Record,
    offset: u64,
    size: usize,
}

pub(crate) struct Pack {
    file: File,
    entries: HashMap<PlaylistItemId, Entry>,
    end: u64,
    records: usize,
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid playlist JPEG pack")
}

impl Pack {
    pub fn open(directory: &Path, playlist: PlaylistId) -> io::Result<Self> {
        let dir = directory.join("playlist-v2");
        fs::create_dir_all(&dir)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!("{playlist}.bin")))?;
        // Never allow two processes to append or repair the same pack concurrently.
        file.try_lock().map_err(|e| io::Error::other(e.to_string()))?;
        let mut len = file.metadata()?.len();
        if len < 8 {
            file.set_len(0)?;
            file.write_all(MAGIC)?;
            len = 8;
        }
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(invalid());
        }
        let mut pack = Self { file, entries: HashMap::new(), end: 8, records: 0 };
        while len - pack.end >= 8 {
            pack.file.seek(SeekFrom::Start(pack.end))?;
            let mut sizes = [0; 8];
            pack.file.read_exact(&mut sizes)?;
            let meta = u32::from_le_bytes(sizes[..4].try_into().unwrap()) as usize;
            let size = u32::from_le_bytes(sizes[4..].try_into().unwrap()) as usize;
            if meta == 0 || meta > MAX_META || size > MAX_JPEG {
                break;
            }
            let offset = pack.end + 8 + meta as u64;
            let next = offset + size as u64;
            if next > len {
                break;
            }
            let mut bytes = vec![0; meta];
            pack.file.read_exact(&mut bytes)?;
            let Ok(record) = serde_json::from_slice::<Record>(&bytes) else {
                break;
            };
            if !record.time.is_finite() || record.time < 0.0 {
                break;
            }
            if size == 0 {
                pack.entries.remove(&record.item);
            } else {
                pack.entries.insert(record.item, Entry { record, offset, size });
            }
            pack.end = next;
            pack.records += 1;
            if pack.records > MAX_RECORDS {
                return Err(invalid());
            }
        }
        if pack.end != len {
            pack.file.set_len(pack.end)?;
        }
        Ok(pack)
    }

    // Audio-only rows use count zero (video sample counts are always positive).
    // The payload is a PNG length, the PNG itself, then little-endian peak pairs.
    pub fn read_waveform(
        &mut self,
        item: PlaylistItemId,
        selection: &Selection,
    ) -> io::Result<Option<(Vec<u8>, crate::engine::waveform::Peaks)>> {
        let Some((_, data)) = self.read(item, selection)? else {
            return Ok(None);
        };
        if selection.count != 0 || data.len() < 4 {
            return Err(invalid());
        }
        let n = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        let end = 4usize.checked_add(n).ok_or_else(invalid)?;
        if n == 0 || end > data.len() || data.len() - end != 4096 * 4 {
            return Err(invalid());
        }
        let peaks = data[end..]
            .chunks_exact(4)
            .map(|p| [i16::from_le_bytes([p[0], p[1]]), i16::from_le_bytes([p[2], p[3]])])
            .collect();
        Ok(Some((data[4..end].to_vec(), peaks)))
    }

    pub fn append_waveform(
        &mut self,
        item: PlaylistItemId,
        selection: Selection,
        png: &[u8],
        peaks: &crate::engine::waveform::Peaks,
    ) -> io::Result<()> {
        if selection.count != 0 || png.is_empty() || png.len() > MAX_JPEG - 4 - 4096 * 4 || peaks.len() != 4096 {
            return Err(invalid());
        }
        let mut data = Vec::with_capacity(4 + png.len() + peaks.len() * 4);
        data.extend_from_slice(&(png.len() as u32).to_le_bytes());
        data.extend_from_slice(png);
        for p in peaks {
            data.extend_from_slice(&p[0].to_le_bytes());
            data.extend_from_slice(&p[1].to_le_bytes());
        }
        self.append(item, selection, 0.0, &data)
    }

    pub fn contains(&self, item: PlaylistItemId, selection: &Selection) -> bool {
        self.entries.get(&item).is_some_and(|entry| &entry.record.selection == selection)
    }

    pub fn invalidate_path(&mut self, path: &Path) -> io::Result<()> {
        let affected: Vec<_> = self
            .entries
            .values()
            .filter(|e| e.record.selection.path == path)
            .map(|e| (e.record.item, e.record.selection.clone()))
            .collect();
        for (item, selection) in affected {
            self.write_record(item, selection, 0.0, &[])?;
        }
        Ok(())
    }

    pub fn read(&mut self, item: PlaylistItemId, selection: &Selection) -> io::Result<Option<(f64, Vec<u8>)>> {
        let Some(entry) = self.entries.get(&item).filter(|e| &e.record.selection == selection) else {
            return Ok(None);
        };
        self.file.seek(SeekFrom::Start(entry.offset))?;
        let mut jpeg = vec![0; entry.size];
        self.file.read_exact(&mut jpeg)?;
        Ok(Some((entry.record.time, jpeg)))
    }

    pub fn append(&mut self, item: PlaylistItemId, selection: Selection, time: f64, jpeg: &[u8]) -> io::Result<()> {
        if jpeg.is_empty() {
            return Err(invalid());
        }
        self.write_record(item, selection, time, jpeg)
    }

    fn write_record(&mut self, item: PlaylistItemId, selection: Selection, time: f64, jpeg: &[u8]) -> io::Result<()> {
        let record = Record { item, selection, time };
        let meta = serde_json::to_vec(&record).map_err(io::Error::other)?;
        if meta.len() > MAX_META
            || jpeg.len() > MAX_JPEG
            || !time.is_finite()
            || time < 0.0
            || self.records >= MAX_RECORDS
        {
            return Err(invalid());
        }
        let offset = self.end + 8 + meta.len() as u64;
        // Publish the in-memory index only after the entire record is written.
        self.file.seek(SeekFrom::Start(self.end))?;
        let result = (|| {
            self.file.write_all(&(meta.len() as u32).to_le_bytes())?;
            self.file.write_all(&(jpeg.len() as u32).to_le_bytes())?;
            self.file.write_all(&meta)?;
            self.file.write_all(jpeg)?;
            self.file.flush()
        })();
        if let Err(err) = result {
            let _ = self.file.set_len(self.end);
            return Err(err);
        }
        self.end = offset + jpeg.len() as u64;
        self.records += 1;
        if jpeg.is_empty() {
            self.entries.remove(&item);
        } else {
            self.entries.insert(item, Entry { record, offset, size: jpeg.len() });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_payload_roundtrip() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!("mav-audio-pack-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let playlist = PlaylistId::new();
            let item = PlaylistItemId::new();
            let selection = Selection { path: "offline.flac".into(), stream: 0, count: 0, target: 0.0 };
            let peaks = vec![[-100, 200]; 4096];
            let mut pack = Pack::open(&root, playlist)?;
            pack.append_waveform(item, selection.clone(), b"mask", &peaks)?;
            drop(pack);
            let mut pack = Pack::open(&root, playlist)?;
            assert_eq!(pack.read_waveform(item, &selection)?, Some((b"mask".to_vec(), peaks)));
            Ok(())
        })();
        let _ = fs::remove_dir_all(root);
        result
    }

    #[test]
    fn packed_jpeg_roundtrip_and_partial_tail() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!("mav-pack-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let playlist = PlaylistId::new();
            let item = PlaylistItemId::new();
            let other = PlaylistItemId::new();
            let selection = Selection { path: "unavailable.mp4".into(), stream: 0, count: 20, target: 5.0 };
            let mut pack = Pack::open(&root, playlist)?;
            pack.append(item, selection.clone(), 4.0, b"first jpeg")?;
            pack.append(other, selection.clone(), 6.0, b"other jpeg")?;
            pack.append(item, selection.clone(), 5.0, b"new jpeg")?;
            assert!(Pack::open(&root, playlist).is_err());
            let end = pack.end;
            pack.file.seek(SeekFrom::End(0))?;
            pack.file.write_all(&[1, 2, 3])?;
            drop(pack);
            let mut pack = Pack::open(&root, playlist)?;
            assert_eq!(pack.file.metadata()?.len(), end);
            assert_eq!(pack.read(item, &selection)?, Some((5.0, b"new jpeg".to_vec())));
            assert_eq!(pack.read(other, &selection)?, Some((6.0, b"other jpeg".to_vec())));
            assert!(pack.read(item, &Selection { target: 9.0, ..selection.clone() })?.is_none());
            pack.invalidate_path(&selection.path)?;
            drop(pack);
            let mut pack = Pack::open(&root, playlist)?;
            assert!(pack.read(item, &selection)?.is_none());
            assert!(pack.read(other, &selection)?.is_none());
            Ok(())
        })();
        let _ = fs::remove_dir_all(root);
        result
    }
}
