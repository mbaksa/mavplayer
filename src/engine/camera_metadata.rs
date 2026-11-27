//! Explicit Sony capture metadata from embedded MP4 XML or adjacent XML files.

use super::types::CameraMetadata;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_XML_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct SidecarStamp {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

/// The exact clip stem avoids picking up metadata from neighboring clips.
/// Prefer Sony's M01 naming convention over a plain XML sidecar.
pub(super) fn candidates(path: &Path) -> Vec<SidecarStamp> {
    if !path.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("mp4")) {
        return Vec::new();
    }
    let Some(stem) = path.file_stem() else { return Vec::new() };
    ["M01.XML", "M01.xml", ".XML", ".xml"]
        .iter()
        .filter_map(|suffix| {
            let mut name = stem.to_os_string();
            name.push(suffix);
            let path = path.with_file_name(name);
            let meta = fs::metadata(&path).ok()?;
            if !meta.is_file() || meta.len() > MAX_XML_BYTES {
                return None;
            }
            Some(SidecarStamp { path, size: meta.len(), modified: meta.modified().ok()? })
        })
        .collect()
}

pub(super) fn read(stamps: &[SidecarStamp]) -> Option<CameraMetadata> {
    stamps.iter().find_map(|stamp| {
        let mut bytes = Vec::new();
        File::open(&stamp.path).ok()?.take(MAX_XML_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_XML_BYTES {
            return None;
        }
        parse(std::str::from_utf8(&bytes).ok()?.trim_start_matches('\u{feff}'), &stamp.path)
    })
}

const MAX_DEPTH: usize = 16;
const MAX_BOXES: usize = 100_000;

#[derive(Clone, Copy)]
struct Mp4Box {
    kind: [u8; 4],
    payload: u64,
    end: u64,
}

fn invalid_box() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid MP4 box")
}

fn read_box(reader: &mut (impl Read + Seek), start: u64, limit: u64, budget: &mut usize) -> io::Result<Mp4Box> {
    if *budget == 0 || limit.checked_sub(start).ok_or_else(invalid_box)? < 8 {
        return Err(invalid_box());
    }
    *budget -= 1;
    reader.seek(SeekFrom::Start(start))?;
    let mut header = [0; 8];
    reader.read_exact(&mut header)?;
    let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let kind = [header[4], header[5], header[6], header[7]];
    let (size, header_size) = match size {
        0 => (limit - start, 8),
        1 => {
            if limit - start < 16 {
                return Err(invalid_box());
            }
            let mut extended = [0; 8];
            reader.read_exact(&mut extended)?;
            (u64::from_be_bytes(extended), 16)
        }
        size => (u64::from(size), 8),
    };
    let end = start.checked_add(size).ok_or_else(invalid_box)?;
    if size < header_size || end > limit {
        return Err(invalid_box());
    }
    Ok(Mp4Box { kind, payload: start + header_size, end })
}

fn inspect_meta(
    reader: &mut (impl Read + Seek),
    meta: Mp4Box,
    path: &Path,
    budget: &mut usize,
) -> io::Result<Option<CameraMetadata>> {
    if meta.end - meta.payload < 4 {
        return Err(invalid_box());
    }
    let mut offset = meta.payload + 4;
    let mut sony_handler = false;
    let mut xml = None;
    while offset < meta.end {
        let child = read_box(reader, offset, meta.end, budget)?;
        match &child.kind {
            b"hdlr" if child.end - child.payload >= 12 => {
                reader.seek(SeekFrom::Start(child.payload + 8))?;
                let mut handler = [0; 4];
                reader.read_exact(&mut handler)?;
                sony_handler |= &handler == b"nrtm";
            }
            b"xml " if child.end - child.payload > 4 && child.end - child.payload - 4 <= MAX_XML_BYTES => {
                xml = Some(child);
            }
            _ => {}
        }
        offset = child.end;
    }
    if sony_handler && let Some(xml) = xml {
        reader.seek(SeekFrom::Start(xml.payload + 4))?;
        let mut bytes = vec![0; (xml.end - xml.payload - 4) as usize];
        reader.read_exact(&mut bytes)?;
        if let Ok(text) = std::str::from_utf8(&bytes) {
            return Ok(parse(text.trim_start_matches('\u{feff}').trim_end_matches('\0'), path).map(|mut camera| {
                camera.embedded = true;
                camera
            }));
        }
    }
    Ok(None)
}

