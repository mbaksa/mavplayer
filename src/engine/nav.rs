//! DVDs with their menus. libdvdnav runs the disc's navigation program (the
//! "virtual machine": first-play logos, menus and their buttons, title and
//! chapter jumps, post-title commands) and hands out the MPEG program stream
//! block by block; FFmpeg's `mpeg` demuxer reads those blocks through a
//! custom `AVIOContext`.
//!
//! The disc is played as a series of *segments*: one per menu domain, VTS or
//! title. Each segment gets its own stream set (built from the disc's IFO
//! attributes, so subtitle streams that only start later are still listed)
//! and its own decoder components; the session, its queues and the window
//! carry on across them.
//!
//! Timestamps restart in every cell and jump on every seek or menu link.
//! They are rewritten to one continuous timeline per segment (title time
//! for titles, so seeking, chapters and the position display work) with an
//! offset per VOBU, looked up by each packet's byte position.
//!
//! libdvdnav events that need the pipeline's attention (a still picture, a
//! jump, the end of a segment) end the reader's input there (EOF to the
//! demuxer); the segment loop handles them and reading resumes.

use super::av::{self, Dict, FormatCtx, IoSource, Packet, ff};
use super::demux::{self, Component, stream_index};
use super::spu::{Highlight, Overlay};
use super::types::*;
use super::{LoadOptions, SeekRequest, SessionCtx};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[allow(non_camel_case_types, dead_code)]
mod sys {
    use std::os::raw::{c_char, c_int};

    #[repr(C)]
    pub struct dvdnav_t {
        _p: [u8; 0],
    }
    /// `pci_t`: only read at fixed (packed) offsets, see `PCI_*`.
    pub type Pci = u8;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct HighlightArea {
        pub palette: u32,
        pub sx: u16,
        pub sy: u16,
        pub ex: u16,
        pub ey: u16,
        pub pts: u32,
        pub button: u32,
    }

    pub const OK: i32 = 1;
    pub const BLOCK_OK: c_int = 0;
    pub const NOP: c_int = 1;
    pub const STILL_FRAME: c_int = 2;
    pub const SPU_STREAM_CHANGE: c_int = 3;
    pub const AUDIO_STREAM_CHANGE: c_int = 4;
    pub const VTS_CHANGE: c_int = 5;
    pub const CELL_CHANGE: c_int = 6;
    pub const NAV_PACKET: c_int = 7;
    pub const STOP: c_int = 8;
    pub const HIGHLIGHT: c_int = 9;
    pub const SPU_CLUT_CHANGE: c_int = 10;
    pub const HOP_CHANNEL: c_int = 12;
    pub const WAIT: c_int = 13;

    pub const AUDIO_STREAM: i32 = 1;
    pub const SUBTITLE_STREAM: i32 = 0;

    // Packed `pci_t` offsets (libdvdread's nav_types.h).
    pub const PCI_VOBU_S_PTM: usize = 12;
    pub const PCI_VOBU_E_PTM: usize = 16;
    pub const PCI_HLI_SS: usize = 96;
    pub const PCI_BTN_NS: usize = 113;

    unsafe extern "C" {
        pub fn dvdnav_open(dest: *mut *mut dvdnav_t, path: *const c_char) -> i32;
        pub fn dvdnav_close(nav: *mut dvdnav_t) -> i32;
        pub fn dvdnav_reset(nav: *mut dvdnav_t) -> i32;
        pub fn dvdnav_err_to_string(nav: *mut dvdnav_t) -> *const c_char;
        pub fn dvdnav_set_readahead_flag(nav: *mut dvdnav_t, flag: i32) -> i32;
        pub fn dvdnav_set_PGC_positioning_flag(nav: *mut dvdnav_t, flag: i32) -> i32;
        pub fn dvdnav_set_region_mask(nav: *mut dvdnav_t, mask: i32) -> i32;
        pub fn dvdnav_menu_language_select(nav: *mut dvdnav_t, code: *mut c_char) -> i32;
        pub fn dvdnav_audio_language_select(nav: *mut dvdnav_t, code: *mut c_char) -> i32;
        pub fn dvdnav_spu_language_select(nav: *mut dvdnav_t, code: *mut c_char) -> i32;
        pub fn dvdnav_get_next_block(nav: *mut dvdnav_t, buf: *mut u8, event: *mut c_int, len: *mut c_int) -> i32;
        pub fn dvdnav_still_skip(nav: *mut dvdnav_t) -> i32;
        pub fn dvdnav_wait_skip(nav: *mut dvdnav_t) -> i32;
        pub fn dvdnav_get_number_of_titles(nav: *mut dvdnav_t, titles: *mut i32) -> i32;
        pub fn dvdnav_title_play(nav: *mut dvdnav_t, title: i32) -> i32;
        pub fn dvdnav_describe_title_chapters(
            nav: *mut dvdnav_t,
            title: i32,
            times: *mut *mut u64,
            duration: *mut u64,
        ) -> u32;
        pub fn dvdnav_menu_call(nav: *mut dvdnav_t, menu: i32) -> i32;
        pub fn dvdnav_current_title_info(nav: *mut dvdnav_t, title: *mut i32, part: *mut i32) -> i32;
        pub fn dvdnav_get_current_time(nav: *mut dvdnav_t) -> i64;
        pub fn dvdnav_jump_to_sector_by_time(nav: *mut dvdnav_t, time: u64, mode: i32) -> i32;
        pub fn dvdnav_go_up(nav: *mut dvdnav_t) -> i32;
        pub fn dvdnav_get_current_highlight(nav: *mut dvdnav_t, button: *mut i32) -> i32;
        pub fn dvdnav_get_current_nav_pci(nav: *mut dvdnav_t) -> *mut Pci;
        pub fn dvdnav_get_highlight_area(pci: *mut Pci, button: i32, mode: i32, hl: *mut HighlightArea) -> i32;
        pub fn dvdnav_upper_button_select(nav: *mut dvdnav_t, pci: *mut Pci) -> i32;
        pub fn dvdnav_lower_button_select(nav: *mut dvdnav_t, pci: *mut Pci) -> i32;
        pub fn dvdnav_right_button_select(nav: *mut dvdnav_t, pci: *mut Pci) -> i32;
        pub fn dvdnav_left_button_select(nav: *mut dvdnav_t, pci: *mut Pci) -> i32;
        pub fn dvdnav_button_activate(nav: *mut dvdnav_t, pci: *mut Pci) -> i32;
        pub fn dvdnav_mouse_select(nav: *mut dvdnav_t, pci: *mut Pci, x: i32, y: i32) -> i32;
        pub fn dvdnav_mouse_activate(nav: *mut dvdnav_t, pci: *mut Pci, x: i32, y: i32) -> i32;
        pub fn dvdnav_get_title_string(nav: *mut dvdnav_t, s: *mut *const c_char) -> i32;
        pub fn dvdnav_get_video_aspect(nav: *mut dvdnav_t) -> u8;
        pub fn dvdnav_get_video_resolution(nav: *mut dvdnav_t, w: *mut u32, h: *mut u32) -> i32;
        pub fn dvdnav_audio_stream_to_lang(nav: *mut dvdnav_t, stream: u8) -> u16;
        pub fn dvdnav_audio_stream_format(nav: *mut dvdnav_t, stream: u8) -> u16;
        pub fn dvdnav_audio_stream_channels(nav: *mut dvdnav_t, stream: u8) -> u16;
        pub fn dvdnav_spu_stream_to_lang(nav: *mut dvdnav_t, stream: u8) -> u16;
        pub fn dvdnav_get_audio_logical_stream(nav: *mut dvdnav_t, n: u8) -> i8;
        pub fn dvdnav_get_spu_logical_stream(nav: *mut dvdnav_t, n: u8) -> i8;
        pub fn dvdnav_set_active_stream(nav: *mut dvdnav_t, n: u8, kind: i32) -> i32;
        pub fn dvdnav_angle_change(nav: *mut dvdnav_t, angle: i32) -> i32;
        pub fn dvdnav_get_angle_info(nav: *mut dvdnav_t, cur: *mut i32, n: *mut i32) -> i32;
        pub fn dvdnav_is_domain_fp(nav: *mut dvdnav_t) -> i8;
        pub fn dvdnav_is_domain_vmgm(nav: *mut dvdnav_t) -> i8;
        pub fn dvdnav_is_domain_vtsm(nav: *mut dvdnav_t) -> i8;
        pub fn dvdnav_is_domain_vts(nav: *mut dvdnav_t) -> i8;
    }

    unsafe extern "C" {
        pub fn free(p: *mut std::ffi::c_void);
    }

    // libdvdread, for reading the IFO files themselves.
    #[repr(C)]
    pub struct dvd_reader_t {
        _p: [u8; 0],
    }
    #[repr(C)]
    pub struct dvd_file_t {
        _p: [u8; 0],
    }
    /// `DVD_READ_INFO_FILE`: a title set's (or the disc's) .IFO.
    pub const READ_INFO_FILE: c_int = 0;

    unsafe extern "C" {
        pub fn DVDOpen(path: *const c_char) -> *mut dvd_reader_t;
        pub fn DVDClose(dvd: *mut dvd_reader_t);
        pub fn DVDOpenFile(dvd: *mut dvd_reader_t, title: c_int, domain: c_int) -> *mut dvd_file_t;
        pub fn DVDCloseFile(f: *mut dvd_file_t);
        pub fn DVDFileSeek(f: *mut dvd_file_t, offset: i32) -> i32;
        pub fn DVDReadBytes(f: *mut dvd_file_t, data: *mut std::ffi::c_void, size: usize) -> isize;
    }
}

