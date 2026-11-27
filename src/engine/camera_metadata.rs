//! Explicit Sony capture metadata from adjacent NonRealTimeMeta XML files.

use super::types::CameraMetadata;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
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
            .find(|n| n.is_element() && n.tag_name().name() == "Item" && n.attribute("name") == Some(key))
            .and_then(|n| n.attribute("value"))
            .and_then(clean)
    };
    Some(CameraMetadata {
        manufacturer: clean(manufacturer),
        model: device.attribute("modelName").and_then(clean),
        capture_gamma: value("CaptureGammaEquation"),
        capture_gamut: value("CaptureColorPrimaries"),
        source: path.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sony_capture_settings() {
        let xml = r#"<NonRealTimeMeta xmlns="urn:schemas-professionalDisc:nonRealTimeMeta:ver.2.20">
            <Device manufacturer="Sony" modelName="ILCE-7M4"/>
            <AcquisitionRecord><Group><Item name="CaptureGammaEquation" value="s-log3-cine"/>
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