fn scan_boxes(
    reader: &mut (impl Read + Seek),
    mut offset: u64,
    end: u64,
    depth: usize,
    path: &Path,
    budget: &mut usize,
) -> io::Result<Option<CameraMetadata>> {
    if depth >= MAX_DEPTH {
        return Ok(None);
    }
    while offset < end {
        let child = read_box(reader, offset, end, budget)?;
        let found = match &child.kind {
            b"meta" => inspect_meta(reader, child, path, budget)?,
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"udta" | b"moof" | b"traf" => {
                scan_boxes(reader, child.payload, child.end, depth + 1, path, budget)?
            }
            _ => None,
        };
        if found.is_some() {
            return Ok(found);
        }
        offset = child.end;
    }
    Ok(None)
}

/// Read Sony's embedded NonRealTimeMeta without decoding or reading mdat.
pub(super) fn read_sony_metadata(path: &Path) -> io::Result<Option<CameraMetadata>> {
    if !path.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("mp4")) {
        return Ok(None);
    }
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut budget = MAX_BOXES;
    scan_boxes(&mut file, 0, len, 0, path, &mut budget)
}

pub(super) fn read_for_media(path: &Path, stamps: &[SidecarStamp]) -> Option<CameraMetadata> {
    read_sony_metadata(path).ok().flatten().or_else(|| read(stamps))
}