/// The disc's titles, from its tables alone: durations and chapters from
/// libdvdnav, languages from the title sets' IFO attributes. Nothing of the
/// video is read, unlike opening each title with FFmpeg.
pub fn list_titles(path: &str) -> Option<Vec<super::disc::DvdTitle>> {
    let c = av::cstr(path);
    let mut dvd: *mut sys::dvdnav_t = ptr::null_mut();
    if unsafe { sys::dvdnav_open(&mut dvd, c.as_ptr()) } != sys::OK || dvd.is_null() {
        return None;
    }
    let mut n = 0;
    unsafe { sys::dvdnav_get_number_of_titles(dvd, &mut n) };
    let mut titles = Vec::new();
    for t in 1..=n.max(0) {
        let mut times: *mut u64 = ptr::null_mut();
        let mut dur = 0u64;
        let chapters = unsafe { sys::dvdnav_describe_title_chapters(dvd, t, &mut times, &mut dur) };
        if !times.is_null() {
            unsafe { sys::free(times as *mut c_void) };
        }
        if chapters == 0 || dur == 0 {
            continue;
        }
        titles.push(super::disc::DvdTitle {
            number: t as u32,
            duration: dur as f64 / 90000.0,
            chapters: chapters as usize,
            audio: Vec::new(),
            subtitles: Vec::new(),
        });
    }
    unsafe { sys::dvdnav_close(dvd) };
    if let Some(langs) = Ifo::open(&c).and_then(|ifo| ifo.languages()) {
        for t in &mut titles {
            if let Some((a, s)) = langs.get(t.number as usize - 1) {
                (t.audio, t.subtitles) = (a.clone(), s.clone());
            }
        }
    }
    Some(titles)
}

/// The disc's IFO files, read with libdvdread.
struct Ifo(*mut sys::dvd_reader_t);

impl Drop for Ifo {
    fn drop(&mut self) {
        unsafe { sys::DVDClose(self.0) };
    }
}

impl Ifo {
    fn open(path: &std::ffi::CStr) -> Option<Ifo> {
        let r = unsafe { sys::DVDOpen(path.as_ptr()) };
        (!r.is_null()).then_some(Ifo(r))
    }

    /// `len` bytes at `offset` of IFO `n` (0: VIDEO_TS.IFO, else VTS_n_0.IFO).
    fn read(&self, n: i32, offset: usize, len: usize) -> Option<Vec<u8>> {
        unsafe {
            let f = sys::DVDOpenFile(self.0, n, sys::READ_INFO_FILE);
            if f.is_null() {
                return None;
            }
            let mut buf = vec![0u8; len];
            let ok = sys::DVDFileSeek(f, offset as i32) == offset as i32
                && sys::DVDReadBytes(f, buf.as_mut_ptr() as *mut c_void, len) == len as isize;
            sys::DVDCloseFile(f);
            ok.then_some(buf)
        }
    }

    /// Audio and subtitle language names of each title (in title order).
    fn languages(&self) -> Option<Vec<(Vec<String>, Vec<String>)>> {
        let be16 = |b: &[u8], o: usize| (b[o] as u16) << 8 | b[o + 1] as u16;
        let be32 = |b: &[u8], o: usize| u32::from_be_bytes(b[o..o + 4].try_into().unwrap());
        let vmgi = self.read(0, 0, 0x100)?;
        if &vmgi[..12] != b"DVDVIDEO-VMG" {
            return None;
        }
        // TT_SRPT: title count, then 12 bytes per title (byte 6: its title set).
        let at = be32(&vmgi, 0xc4) as usize * BLOCK;
        let count = (be16(&self.read(0, at, 8)?, 0) as usize).min(99);
        let srpt = self.read(0, at, 8 + count * 12)?;
        let mut sets: HashMap<u8, (Vec<String>, Vec<String>)> = HashMap::new();
        let name = |code: u16| lang_code(code).map(|l| language_name(&l)).unwrap_or_else(|| "?".into());
        let mut out = Vec::new();
        for i in 0..count {
            let vts = srpt[8 + i * 12 + 6];
            let l = sets.entry(vts).or_insert_with(|| {
                // VTSI_MAT: audio streams at 0x202/0x204 (8 bytes each),
                // subpictures at 0x254/0x256 (6 bytes each); language codes
                // at bytes 2–3 of each.
                match self.read(vts as i32, 0, 0x400) {
                    Some(m) if &m[..12] == b"DVDVIDEO-VTS" => {
                        let na = (be16(&m, 0x202) as usize).min(8);
                        let ns = (be16(&m, 0x254) as usize).min(32);
                        (
                            (0..na).map(|k| name(be16(&m, 0x204 + k * 8 + 2))).collect(),
                            (0..ns).map(|k| name(be16(&m, 0x256 + k * 6 + 2))).collect(),
                        )
                    }
                    _ => Default::default(),
                }
            });
            out.push(l.clone());
        }
        Some(out)
    }
}

const BLOCK: usize = 2048;
/// Same back-pressure limits as files (see demux.rs).
const MAX_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const MIN_FRAMES: usize = 25;
const READ_AHEAD_SECS: f64 = 3.0;

/// Something the reader stopped at for the segment loop to handle.
#[derive(Debug, Clone, PartialEq)]
enum Pending {
    /// A still picture: hold it for this many seconds (255: until a button).
    Still(i32),
    /// libdvdnav wants the decoders drained before it goes on.
    Wait,
    /// A jump (button, menu call, link): what is buffered is stale.
    Hop,
    /// Another menu domain, VTS or title begins.
    SegmentEnd {
        immediate: bool,
    },
    /// The disc's program ended.
    Stop,
    Error(String),
}

struct State {
    dvd: *mut sys::dvdnav_t,
    block: Box<[u8; BLOCK]>,
    /// Data not yet handed to the demuxer (a block read while priming, or
    /// the rest of one that didn't fit).
    stash: Vec<u8>,
    pending: Option<Pending>,
    priming: bool,
    /// Bytes handed to the demuxer so far (its notion of file position).
    pos: i64,
    /// Timestamp offsets (90 kHz) from the byte position where each applies.
    offsets: VecDeque<(i64, i64)>,
    offset: i64,
    last_e_ptm: Option<i64>,
    /// The next VOBU takes its time from libdvdnav (segment start, jumps).
    rebase: bool,
    /// The jump we started ourselves (a seek) reports a hop: not a new one.
    expect_hop: bool,
    data_since_hop: bool,
    seg_title: Option<i32>,
    /// Playing just this title (`dvd://N`): leaving it ends playback,
    /// rather than following the disc's program to its menus.
    only_title: Option<i32>,
    title: i32,
    part: i32,
    /// The streams the disc's program selected (physical numbers; for
    /// subpictures 0x80 is set while subtitles are off).
    vm_audio: i32,
    vm_spu: i32,
    audio_changed: bool,
    spu_changed: bool,
    nav_changed: bool,
    /// A new subpicture palette (0x00YYCrCb entries).
    clut: Option<[u32; 16]>,
}

unsafe impl Send for State {}

impl Drop for State {
    fn drop(&mut self) {
        unsafe { sys::dvdnav_close(self.dvd) };
    }
}

pub struct Nav {
    st: Mutex<State>,
    /// The UI pressed a button or called a menu: a still may be over.
    poked: AtomicBool,
    pub overlay: Arc<Overlay>,
    disc_title: Option<String>,
    titles: u32,
    /// See [`State::only_title`].
    pub only: Option<u32>,
}

fn err(dvd: *mut sys::dvdnav_t) -> String {
    unsafe { av::from_cstr(sys::dvdnav_err_to_string(dvd)) }.unwrap_or_else(|| "unknown error".into())
}

/// A language preference ("eng", "en", "English") as the two-letter code
/// libdvdnav wants.
fn iso639_1(pref: &str) -> Option<String> {
    const CODES: &[&str] = &[
        "en", "fr", "de", "es", "it", "pt", "ru", "ja", "zh", "ko", "nl", "sv", "no", "da", "fi", "pl", "cs", "hu",
        "tr", "el", "he", "ar", "hi", "th", "uk", "ro", "bg", "hr", "sr", "vi", "id",
    ];
    let p = pref.trim().to_ascii_lowercase();
    if p.len() == 2 && p.chars().all(|c| c.is_ascii_lowercase()) {
        return Some(p);
    }
    CODES.iter().find(|c| language_matches(&p, c)).map(|c| c.to_string())
}

fn lang_code(v: u16) -> Option<String> {
    let (a, b) = ((v >> 8) as u8, v as u8);
    (a.is_ascii_alphabetic() && b.is_ascii_alphabetic()).then(|| format!("{}{}", a as char, b as char).to_lowercase())
}

impl Nav {
    pub fn open(path: &str, cfg: &EngineConfig, only: Option<u32>) -> anyhow::Result<Nav> {
        let mut dvd: *mut sys::dvdnav_t = ptr::null_mut();
        let c = av::cstr(path);
        if unsafe { sys::dvdnav_open(&mut dvd, c.as_ptr()) } != sys::OK || dvd.is_null() {
            anyhow::bail!("Could not read the DVD. Encrypted discs need libdvdcss.");
        }
        unsafe {
            sys::dvdnav_set_readahead_flag(dvd, 1);
            sys::dvdnav_set_PGC_positioning_flag(dvd, 1);
            if cfg.dvd_region > 0 && cfg.dvd_region <= 8 {
                sys::dvdnav_set_region_mask(dvd, 1 << (cfg.dvd_region - 1));
            }
            // The disc's own program picks audio, subtitles and the menu
            // language from these.
            let sysl = system_languages();
            let pick =
                |prefs: &[String]| prefs.iter().chain(sysl.iter()).find_map(|p| iso639_1(p)).map(|l| av::cstr(&l));
            if let Some(l) = pick(&[]) {
                sys::dvdnav_menu_language_select(dvd, l.as_ptr() as *mut c_char);
            }
            if let Some(l) = pick(&cfg.audio_langs) {
                sys::dvdnav_audio_language_select(dvd, l.as_ptr() as *mut c_char);
            }
            if let Some(l) = pick(&cfg.sub_langs) {
                sys::dvdnav_spu_language_select(dvd, l.as_ptr() as *mut c_char);
            }
        }
        let mut s: *const c_char = ptr::null();
        let disc_title =
            unsafe { if sys::dvdnav_get_title_string(dvd, &mut s) == sys::OK { av::from_cstr(s) } else { None } }
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty());
        let mut n = 0;
        unsafe { sys::dvdnav_get_number_of_titles(dvd, &mut n) };
        Ok(Nav {
            st: Mutex::new(State {
                dvd,
                block: Box::new([0; BLOCK]),
                stash: Vec::new(),
                pending: None,
                priming: false,
                pos: 0,
                offsets: VecDeque::new(),
                offset: 0,
                last_e_ptm: None,
                rebase: true,
                expect_hop: false,
                data_since_hop: false,
                seg_title: None,
                only_title: only.map(|t| t as i32),
                title: 0,
                part: 0,
                vm_audio: -1,
                vm_spu: -1,
                audio_changed: false,
                spu_changed: false,
                nav_changed: true,
                clut: None,
            }),
            poked: AtomicBool::new(false),
            overlay: Arc::new(Overlay::default()),
            disc_title,
            titles: n.max(0) as u32,
            only,
        })
    }

    /// Reads the next data for the demuxer (0: stopped at an event).
    fn read(&self, buf: &mut [u8], alive: &dyn Fn() -> bool) -> c_int {
        let mut g = self.st.lock();
        if g.pending.is_some() {
            return 0;
        }
        loop {
            if !g.stash.is_empty() {
                let n = buf.len().min(g.stash.len());
                buf[..n].copy_from_slice(&g.stash[..n]);
                g.stash.drain(..n);
                g.pos += n as i64;
                g.data_since_hop = true;
                return n as c_int;
            }
            if !alive() {
                return ff::AVERROR_EXIT;
            }
            match g.next_event() {
                Some(p) => {
                    g.pending = Some(p);
                    return 0;
                }
                None => continue,
            }
        }
    }

    fn take_pending(&self) -> Option<Pending> {
        self.st.lock().pending.take()
    }

    /// Byte position → timestamp offset (90 kHz) for a packet.
    fn offset_for(&self, pos: i64) -> i64 {
        let mut g = self.st.lock();
        // Keep a few MB of history: parsed video frames come out later
        // than the packets read after them.
        while g.offsets.len() > 1 && g.offsets[1].0 <= pos - 8 * 1024 * 1024 {
            g.offsets.pop_front();
        }
        if pos < 0 {
            return g.offset;
        }
        g.offsets.iter().rev().find(|(p, _)| *p <= pos).map(|(_, o)| *o).unwrap_or(g.offset)
    }

    pub fn poke(&self) {
        self.poked.store(true, Ordering::Release);
    }

    /// Runs a remote-control command. Returns false if the disc refused it
    /// (no buttons there, a prohibited operation, …).
    pub fn command(&self, cmd: DvdCommand) -> bool {
        // Hovering mustn't make the UI wait for a disc read in progress.
        let mut g = match cmd {
            DvdCommand::Hover(..) => match self.st.try_lock_for(Duration::from_millis(5)) {
                Some(g) => g,
                None => return false,
            },
            _ => self.st.lock(),
        };
        let dvd = g.dvd;
        let ok = unsafe {
            let pci = sys::dvdnav_get_current_nav_pci(dvd);
            let r = match cmd {
                DvdCommand::Up => sys::dvdnav_upper_button_select(dvd, pci),
                DvdCommand::Down => sys::dvdnav_lower_button_select(dvd, pci),
                DvdCommand::Left => sys::dvdnav_left_button_select(dvd, pci),
                DvdCommand::Right => sys::dvdnav_right_button_select(dvd, pci),
                DvdCommand::Activate => sys::dvdnav_button_activate(dvd, pci),
                DvdCommand::Hover(x, y) => sys::dvdnav_mouse_select(dvd, pci, x as i32, y as i32),
                DvdCommand::Click(x, y) => sys::dvdnav_mouse_activate(dvd, pci, x as i32, y as i32),
                DvdCommand::Menu(m) => sys::dvdnav_menu_call(
                    dvd,
                    match m {
                        DvdMenu::Title => 2,
                        DvdMenu::Root => 3,
                        DvdMenu::Subtitle => 4,
                        DvdMenu::Audio => 5,
                        DvdMenu::Angle => 6,
                        DvdMenu::Chapter => 7,
                    },
                ),
                DvdCommand::Back => {
                    if sys::dvdnav_is_domain_vts(dvd) == 0 {
                        // From a menu: resume the title it was called from.
                        let r = sys::dvdnav_menu_call(dvd, 0);
                        if r == sys::OK { r } else { sys::dvdnav_go_up(dvd) }
                    } else {
                        sys::dvdnav_go_up(dvd)
                    }
                }
                DvdCommand::PlayTitle(n) => sys::dvdnav_title_play(dvd, n as i32),
                DvdCommand::Angle(n) => sys::dvdnav_angle_change(dvd, n as i32),
            };
            r == sys::OK
        };
        g.nav_changed = true;
        drop(g);
        log::debug!("DVD command {cmd:?}: {}", if ok { "ok" } else { "refused" });
        if !matches!(cmd, DvdCommand::Hover(..)) {
            self.poke();
        }
        ok
    }

    /// Updates the button highlight from libdvdnav; true if it changed.
    fn update_highlight(&self) -> bool {
        let g = self.st.lock();
        let hl = unsafe { g.highlight() };
        drop(g);
        self.overlay.set_highlight(hl)
    }

    fn dvd_state(&self, domain: DvdDomain) -> DvdState {
        let g = self.st.lock();
        let dvd = g.dvd;
        unsafe {
            let pci = sys::dvdnav_get_current_nav_pci(dvd);
            let buttons =
                if pci.is_null() || *pci.add(sys::PCI_HLI_SS) as u16 | *pci.add(sys::PCI_HLI_SS + 1) as u16 == 0 {
                    0
                } else {
                    *pci.add(sys::PCI_BTN_NS) as u32
                };
            let mut b = 0;
            sys::dvdnav_get_current_highlight(dvd, &mut b);
            let (mut angle, mut angles) = (0, 0);
            if domain == DvdDomain::Title {
                sys::dvdnav_get_angle_info(dvd, &mut angle, &mut angles);
            }
            DvdState {
                domain,
                buttons,
                button: if buttons > 0 { b.max(0) as u32 } else { 0 },
                title: if domain == DvdDomain::Title { g.title.max(0) as u32 } else { 0 },
                chapter: if domain == DvdDomain::Title { g.part.max(0) as u32 } else { 0 },
                titles: self.titles,
                angle: angle.max(0) as u32,
                angles: angles.max(0) as u32,
            }
        }
    }
}