fn parse(xml: &str, path: &Path) -> Option<CameraMetadata> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "NonRealTimeMeta" {
        return None;
    }
    let device = root.children().find(|n| n.is_element() && n.tag_name().name() == "Device")?;
    let manufacturer = device.attribute("manufacturer")?;
    if !manufacturer.eq_ignore_ascii_case("Sony") {
        return None;
    }
    let clean = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_owned());
    let value = |key: &str| {
        root.descendants()
            .find(|n| {
                n.is_element() && n.tag_name().name() == "Item" && n.attribute("name") == Some(key)
                    && n.parent().is_some_and(|g| g.tag_name().name() == "Group" && g.attribute("name") == Some("CameraUnitMetadataSet")
                        && g.parent().is_some_and(|a| a.tag_name().name() == "AcquisitionRecord"))
            })
            .and_then(|n| n.attribute("value"))
            .and_then(clean)
    };
    Some(CameraMetadata {
        manufacturer: clean(manufacturer),
        model: device.attribute("modelName").and_then(clean),
        capture_gamma: value("CaptureGammaEquation"),
        capture_gamut: value("CaptureColorPrimaries"),
        coding_equations: value("CodingEquations"),
        capture_fps: root.descendants().find_map(|n| n.attribute("captureFps")).and_then(clean),
        codec: root.descendants().find_map(|n| n.attribute("videoCodec")).and_then(clean),
        lens_model: root.descendants().find_map(|n| n.attribute("lensModel")).and_then(clean),
        embedded: false,
        source: path.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        bytes.extend(kind);
        bytes.extend(payload);
        bytes
    }

    fn synthetic(handler: &[u8; 4], xml: &str) -> Vec<u8> {
        let mut h = vec![0; 8];
        h.extend(handler);
        let mut x = vec![0; 4];
        x.extend(xml.as_bytes());
        let mut meta = vec![0; 4];
        meta.extend(atom(b"xml ", &x));
        meta.extend(atom(b"hdlr", &h));
        let mut bytes = atom(b"mdat", &[0; 128]);
        bytes.extend(atom(b"moov", &atom(b"udta", &atom(b"meta", &meta))));
        bytes
    }

    fn scan(bytes: &[u8]) -> io::Result<Option<CameraMetadata>> {
        let mut budget = MAX_BOXES;
        scan_boxes(&mut io::Cursor::new(bytes), 0, bytes.len() as u64, 0, Path::new("clip.mp4"), &mut budget)
    }

    #[test]
    fn embedded_sony_and_unknown_gamma() {
        let xml = r#"<NonRealTimeMeta><Device manufacturer="Sony" modelName="ILCE-6500"/></NonRealTimeMeta>"#;
        let bytes = synthetic(b"nrtm", xml);
        let camera = scan(&bytes).unwrap().unwrap();
        assert!(camera.embedded);
        assert_eq!(camera.model.as_deref(), Some("ILCE-6500"));
        assert!(camera.capture_gamma.is_none());
        assert!(scan(&synthetic(b"mdir", xml)).unwrap().is_none());
        assert!(scan(&synthetic(b"nrtm", "<broken")).unwrap().is_none());
        assert!(scan(&atom(b"mdat", xml.as_bytes())).unwrap().is_none());
        assert!(scan(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn extended_and_parent_bounded_boxes() {
        let xml = r#"<NonRealTimeMeta><Device manufacturer="Sony" modelName="ILCE-7M4"/><AcquisitionRecord><Group name="CameraUnitMetadataSet"><Item name="CaptureGammaEquation" value="s-log3"/><Item name="CaptureColorPrimaries" value="s-gamut3"/></Group></AcquisitionRecord></NonRealTimeMeta>"#;
        let payload = synthetic(b"nrtm", xml);
        let mut extended = vec![0, 0, 0, 1, b'm', b'o', b'o', b'v'];
        extended.extend(((payload.len() + 16) as u64).to_be_bytes());
        extended.extend(&payload);
        let camera = scan(&extended).unwrap().unwrap();
        assert_eq!(camera.capture_gamma.as_deref(), Some("s-log3"));
        assert_eq!(camera.capture_gamut.as_deref(), Some("s-gamut3"));
        let mut bounded = vec![0, 0, 0, 0, b'u', b'd', b't', b'a'];
        bounded.extend(payload);
        assert!(scan(&bounded).unwrap().is_some());
        extended[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(scan(&extended).is_err());
    }

    #[test]
    fn sony_capture_settings() {
        let xml = r#"<NonRealTimeMeta xmlns="urn:schemas-professionalDisc:nonRealTimeMeta:ver.2.20">
            <Device manufacturer="Sony" modelName="ILCE-7M4"/>
            <AcquisitionRecord><Group name="CameraUnitMetadataSet"><Item name="CaptureGammaEquation" value="s-log3-cine"/>
            <Item name="CaptureColorPrimaries" value="s-gamut3-cine"/></Group></AcquisitionRecord>
            </NonRealTimeMeta>"#;
        let camera = parse(xml, Path::new("C0758M01.XML")).unwrap();
        assert_eq!(camera.model.as_deref(), Some("ILCE-7M4"));
        assert_eq!(camera.capture_gamma.as_deref(), Some("s-log3-cine"));
        assert_eq!(camera.capture_gamut.as_deref(), Some("s-gamut3-cine"));
    }

    #[test]
    fn missing_fields_are_not_inferred() {
        let xml = r#"<NonRealTimeMeta><Device manufacturer="Sony" modelName="ILCE-6500"/></NonRealTimeMeta>"#;
        let camera = parse(xml, Path::new("clip.xml")).unwrap();
        assert!(camera.capture_gamma.is_none());
        assert!(camera.capture_gamut.is_none());
        assert!(parse("<broken", Path::new("clip.xml")).is_none());
        assert!(parse("<Other/>", Path::new("clip.xml")).is_none());
    }
}