impl State {
    /// Reads one block or event. `Some` when the reader has to stop.
    fn next_event(&mut self) -> Option<Pending> {
        let (mut ev, mut len) = (0 as c_int, 0 as c_int);
        let r = unsafe { sys::dvdnav_get_next_block(self.dvd, self.block.as_mut_ptr(), &mut ev, &mut len) };
        if r != sys::OK {
            return Some(Pending::Error(err(self.dvd)));
        }
        let len = (len.max(0) as usize).min(BLOCK);
        match ev {
            sys::BLOCK_OK => {
                let b = self.block[..len].to_vec();
                self.stash.extend_from_slice(&b);
                None
            }
            sys::NAV_PACKET => {
                self.on_nav_packet();
                None
            }
            sys::STILL_FRAME => {
                let n = i32::from_ne_bytes(self.block[0..4].try_into().unwrap());
                if self.priming && !self.data_since_hop {
                    // Nothing shown yet to hold: don't stall on it.
                    unsafe { sys::dvdnav_still_skip(self.dvd) };
                    return None;
                }
                Some(Pending::Still(n))
            }
            sys::WAIT => {
                if self.priming {
                    unsafe { sys::dvdnav_wait_skip(self.dvd) };
                    return None;
                }
                Some(Pending::Wait)
            }
            sys::HOP_CHANNEL => {
                self.rebase = true;
                self.last_e_ptm = None;
                if std::mem::take(&mut self.expect_hop) || self.priming {
                    return None;
                }
                if self.left_title() {
                    // The title's end (its post commands jumped elsewhere):
                    // what is buffered still plays.
                    return Some(Pending::Stop);
                }
                self.data_since_hop = false;
                Some(Pending::Hop)
            }
            sys::VTS_CHANGE => {
                if self.priming {
                    return None;
                }
                if self.left_title() {
                    return Some(Pending::Stop);
                }
                Some(Pending::SegmentEnd { immediate: !self.data_since_hop })
            }
            sys::CELL_CHANGE => {
                let (mut t, mut p) = (0, 0);
                unsafe { sys::dvdnav_current_title_info(self.dvd, &mut t, &mut p) };
                let in_title = unsafe { sys::dvdnav_is_domain_vts(self.dvd) } != 0;
                self.title = t;
                self.part = p;
                self.nav_changed = true;
                if !self.priming && self.left_title() {
                    return Some(Pending::Stop);
                }
                if in_title && !self.priming {
                    match self.seg_title {
                        Some(st) if st != t => {
                            return Some(Pending::SegmentEnd { immediate: !self.data_since_hop });
                        }
                        None => self.seg_title = Some(t),
                        _ => {}
                    }
                }
                None
            }
            sys::SPU_CLUT_CHANGE => {
                let mut clut = [0u32; 16];
                for (i, c) in clut.iter_mut().enumerate() {
                    *c = u32::from_ne_bytes(self.block[i * 4..i * 4 + 4].try_into().unwrap());
                }
                self.clut = Some(clut);
                None
            }
            sys::SPU_STREAM_CHANGE => {
                self.vm_spu = i32::from_ne_bytes(self.block[0..4].try_into().unwrap());
                self.spu_changed = true;
                None
            }
            sys::AUDIO_STREAM_CHANGE => {
                self.vm_audio = i32::from_ne_bytes(self.block[0..4].try_into().unwrap());
                self.audio_changed = true;
                None
            }
            sys::HIGHLIGHT => {
                self.nav_changed = true;
                None
            }
            sys::STOP => Some(Pending::Stop),
            _ => None,
        }
    }

    /// Playing a single title, and the navigator has gone somewhere else.
    fn left_title(&self) -> bool {
        let Some(only) = self.only_title else { return false };
        let (mut t, mut p) = (0, 0);
        unsafe {
            sys::dvdnav_is_domain_vts(self.dvd) == 0
                || sys::dvdnav_current_title_info(self.dvd, &mut t, &mut p) != sys::OK
                || t != only
        }
    }

    /// A new VOBU: work out its timestamp offset.
    fn on_nav_packet(&mut self) {
        let pci = unsafe { sys::dvdnav_get_current_nav_pci(self.dvd) };
        if pci.is_null() {
            return;
        }
        let rd =
            |o: usize| unsafe { u32::from_ne_bytes(std::slice::from_raw_parts(pci.add(o), 4).try_into().unwrap()) };
        let (s, e) = (rd(sys::PCI_VOBU_S_PTM) as i64, rd(sys::PCI_VOBU_E_PTM) as i64);
        if self.rebase {
            // Title (or menu program) time, from the disc's own clock.
            let t = unsafe { sys::dvdnav_get_current_time(self.dvd) }.max(0);
            self.offset = t - s;
            self.rebase = false;
        } else if let Some(le) = self.last_e_ptm
            && s != le
        {
            // Timestamps restarted (a new cell): continue seamlessly.
            self.offset += le - s;
        }
        self.last_e_ptm = Some(e);
        let pos = self.pos + self.stash.len() as i64;
        if self.offsets.back().is_none_or(|&(_, o)| o != self.offset) {
            self.offsets.push_back((pos, self.offset));
        }
        self.nav_changed = true;
    }

    /// The highlighted button's rectangle and colours, if a menu shows one.
    unsafe fn highlight(&self) -> Option<Highlight> {
        unsafe {
            let pci = sys::dvdnav_get_current_nav_pci(self.dvd);
            if pci.is_null() {
                return None;
            }
            let ss = *pci.add(sys::PCI_HLI_SS) as u16 | *pci.add(sys::PCI_HLI_SS + 1) as u16;
            if ss == 0 || *pci.add(sys::PCI_BTN_NS) == 0 {
                return None;
            }
            let mut b = 0;
            if sys::dvdnav_get_current_highlight(self.dvd, &mut b) != sys::OK || b <= 0 {
                return None;
            }
            let mut hl = sys::HighlightArea::default();
            if sys::dvdnav_get_highlight_area(pci, b, 0, &mut hl) != sys::OK {
                return None;
            }
            Some(Highlight {
                sx: hl.sx as u32,
                sy: hl.sy as u32,
                ex: hl.ex as u32,
                ey: hl.ey as u32,
                palette: hl.palette,
            })
        }
    }
}

/// What libdvdnav changed since the segment loop last looked.
struct Changes {
    audio: Option<i32>,
    spu: Option<i32>,
    nav: bool,
    clut: Option<[u32; 16]>,
}

impl Nav {
    fn take_changes(&self) -> Changes {
        let mut g = self.st.lock();
        Changes {
            audio: std::mem::take(&mut g.audio_changed).then_some(g.vm_audio),
            spu: std::mem::take(&mut g.spu_changed).then_some(g.vm_spu),
            nav: std::mem::take(&mut g.nav_changed),
            clut: g.clut.take(),
        }
    }

    fn still_skip(&self) {
        unsafe { sys::dvdnav_still_skip(self.st.lock().dvd) };
    }

    fn wait_skip(&self) {
        unsafe { sys::dvdnav_wait_skip(self.st.lock().dvd) };
    }

    /// Jumps to `t` seconds into the current title.
    fn seek(&self, t: f64) -> bool {
        let mut g = self.st.lock();
        // A little early: the decoders skip ahead to the exact time.
        let ticks = ((t - 0.4).max(0.0) * 90000.0) as u64;
        let ok = unsafe { sys::dvdnav_jump_to_sector_by_time(g.dvd, ticks, 0) } == sys::OK;
        if ok {
            g.stash.clear();
            g.pending = None;
            g.rebase = true;
            g.last_e_ptm = None;
            g.expect_hop = true;
            g.offsets.clear();
        }
        ok
    }

    fn set_audio(&self, logical: u8) {
        unsafe { sys::dvdnav_set_active_stream(self.st.lock().dvd, logical, sys::AUDIO_STREAM) };
    }

    /// Starts title `n` (before the first segment, to resume a reload).
    fn play_title(&self, n: u32) -> bool {
        let g = self.st.lock();
        let ok = unsafe { sys::dvdnav_title_play(g.dvd, n as i32) } == sys::OK;
        if !ok {
            log::debug!("DVD: can't resume title {n}: {}", err(g.dvd));
        }
        ok
    }

    fn restart(&self) {
        let mut g = self.st.lock();
        match g.only_title {
            Some(t) => unsafe {
                sys::dvdnav_title_play(g.dvd, t);
            },
            None => unsafe {
                sys::dvdnav_reset(g.dvd);
            },
        }
        g.stash.clear();
        g.pending = None;
        g.rebase = true;
    }

    fn set_angle(&self, angle: u32) {
        let g = self.st.lock();
        if unsafe { sys::dvdnav_angle_change(g.dvd, angle as i32) } != sys::OK {
            log::debug!("DVD: can't switch to angle {angle}: {}", err(g.dvd));
        }
    }

    /// Reads up to the first picture data of a new segment and describes it.
    fn begin_segment(&self) -> SegmentVm {
        let mut g = self.st.lock();
        g.pending = None;
        g.priming = true;
        g.seg_title = None;
        g.rebase = true;
        g.last_e_ptm = None;
        g.offsets.clear();
        for _ in 0..100_000 {
            if !g.stash.is_empty() || g.pending.is_some() {
                break;
            }
            if let Some(p) = g.next_event() {
                g.pending = Some(p);
            }
        }
        g.priming = false;
        let dvd = g.dvd;
        unsafe {
            let domain = if sys::dvdnav_is_domain_vts(dvd) != 0 {
                DvdDomain::Title
            } else if sys::dvdnav_is_domain_fp(dvd) != 0 {
                DvdDomain::FirstPlay
            } else {
                DvdDomain::Menu
            };
            let (mut t, mut p) = (0, 0);
            sys::dvdnav_current_title_info(dvd, &mut t, &mut p);
            g.title = t;
            g.part = p;
            if domain == DvdDomain::Title {
                g.seg_title = Some(t);
            }
            let (mut w, mut h) = (0u32, 0u32);
            if sys::dvdnav_get_video_resolution(dvd, &mut w, &mut h) != sys::OK || w == 0 || h == 0 {
                (w, h) = (720, 480);
            }
            let wide = sys::dvdnav_get_video_aspect(dvd) == 3;
            let mut audio = Vec::new();
            for l in 0..8u8 {
                let phys = sys::dvdnav_get_audio_logical_stream(dvd, l);
                if phys < 0 {
                    continue;
                }
                let phys = phys as i32 & 7;
                let (id, codec) = match sys::dvdnav_audio_stream_format(dvd, l) {
                    0 => (0x80 + phys, ff::AVCodecID::AV_CODEC_ID_AC3),
                    2 | 3 => (0x1c0 + phys, ff::AVCodecID::AV_CODEC_ID_MP2),
                    4 => (0xa0 + phys, ff::AVCodecID::AV_CODEC_ID_PCM_DVD),
                    6 => (0x88 + phys, ff::AVCodecID::AV_CODEC_ID_DTS),
                    _ => continue,
                };
                if audio.iter().any(|a: &AudioStream| a.id == id) {
                    continue;
                }
                audio.push(AudioStream {
                    id,
                    phys,
                    logical: l,
                    codec,
                    channels: sys::dvdnav_audio_stream_channels(dvd, l).clamp(1, 8) as i32,
                    lang: lang_code(sys::dvdnav_audio_stream_to_lang(dvd, l)),
                });
            }
            let mut spu: Vec<(i32, Option<String>)> = Vec::new();
            for l in 0..32u8 {
                let phys = sys::dvdnav_get_spu_logical_stream(dvd, l);
                if phys < 0 {
                    continue;
                }
                let id = 0x20 + (phys as i32 & 0x1f);
                if !spu.iter().any(|s| s.0 == id) {
                    spu.push((id, lang_code(sys::dvdnav_spu_stream_to_lang(dvd, l))));
                }
            }
            // The stream the disc's program shows (menu buttons) is always there.
            if g.vm_spu >= 0 && !spu.iter().any(|s| s.0 == 0x20 + (g.vm_spu & 0x1f)) {
                spu.push((0x20 + (g.vm_spu & 0x1f), None));
            }
            let mut chapters = Vec::new();
            let mut duration = 0.0;
            if domain == DvdDomain::Title && t > 0 {
                let mut times: *mut u64 = ptr::null_mut();
                let mut dur = 0u64;
                let n = sys::dvdnav_describe_title_chapters(dvd, t, &mut times, &mut dur);
                if !times.is_null() {
                    // `times` are the chapters' ends.
                    let ends = std::slice::from_raw_parts(times, n as usize);
                    let mut start = 0.0;
                    for &e in ends {
                        let e = e as f64 / 90000.0;
                        if e > start {
                            chapters.push((start, e));
                        }
                        start = e;
                    }
                    sys::free(times as *mut c_void);
                }
                duration = dur as f64 / 90000.0;
            }
            SegmentVm {
                domain,
                title: t.max(0) as u32,
                width: w,
                height: h,
                wide,
                audio,
                spu,
                vm_audio: g.vm_audio,
                vm_spu: g.vm_spu,
                chapters,
                duration,
            }
        }
    }
}

struct AudioStream {
    /// MPEG program stream id (0x80 + n for AC-3, …).
    id: i32,
    phys: i32,
    logical: u8,
    codec: ff::AVCodecID,
    channels: i32,
    lang: Option<String>,
}

/// The disc's description of a segment.
struct SegmentVm {
    domain: DvdDomain,
    title: u32,
    width: u32,
    height: u32,
    wide: bool,
    audio: Vec<AudioStream>,
    /// Subpicture streams: program stream id, language.
    spu: Vec<(i32, Option<String>)>,
    vm_audio: i32,
    vm_spu: i32,
    chapters: Vec<(f64, f64)>,
    duration: f64,
}

/// The demuxer's input: blocks from libdvdnav.
struct NavIo {
    nav: Arc<Nav>,
    ctx: Arc<SessionCtx>,
}

impl IoSource for NavIo {
    fn read(&mut self, buf: &mut [u8]) -> c_int {
        let ctx = &self.ctx;
        self.nav.read(buf, &|| ctx.alive())
    }
}

struct Segment {
    /// Streams as the pipeline sees them (from the disc's attributes).
    outer: FormatCtx,
    /// The MPEG-PS demuxer reading libdvdnav's blocks.
    inner: FormatCtx,
    info: Arc<MediaInfo>,
    /// Program stream id → outer stream index.
    ids: HashMap<i32, usize>,
    vm: SegmentVm,
    video: Option<usize>,
}

impl Segment {
    fn audio_for_phys(&self, phys: i32) -> Option<usize> {
        let a = self.vm.audio.iter().find(|a| a.phys == phys)?;
        self.ids.get(&a.id).copied()
    }
    fn spu_for_phys(&self, phys: i32) -> Option<usize> {
        self.ids.get(&(0x20 + (phys & 0x1f))).copied()
    }
    fn logical_audio(&self, idx: usize) -> Option<u8> {
        self.vm.audio.iter().find(|a| self.ids.get(&a.id) == Some(&idx)).map(|a| a.logical)
    }
}

fn open_segment(ctx: &Arc<SessionCtx>, nav: &Arc<Nav>, source: &MediaSource) -> anyhow::Result<Segment> {
    let vm = nav.begin_segment();
    unsafe {
        // The demuxer over libdvdnav's blocks.
        let io = NavIo { nav: nav.clone(), ctx: ctx.clone() };
        let mut ic = av::custom_io_context(Box::new(io), BLOCK)?;
        (*ic).interrupt_callback.callback = Some(demux::interrupt_cb);
        (*ic).interrupt_callback.opaque = Arc::as_ptr(ctx) as *mut c_void;
        // The disc describes the streams; this only needs their details.
        (*ic).probesize = 512 * 1024;
        (*ic).max_analyze_duration = 500_000;
        let fmt = ff::av_find_input_format(av::cstr("mpeg").as_ptr());
        let io_ptr = (*ic).opaque;
        let empty = av::cstr("");
        let mut opts = Dict::new();
        let r = ff::avformat_open_input(&mut ic, empty.as_ptr(), fmt, &mut opts.0);
        if r < 0 {
            av::free_custom_io(io_ptr);
            anyhow::bail!("could not read the DVD: {}", av::err_str(r));
        }
        let inner = FormatCtx(ic);
        ff::avformat_find_stream_info(ic, ptr::null_mut());

        let oc = ff::avformat_alloc_context();
        if oc.is_null() {
            anyhow::bail!("out of memory");
        }
        let outer = FormatCtx(oc);
        let inner_stream = |id: i32| {
            (0..inner.nb_streams())
                .map(|i| inner.stream(i))
                .find(|&s| (*s).id == id && (*(*s).codecpar).codec_id != ff::AVCodecID::AV_CODEC_ID_NONE)
        };
        let mut ids = HashMap::new();
        let mut add = |id: i32, lang: Option<&str>| -> *mut ff::AVStream {
            let st = ff::avformat_new_stream(oc, ptr::null());
            (*st).id = id;
            (*st).time_base = ff::AVRational { num: 1, den: 90000 };
            if let Some(l) = lang {
                ff::av_dict_set(&mut (*st).metadata, av::cstr("language").as_ptr(), av::cstr(l).as_ptr(), 0);
            }
            ids.insert(id, (*st).index as usize);
            st
        };
        // Video.
        let st = add(0x1e0, None);
        let video = Some((*st).index as usize);
        match inner_stream(0x1e0) {
            Some(src) => {
                ff::avcodec_parameters_copy((*st).codecpar, (*src).codecpar);
                (*st).avg_frame_rate = (*src).avg_frame_rate;
                (*st).r_frame_rate = (*src).r_frame_rate;
                (*st).sample_aspect_ratio = (*src).sample_aspect_ratio;
            }
            None => {
                let par = &mut *(*st).codecpar;
                par.codec_type = ff::AVMediaType::AVMEDIA_TYPE_VIDEO;
                par.codec_id = ff::AVCodecID::AV_CODEC_ID_MPEG2VIDEO;
                par.width = vm.width as c_int;
                par.height = vm.height as c_int;
                let (dw, dh) = if vm.wide { (16, 9) } else { (4, 3) };
                par.sample_aspect_ratio = ff::AVRational { num: dw * vm.height as c_int, den: dh * vm.width as c_int };
                let fr = if vm.height == 576 {
                    ff::AVRational { num: 25, den: 1 }
                } else {
                    ff::AVRational { num: 30000, den: 1001 }
                };
                (*st).avg_frame_rate = fr;
                (*st).r_frame_rate = fr;
            }
        }
        // Audio.
        for a in &vm.audio {
            let st = add(a.id, a.lang.as_deref());
            match inner_stream(a.id) {
                Some(src) => {
                    ff::avcodec_parameters_copy((*st).codecpar, (*src).codecpar);
                }
                None => {
                    let par = &mut *(*st).codecpar;
                    par.codec_type = ff::AVMediaType::AVMEDIA_TYPE_AUDIO;
                    par.codec_id = a.codec;
                    par.sample_rate = 48000;
                    ff::av_channel_layout_default(&mut par.ch_layout, a.channels);
                }
            }
        }
        // Subpictures.
        for (id, lang) in &vm.spu {
            let st = add(*id, lang.as_deref());
            let par = &mut *(*st).codecpar;
            par.codec_type = ff::AVMediaType::AVMEDIA_TYPE_SUBTITLE;
            par.codec_id = ff::AVCodecID::AV_CODEC_ID_DVD_SUBTITLE;
            par.width = vm.width as c_int;
            par.height = vm.height as c_int;
        }

        let mut info = demux::build_media_info(&outer, source);
        info.format_name = "dvdnav".into();
        info.format_long_name = if nav.only.is_some() { "DVD-Video" } else { "DVD-Video with menus" }.into();
        info.duration = vm.duration;
        info.start_time = 0.0;
        info.seekable = vm.domain == DvdDomain::Title && vm.duration > 0.0;
        info.title = nav.disc_title.clone();
        info.chapters = vm
            .chapters
            .iter()
            .enumerate()
            .map(|(i, &(start, end))| Chapter { start, end, title: format!("Chapter {}", i + 1) })
            .collect();
        Ok(Segment { outer, inner, info: Arc::new(info), ids, vm, video })
    }
}

enum Outcome {
    /// The next segment (menu, title) starts.
    Next,
    /// The disc's program stopped.
    Ended,
    Aborted,
}

#[derive(Clone, Copy, Debug)]
enum After {
    Still(i32),
    WaitSkip,
    Next,
    End,
}

enum Phase {
    Reading,
    /// Waiting until everything queued has been played.
    Draining(After, Instant),
    /// Holding a still picture until this time (`None`: until a button).
    Still(Option<Instant>),
}

/// Plays a DVD with its menus, or one of its titles (`dvd://N`): the demux
/// thread of such a session.
pub fn run(ctx: Arc<SessionCtx>, source: MediaSource, path: String, opts: &LoadOptions) {
    let cfg = ctx.controls.config.lock().clone();
    let (only, angle) = match &source {
        MediaSource::Dvd { title, angle, .. } => ((*title > 0).then_some(*title), *angle),
        _ => (None, 0),
    };
    let opened = Nav::open(&path, &cfg, only).and_then(|n| match only {
        Some(t) if !n.play_title(t) => Err(anyhow::anyhow!("Could not play title {t} of the DVD")),
        _ => Ok(n),
    });
    let nav = match opened {
        Ok(n) => Arc::new(n),
        Err(e) => {
            if ctx.alive() {
                let msg = e.to_string();
                ctx.update_state(|st| st.status = Status::Error(msg.clone()));
                ctx.emit(PlayerEvent::Error(msg));
            }
            return;
        }
    };
    *ctx.nav.lock() = Some(nav.clone());
    *ctx.spu_overlay.lock() = Some(nav.overlay.clone());
    if only.is_some() && angle > 1 {
        nav.set_angle(angle);
    }
    // A reload (hardware decoding toggled, …) goes back to the same title
    // and time rather than through the first-play program and the menus.
    let mut resume = match only {
        Some(t) => Some((t, opts.start_at)),
        None => opts.dvd_title.filter(|t| *t > 0 && nav.play_title(*t)).map(|t| (t, opts.start_at)),
    };
    let mut first = true;
    while ctx.alive() {
        let seg = match open_segment(&ctx, &nav, &source) {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                ctx.update_state(|st| st.status = Status::Error(msg.clone()));
                ctx.emit(PlayerEvent::Error(msg));
                break;
            }
        };
        let cfg = ctx.controls.config.lock().clone();
        // Only if the title's own program didn't go somewhere else first.
        let start_at = resume
            .take()
            .filter(|(t, _)| seg.vm.domain == DvdDomain::Title && seg.vm.title == *t)
            .and_then(|(_, at)| at);
        // A single title whose own program went elsewhere before it began.
        let outcome = if only.is_some_and(|t| seg.vm.domain != DvdDomain::Title || seg.vm.title != t) {
            Outcome::Ended
        } else {
            play_segment(&ctx, &nav, seg, &cfg, first, start_at)
        };
        match outcome {
            Outcome::Next => first = false,
            Outcome::Aborted => break,
            Outcome::Ended => {
                // A single title in a playlist hands over to the next item
                // (unless the audio output already did).
                let advanced = only.is_some() && (!ctx.is_current() || super::advance(&ctx));
                if !advanced {
                    ctx.update_state(|st| st.status = Status::Ended);
                    ctx.emit(PlayerEvent::EndOfFile);
                }
                // Play (a seek to the start) or a menu/title command starts
                // again (a single title from its start).
                while ctx.alive() {
                    // Release the request lock before performing potentially slow seeks.
                    let request = ctx.seek_req.lock().take();
                    if let Some(req) = request {
                        ctx.seeking.store(false, Ordering::Release);
                        nav.restart();
                        // A single title goes to the time asked for.
                        resume = only.map(|t| (t, Some(req.target)));
                        break;
                    }
                    if nav.poked.swap(false, Ordering::AcqRel) {
                        break;
                    }
                    let mut g = ctx.demux_lock.lock();
                    ctx.demux_cv.wait_for(&mut g, Duration::from_millis(50));
                }
            }
        }
    }
    ctx.vq.abort();
    ctx.aq.abort();
    ctx.sq.abort();
    ctx.frames.abort();
    ctx.abuf.abort();
}

/// Rewinds the demuxer after it stopped at an event; `discard` drops
/// what it had buffered (after a jump).
fn reset_inner(inner: &FormatCtx, discard: bool) {
    unsafe {
        let ic = inner.0;
        let pb = (*ic).pb;
        if !pb.is_null() {
            (*pb).eof_reached = 0;
            (*pb).error = 0;
            if discard {
                (*pb).buf_ptr = (*pb).buf_end;
            }
        }
        ff::avformat_flush(ic);
    }
}

fn flush_all(ctx: &SessionCtx) {
    ctx.vq.flush();
    ctx.aq.flush();
    ctx.sq.flush();
    ctx.abuf.flush(ctx.aq.serial());
    *ctx.seek_target.lock() = None;
}

fn stop_component(ctx: &SessionCtx, kind: TrackKind, c: Component) {
    match kind {
        TrackKind::Audio => {
            ctx.audio_active.store(false, Ordering::Release);
            ctx.aq.flush();
            ctx.abuf.flush(ctx.aq.serial());
        }
        TrackKind::Video => {
            ctx.video_active.store(false, Ordering::Release);
            ctx.vq.flush();
            ctx.frames.clear();
        }
        TrackKind::Subtitle => {
            ctx.sq.flush();
            ctx.subs.clear_embedded();
        }
    }
    c.stop();
}

struct Components {
    video: Option<Component>,
    audio: Option<Component>,
    sub: Option<Component>,
}

impl Components {
    fn slot(&mut self, kind: TrackKind) -> &mut Option<Component> {
        match kind {
            TrackKind::Video => &mut self.video,
            TrackKind::Audio => &mut self.audio,
            TrackKind::Subtitle => &mut self.sub,
        }
    }

    /// Makes `kind` decode stream `target` (or nothing).
    fn switch(
        &mut self,
        ctx: &Arc<SessionCtx>,
        ic: &FormatCtx,
        kind: TrackKind,
        target: Option<usize>,
        cfg: &EngineConfig,
    ) {
        let slot = self.slot(kind);
        if slot.as_ref().map(|c| c.stream) == target {
            return;
        }
        if let Some(c) = slot.take() {
            stop_component(ctx, kind, c);
        }
        if let Some(i) = target {
            match demux::start_component(ctx, ic, kind, i, cfg) {
                Ok(c) => *slot = Some(c),
                Err(e) => log::warn!("DVD {kind:?} stream: {e}"),
            }
        }
    }

    fn stop(self, ctx: &SessionCtx) {
        for (kind, c) in
            [(TrackKind::Video, self.video), (TrackKind::Audio, self.audio), (TrackKind::Subtitle, self.sub)]
        {
            if let Some(c) = c {
                stop_component(ctx, kind, c);
            }
        }
    }

    /// Asks the decoders to finish what they have (before a still, a wait
    /// or the end of a segment).
    fn drain(&self, ctx: &SessionCtx) {
        if self.video.is_some() {
            ctx.video_finished.store(-1, Ordering::Release);
            ctx.vq.put_drain();
        }
        if self.audio.is_some() {
            ctx.audio_finished.store(-1, Ordering::Release);
            ctx.aq.put_drain();
        }
        ctx.eof.store(true, Ordering::Release);
    }

    fn drained(&self, ctx: &SessionCtx) -> bool {
        let audio = self.audio.is_none()
            || (ctx.audio_finished.load(Ordering::Acquire) == ctx.aq.serial() && ctx.abuf.is_empty());
        let video = self.video.is_none()
            || !ctx.video_active.load(Ordering::Acquire)
            || (ctx.video_finished.load(Ordering::Acquire) == ctx.vq.serial() && ctx.frames.len() == 0);
        audio && video
    }
}

/// Which subtitle stream to decode, whether only its forced pictures show,
/// and the track to report as selected.
fn route_subs(
    seg: &Segment,
    vm_spu: i32,
    user: Option<Option<usize>>,
    cfg: &EngineConfig,
) -> (Option<usize>, bool, Option<usize>) {
    let vm_stream = (vm_spu >= 0).then(|| seg.spu_for_phys(vm_spu)).flatten();
    let vm_shown = vm_spu >= 0 && vm_spu & 0x80 == 0;
    if seg.vm.domain != DvdDomain::Title {
        // Menus: the disc's subpicture carries the buttons.
        return (vm_stream, false, None);
    }
    match user {
        Some(Some(i)) => (Some(i), false, Some(i)),
        Some(None) => (vm_stream, true, None),
        None if cfg.show_subs => match stream_index(pick_subtitle(&seg.info, &cfg.sub_langs)) {
            Some(i) => (Some(i), false, Some(i)),
            None => (vm_stream, true, None),
        },
        None if vm_shown => (vm_stream, false, vm_stream),
        None => (vm_stream, true, None),
    }
}

fn play_segment(
    ctx: &Arc<SessionCtx>,
    nav: &Arc<Nav>,
    seg: Segment,
    cfg: &EngineConfig,
    first: bool,
    start_at: Option<f64>,
) -> Outcome {
    // A clean slate for the new stream set.
    flush_all(ctx);
    ctx.frames.clear();
    ctx.subs.clear_embedded();
    nav.overlay.clear();
    ctx.eof.store(false, Ordering::Release);
    ctx.start_time.set(0.0);
    ctx.duration.set(seg.info.duration);
    // Shown until the new segment's clock runs (a silent still menu has none).
    ctx.seek_display.set(0.0);
    ctx.seeking.store(false, Ordering::Release);
    nav.overlay.set_canvas(seg.vm.width, seg.vm.height);
    for i in 0..seg.outer.nb_streams() {
        unsafe { (*seg.outer.stream(i)).discard = ff::AVDiscard::AVDISCARD_ALL };
    }
    let domain = seg.vm.domain;
    let info = seg.info.clone();
    log::debug!(
        "DVD segment: {domain:?} title {} ({:.0} s, {} audio, {} subpicture streams)",
        seg.vm.title,
        seg.vm.duration,
        seg.vm.audio.len(),
        seg.vm.spu.len()
    );

    let mut user_sub: Option<Option<usize>> = None;
    let mut selected = Selected {
        video: seg.video.map(TrackId::Stream),
        audio: seg
            .audio_for_phys(seg.vm.vm_audio)
            .map(TrackId::Stream)
            .or_else(|| info.tracks_of(TrackKind::Audio).next().map(|t| t.id)),
        subtitle: None,
    };
    let (sub_stream, forced, shown) = route_subs(&seg, seg.vm.vm_spu, user_sub, cfg);
    nav.overlay.forced_only.store(forced, Ordering::Release);
    selected.subtitle = shown.map(TrackId::Stream);

    let mut comps = Components { video: None, audio: None, sub: None };
    comps.switch(ctx, &seg.outer, TrackKind::Audio, stream_index(selected.audio), cfg);
    if comps.audio.is_none() {
        selected.audio = None;
    }
    comps.switch(ctx, &seg.outer, TrackKind::Video, seg.video, cfg);
    comps.switch(ctx, &seg.outer, TrackKind::Subtitle, sub_stream, cfg);

    let paused = ctx.paused();
    let mut dvd_state = nav.dvd_state(domain);
    ctx.update_state(|st| {
        st.media = Some(info.clone());
        st.selected = selected.clone();
        st.status = if paused { Status::Paused } else { Status::Playing };
        // A single title is played like a file: no menu controls.
        st.dvd = nav.only.is_none().then(|| dvd_state.clone());
    });
    if first {
        log::debug!("startup: DVD navigation ready at {:.0} ms", crate::ms_since_start());
    }
    ctx.emit(PlayerEvent::Loaded);
    if let Some(t) = start_at.filter(|t| *t > 0.5 && info.seekable) {
        ctx.request_seek(SeekRequest { target: t, relative: false, precise: true, direction: 0, origin: 0.0 });
    }

    let mut phase = Phase::Reading;
    let pkt = Packet::new();
    // Latest timestamps queued (video, audio), for the read-ahead limit.
    let mut queued_to = [f64::NAN; 2];
    let mut held = HeldAudio::default();
    let mut read_errors = 0;
    loop {
        if !ctx.alive() {
            comps.stop(ctx);
            return Outcome::Aborted;
        }

        // Track switches from the UI.
        let switches: Vec<_> = std::mem::take(&mut *ctx.switch_req.lock());
        if !switches.is_empty() {
            let pos = ctx.position();
            for (kind, id) in switches {
                let target = match kind {
                    TrackKind::Subtitle => {
                        user_sub = Some(stream_index(id));
                        let (s, forced, shown) = route_subs(&seg, nav.st.lock().vm_spu, user_sub, cfg);
                        nav.overlay.forced_only.store(forced, Ordering::Release);
                        selected.subtitle = shown.map(TrackId::Stream);
                        s
                    }
                    TrackKind::Audio => {
                        let t = stream_index(id);
                        if let Some(l) = t.and_then(|i| seg.logical_audio(i)) {
                            nav.set_audio(l);
                        }
                        selected.audio = id;
                        t
                    }
                    TrackKind::Video => stream_index(id),
                };
                comps.switch(ctx, &seg.outer, kind, target, cfg);
            }
            let sel = selected.clone();
            ctx.update_state(|st| st.selected = sel);
            ctx.emit(PlayerEvent::TracksChanged);
            if info.seekable {
                ctx.request_seek(SeekRequest {
                    target: pos,
                    relative: false,
                    precise: true,
                    direction: 0,
                    origin: 0.0,
                });
            }
        }

        // Seeks (titles only).
        // Release the request lock before performing potentially slow seeks.
        let request = ctx.seek_req.lock().take();
        if let Some(req) = request {
            let dur = ctx.duration.get();
            let target = req.target.clamp(0.0, (dur - 0.5).max(0.0));
            if info.seekable && nav.seek(target) {
                reset_inner(&seg.inner, true);
                flush_all(ctx);
                let serial = ctx.vq.serial().max(ctx.aq.serial());
                *ctx.seek_target.lock() = if req.precise { Some((target, serial)) } else { None };
                ctx.vclk.set(target, ctx.vq.serial());
                ctx.eof.store(false, Ordering::Release);
                queued_to = [f64::NAN; 2];
                phase = Phase::Reading;
                if ctx.paused() {
                    ctx.show_one.store(true, Ordering::Release);
                }
                if comps.video.is_none() || !ctx.video_active.load(Ordering::Acquire) {
                    ctx.seeking.store(false, Ordering::Release);
                }
                ctx.update_state(|st| {
                    if st.status == Status::Ended {
                        st.status = if ctx.paused() { Status::Paused } else { Status::Playing };
                    }
                });
            } else {
                ctx.seeking.store(false, Ordering::Release);
                ctx.update_state(|st| st.seeking = false);
            }
            ctx.wake();
        }

        // What the disc's program changed.
        let ch = nav.take_changes();
        if let Some(c) = ch.clut {
            nav.overlay.set_clut(&c);
            nav.overlay.refresh(&ctx.subs);
        }
        let mut tracks_changed = false;
        if let Some(a) = ch.audio
            && let Some(i) = seg.audio_for_phys(a)
            && comps.audio.as_ref().map(|c| c.stream) != Some(i)
        {
            comps.switch(ctx, &seg.outer, TrackKind::Audio, Some(i), cfg);
            selected.audio = comps.audio.as_ref().map(|c| TrackId::Stream(c.stream));
            tracks_changed = true;
        }
        if let Some(s) = ch.spu
            && user_sub.is_none()
        {
            let (stream, forced, shown) = route_subs(&seg, s, None, cfg);
            nav.overlay.forced_only.store(forced, Ordering::Release);
            if comps.sub.as_ref().map(|c| c.stream) != stream || selected.subtitle != shown.map(TrackId::Stream) {
                comps.switch(ctx, &seg.outer, TrackKind::Subtitle, stream, cfg);
                selected.subtitle = shown.map(TrackId::Stream);
                tracks_changed = true;
            }
        }
        if tracks_changed {
            let sel = selected.clone();
            ctx.update_state(|st| st.selected = sel);
            ctx.emit(PlayerEvent::TracksChanged);
        }
        if ch.nav {
            if nav.update_highlight() {
                nav.overlay.refresh(&ctx.subs);
                ctx.wake();
            }
            let s = nav.dvd_state(domain);
            if s != dvd_state && nav.only.is_none() {
                dvd_state = s.clone();
                ctx.update_state(|st| st.dvd = Some(s));
                ctx.wake();
            }
        }

        match phase {
            Phase::Reading => {
                if read_ahead_full(ctx, &comps, &queued_to) {
                    update_buffering(ctx, comps.audio.is_some());
                    let mut g = ctx.demux_lock.lock();
                    ctx.demux_cv.wait_for(&mut g, Duration::from_millis(10));
                    continue;
                }
                let r = unsafe { ff::av_read_frame(seg.inner.0, pkt.0) };
                if r >= 0 {
                    read_errors = 0;
                    route_packet(ctx, nav, &seg, &comps, &pkt, &mut queued_to, &mut held);
                    update_buffering(ctx, comps.audio.is_some());
                    continue;
                }
                if r == ff::AVERROR_EXIT || !ctx.alive() {
                    comps.stop(ctx);
                    return Outcome::Aborted;
                }
                let pending = nav.take_pending();
                reset_inner(&seg.inner, matches!(pending, Some(Pending::Hop)));
                let after = match pending {
                    Some(Pending::Hop) => {
                        // A jump: drop everything on its way to the screen.
                        ctx.seek_display.set(ctx.position());
                        flush_all(ctx);
                        nav.overlay.clear();
                        queued_to = [f64::NAN; 2];
                        continue;
                    }
                    Some(Pending::Still(n)) => After::Still(n),
                    Some(Pending::Wait) => After::WaitSkip,
                    Some(Pending::SegmentEnd { immediate: true }) => {
                        comps.stop(ctx);
                        return Outcome::Next;
                    }
                    Some(Pending::SegmentEnd { immediate: false }) => After::Next,
                    Some(Pending::Stop) => After::End,
                    Some(Pending::Error(e)) => {
                        log::warn!("DVD read error: {e}");
                        ctx.emit(PlayerEvent::Warning(format!("DVD read error: {e}")));
                        After::End
                    }
                    None => {
                        // A demuxer hiccup rather than an event: retry a few times.
                        read_errors += 1;
                        if read_errors < 50 {
                            continue;
                        }
                        After::End
                    }
                };
                held.release(ctx, &comps, None);
                comps.drain(ctx);
                phase = Phase::Draining(after, Instant::now());
            }
            Phase::Draining(after, since) => {
                update_buffering(ctx, comps.audio.is_some());
                if nav.only.is_some() && matches!(after, After::End) {
                    super::preload_next(ctx);
                }
                let poked_still = matches!(after, After::Still(_)) && nav.poked.swap(false, Ordering::AcqRel);
                // Paused or stuck decoders: don't wait forever.
                let since = if ctx.paused() { Instant::now() } else { since };
                if poked_still {
                    ctx.eof.store(false, Ordering::Release);
                    phase = Phase::Reading;
                    continue;
                }
                if !comps.drained(ctx) && since.elapsed() < Duration::from_secs(15) {
                    phase = Phase::Draining(after, since);
                    let mut g = ctx.demux_lock.lock();
                    ctx.demux_cv.wait_for(&mut g, Duration::from_millis(10));
                    continue;
                }
                match after {
                    After::Still(n) => {
                        let until = (0..255).contains(&n).then(|| Instant::now() + Duration::from_secs(n as u64));
                        phase = Phase::Still(until);
                    }
                    After::WaitSkip => {
                        nav.wait_skip();
                        ctx.eof.store(false, Ordering::Release);
                        phase = Phase::Reading;
                    }
                    After::Next => {
                        comps.stop(ctx);
                        return Outcome::Next;
                    }
                    After::End => {
                        comps.stop(ctx);
                        return Outcome::Ended;
                    }
                }
            }
            Phase::Still(until) => {
                update_buffering(ctx, comps.audio.is_some());
                if nav.poked.swap(false, Ordering::AcqRel) {
                    ctx.eof.store(false, Ordering::Release);
                    phase = Phase::Reading;
                    continue;
                }
                // The still's timer stops while paused.
                let until = until.map(|u| if ctx.paused() { u + Duration::from_millis(10) } else { u });
                if until.is_some_and(|u| Instant::now() >= u) {
                    nav.still_skip();
                    ctx.eof.store(false, Ordering::Release);
                    phase = Phase::Reading;
                    continue;
                }
                phase = Phase::Still(until);
                let mut g = ctx.demux_lock.lock();
                ctx.demux_cv.wait_for(&mut g, Duration::from_millis(10));
            }
        }
    }
}

/// The latest audio packet, queued only once the next one shows its
/// timestamp is right. FFmpeg's parsers give a frame that ends exactly where
/// its PES packet does the next packet's timestamp, the same as the frame
/// after it (a 192 kbit/s AC-3 track does that every 21 frames). Taken as it
/// is, the audio clock runs a frame ahead there, and FFmpeg's `dvdvideo`
/// demuxer even drops such frames as duplicates, so the sound skipped and
/// the pictures were dropped to keep up.
#[derive(Default)]
struct HeldAudio {
    /// The packet, its stream and the audio queue's serial when it was read
    /// (a flush since then makes it stale).
    pkt: Option<(Packet, usize, i32)>,
}

impl HeldAudio {
    /// Queues the held packet (if still current), its timestamps first
    /// corrected from those of `next`, the packet that follows it.
    fn release(&mut self, ctx: &SessionCtx, comps: &Components, next: Option<&Packet>) {
        let Some((mut pkt, stream, serial)) = self.pkt.take() else { return };
        if serial != ctx.aq.serial() || comps.audio.as_ref().map(|c| c.stream) != Some(stream) {
            return;
        }
        if let Some(next) = next {
            unsafe { fix_audio_timestamps(pkt.0, next.0) };
        }
        ctx.aq.put(pkt.take());
    }
}

/// `p` doesn't start before `next`, which follows it: it really starts its
/// own duration earlier.
unsafe fn fix_audio_timestamps(p: *mut ff::AVPacket, next: *const ff::AVPacket) {
    unsafe {
        let (pts, npts, dur) = ((*p).pts, (*next).pts, (*p).duration);
        if pts != av::NOPTS && npts != av::NOPTS && dur > 0 && pts >= npts {
            (*p).pts = npts - dur;
            if (*p).dts != av::NOPTS {
                (*p).dts = (*p).pts;
            }
        }
    }
}

/// Queues a demuxed packet for its decoder, on the segment's timeline.
fn route_packet(
    ctx: &SessionCtx,
    nav: &Nav,
    seg: &Segment,
    comps: &Components,
    pkt: &Packet,
    queued_to: &mut [f64; 2],
    held: &mut HeldAudio,
) {
    unsafe {
        let p = pkt.0;
        let ist = seg.inner.stream((*p).stream_index as usize);
        let Some(&oi) = seg.ids.get(&(*ist).id) else {
            ff::av_packet_unref(p);
            return;
        };
        let itb = (*ist).time_base;
        let to90k = |ts: i64| ff::av_rescale_q(ts, itb, ff::AVRational { num: 1, den: 90000 });
        let off = nav.offset_for((*p).pos);
        if (*p).pts != av::NOPTS {
            (*p).pts = to90k((*p).pts) + off;
        }
        if (*p).dts != av::NOPTS {
            (*p).dts = to90k((*p).dts) + off;
        }
        if (*p).duration > 0 {
            (*p).duration = to90k((*p).duration);
        }
        (*p).stream_index = oi as c_int;
        let t = if (*p).pts != av::NOPTS { (*p).pts as f64 / 90000.0 } else { f64::NAN };
        let mut pkt = Packet(ff::av_packet_alloc());
        ff::av_packet_move_ref(pkt.0, p);
        if comps.video.as_ref().is_some_and(|c| c.stream == oi) {
            if t.is_finite() {
                queued_to[0] = t;
            }
            ctx.vq.put(pkt.take());
        } else if comps.audio.as_ref().is_some_and(|c| c.stream == oi) {
            if t.is_finite() {
                queued_to[1] = t;
            }
            held.release(ctx, comps, Some(&pkt));
            held.pkt = Some((pkt, oi, ctx.aq.serial()));
        } else if comps.sub.as_ref().is_some_and(|c| c.stream == oi) {
            ctx.sq.put(pkt.take());
        }
    }
}

/// Enough is buffered: stop reading for now.
fn read_ahead_full(ctx: &SessionCtx, comps: &Components, queued_to: &[f64; 2]) -> bool {
    let total = ctx.vq.bytes() + ctx.aq.bytes() + ctx.sq.bytes();
    if total > MAX_QUEUE_BYTES {
        return true;
    }
    let pos = ctx.master_clock();
    let ahead = |q: &super::sync::PacketQueue, active: bool, to: f64| {
        !active || (q.len() > MIN_FRAMES && (!pos.is_finite() || !to.is_finite() || to - pos > READ_AHEAD_SECS))
    };
    let video = comps.video.is_some() && ctx.video_active.load(Ordering::Acquire);
    ahead(&ctx.vq, video, queued_to[0]) && ahead(&ctx.aq, comps.audio.is_some(), queued_to[1])
}

fn update_buffering(ctx: &SessionCtx, has_audio: bool) {
    if ctx.is_current()
        && let Some(mut st) = ctx.shared.state.try_lock()
    {
        let empty = (has_audio && ctx.aq.len() == 0 && ctx.abuf.is_empty()) && !ctx.eof.load(Ordering::Acquire);
        st.buffering = empty && !ctx.paused();
        st.seeking = ctx.seeking.load(Ordering::Acquire);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes() {
        assert_eq!(iso639_1("eng").as_deref(), Some("en"));
        assert_eq!(iso639_1("de").as_deref(), Some("de"));
        assert_eq!(iso639_1("French").as_deref(), Some("fr"));
        assert_eq!(lang_code(u16::from_be_bytes(*b"FR")).as_deref(), Some("fr"));
        assert_eq!(lang_code(0xffff), None);
    }

    #[test]
    fn audio_frame_with_the_next_ones_timestamp_is_moved_back() {
        let ts = |p: &Packet| unsafe { ((*p.0).pts, (*p.0).dts) };
        let make = |pts: i64, dur: i64| {
            let p = Packet::new();
            unsafe {
                ((*p.0).pts, (*p.0).dts, (*p.0).duration) = (pts, pts, dur);
            }
            p
        };
        // As FFmpeg's parser stamps them: 12375 got 15255, the next frame's.
        let (a, b) = (make(15255, 2880), make(15255, 2880));
        unsafe { fix_audio_timestamps(a.0, b.0) };
        assert_eq!(ts(&a), (12375, 12375));
        // In order: left alone.
        let (a, b) = (make(9495, 2880), make(12375, 2880));
        unsafe { fix_audio_timestamps(a.0, b.0) };
        assert_eq!(ts(&a), (9495, 9495));
        // Nothing to go by.
        let (a, b) = (make(15255, 0), make(15255, 2880));
        unsafe { fix_audio_timestamps(a.0, b.0) };
        assert_eq!(ts(&a), (15255, 15255));
    }
}
