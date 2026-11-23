//! DVD subpictures for discs played with their menus: decoding SPU packets
//! ourselves (rather than with FFmpeg's `dvdsub`) so that menu buttons can
//! be drawn with the highlight colours libdvdnav reports, with the palette
//! of whichever menu or title is playing, and so forced subtitles can be
//! told apart.

use super::SessionCtx;
use super::av::{self, ff};
use super::sync::PacketQueue;
use super::types::*;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static IDS: AtomicU64 = AtomicU64::new(1 << 48);

/// A decoded subpicture: 2-bit pixel codes plus the colours and contrasts
/// (palette indices and 0..15 opacities) its control sequence chose.
#[derive(Clone, Debug, PartialEq)]
pub struct Spu {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub codes: Vec<u8>,
    pub color: [u8; 4],
    pub alpha: [u8; 4],
    /// Seconds after the packet's timestamp it appears; `None`: it doesn't
    /// (a packet that only clears the screen).
    pub start: Option<f64>,
    pub stop: Option<f64>,
    /// Shown even when subtitles are off ("forced start display").
    pub forced: bool,
}

/// One control-sequence delay unit: 1024 ticks of the 90 kHz clock.
const DELAY_UNIT: f64 = 1024.0 / 90000.0;

/// Parses a complete SPU packet.
pub fn parse(buf: &[u8]) -> Option<Spu> {
    let be16 = |o: usize| -> Option<usize> { Some(((*buf.get(o)? as usize) << 8) | *buf.get(o + 1)? as usize) };
    let size = be16(0)?;
    if size > buf.len() || size < 4 {
        return None;
    }
    let buf = &buf[..size];
    let mut dcsq = be16(2)?;
    let mut spu = Spu {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        codes: Vec::new(),
        color: [0, 1, 2, 3],
        alpha: [0, 15, 15, 15],
        start: None,
        stop: None,
        forced: false,
    };
    let (mut x2, mut y2) = (0u32, 0u32);
    let mut fields: Option<(usize, usize)> = None;
    // Control sequences form a chain; the last one points to itself.
    for _ in 0..64 {
        let delay = be16(dcsq)? as f64 * DELAY_UNIT;
        let next = be16(dcsq + 2)?;
        let mut i = dcsq + 4;
        while let Some(&cmd) = buf.get(i) {
            i += 1;
            match cmd {
                0x00 => {
                    spu.forced = true;
                    spu.start.get_or_insert(delay);
                }
                0x01 => {
                    spu.start.get_or_insert(delay);
                }
                0x02 => {
                    spu.stop.get_or_insert(delay);
                }
                0x03 => {
                    let v = be16(i)?;
                    spu.color = [(v & 0xf) as u8, (v >> 4 & 0xf) as u8, (v >> 8 & 0xf) as u8, (v >> 12 & 0xf) as u8];
                    i += 2;
                }
                0x04 => {
                    let v = be16(i)?;
                    spu.alpha = [(v & 0xf) as u8, (v >> 4 & 0xf) as u8, (v >> 8 & 0xf) as u8, (v >> 12 & 0xf) as u8];
                    i += 2;
                }
                0x05 => {
                    let b = buf.get(i..i + 6)?;
                    spu.x = (b[0] as u32) << 4 | (b[1] as u32) >> 4;
                    x2 = ((b[1] as u32) & 0xf) << 8 | b[2] as u32;
                    spu.y = (b[3] as u32) << 4 | (b[4] as u32) >> 4;
                    y2 = ((b[4] as u32) & 0xf) << 8 | b[5] as u32;
                    i += 6;
                }
                0x06 => {
                    fields = Some((be16(i)?, be16(i + 2)?));
                    i += 4;
                }
                0x07 => {
                    // Colour/contrast changes within the picture: skipped.
                    i += be16(i)?;
                }
                0xff => break,
                _ => break,
            }
        }
        if next == dcsq || next >= buf.len() {
            break;
        }
        dcsq = next;
    }
    if x2 >= spu.x && y2 >= spu.y {
        spu.w = x2 - spu.x + 1;
        spu.h = y2 - spu.y + 1;
    }
    if let Some((top, bottom)) = fields
        && spu.w > 0
        && spu.h > 0
        && spu.w <= 1920
        && spu.h <= 1152
    {
        spu.codes = vec![0u8; (spu.w * spu.h) as usize];
        // Even lines come from the top field, odd ones from the bottom.
        for (field, start) in [(0, top), (1, bottom)] {
            decode_field(buf, start, spu.w as usize, spu.h as usize, field, &mut spu.codes);
        }
    } else {
        spu.w = 0;
        spu.h = 0;
    }
    Some(spu)
}

/// Run-length decoding of one field into every other line of `out`.
fn decode_field(buf: &[u8], start: usize, w: usize, h: usize, field: usize, out: &mut [u8]) {
    let mut nib = start * 2; // position in nibbles
    let get = |n: usize| -> u32 {
        match buf.get(n / 2) {
            Some(&b) => (if n.is_multiple_of(2) { b >> 4 } else { b & 0xf }) as u32,
            None => 0,
        }
    };
    let mut y = field;
    while y < h {
        let mut x = 0;
        while x < w {
            if nib / 2 >= buf.len() {
                return;
            }
            let mut v = get(nib);
            nib += 1;
            if v < 0x4 {
                v = v << 4 | get(nib);
                nib += 1;
                if v < 0x10 {
                    v = v << 4 | get(nib);
                    nib += 1;
                    if v < 0x40 {
                        v = v << 4 | get(nib);
                        nib += 1;
                    }
                }
            }
            let code = (v & 3) as u8;
            let run = (v >> 2) as usize;
            // A run of 0 fills the rest of the line.
            let n = if run == 0 { w - x } else { run.min(w - x) };
            out[y * w + x..y * w + x + n].fill(code);
            x += n;
        }
        // Lines start on a byte boundary.
        nib = nib.div_ceil(2) * 2;
        y += 2;
    }
}

/// Collects the PES packets of one SPU (a subpicture spans several).
#[derive(Default)]
pub struct Assembler {
    buf: Vec<u8>,
    pts: f64,
}

impl Assembler {
    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// Adds a packet; returns a complete SPU's bytes and timestamp.
    pub fn push(&mut self, data: &[u8], pts: f64) -> Option<(Vec<u8>, f64)> {
        if self.buf.is_empty() {
            self.pts = pts;
        }
        self.buf.extend_from_slice(data);
        let size = if self.buf.len() >= 2 { (self.buf[0] as usize) << 8 | self.buf[1] as usize } else { return None };
        if size < 4 || self.buf.len() > 65536 {
            self.buf.clear();
            return None;
        }
        if self.buf.len() >= size {
            let out = std::mem::take(&mut self.buf);
            return Some((out, self.pts));
        }
        None
    }
}

/// The colours a menu button is drawn with while it is highlighted: a
/// rectangle (inclusive) and libdvdnav's packed palette (four colour
/// indices in the high 16 bits, four contrasts in the low 16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Highlight {
    pub sx: u32,
    pub sy: u32,
    pub ex: u32,
    pub ey: u32,
    pub palette: u32,
}

/// The subpicture on screen and what it is drawn with. Shared by the
/// subpicture decoder thread, the DVD navigator and the UI's commands.
pub struct Overlay {
    st: Mutex<OverlayState>,
    /// Only forced subpictures are shown (a title with subtitles off).
    pub forced_only: AtomicBool,
}

struct OverlayState {
    /// RGB of the 16-colour palette.
    clut: [[u8; 3]; 16],
    highlight: Option<Highlight>,
    canvas: (u32, u32),
    /// The latest subpicture, with its interval in media time.
    last: Option<(Spu, f64, f64)>,
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay {
            st: Mutex::new(OverlayState { clut: default_clut(), highlight: None, canvas: (720, 480), last: None }),
            forced_only: AtomicBool::new(false),
        }
    }
}

fn default_clut() -> [[u8; 3]; 16] {
    let mut c = [[0u8; 3]; 16];
    for (i, e) in c.iter_mut().enumerate() {
        let v = (i * 17) as u8;
        *e = [v, v, v];
    }
    c
}

/// A DVD palette entry (0x00YYCrCb) as RGB (BT.601, limited range).
pub fn ycrcb_to_rgb(v: u32) -> [u8; 3] {
    let y = ((v >> 16) & 0xff) as f32 - 16.0;
    let cr = ((v >> 8) & 0xff) as f32 - 128.0;
    let cb = (v & 0xff) as f32 - 128.0;
    let c = |x: f32| x.round().clamp(0.0, 255.0) as u8;
    [c(1.164 * y + 1.596 * cr), c(1.164 * y - 0.813 * cr - 0.391 * cb), c(1.164 * y + 2.018 * cb)]
}

impl Overlay {
    pub fn set_clut(&self, yuv: &[u32; 16]) {
        let mut g = self.st.lock();
        for (o, &v) in g.clut.iter_mut().zip(yuv) {
            *o = ycrcb_to_rgb(v);
        }
    }

    pub fn set_canvas(&self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.st.lock().canvas = (w, h);
        }
    }

    /// Changes the highlighted button; returns whether anything changed.
    pub fn set_highlight(&self, hl: Option<Highlight>) -> bool {
        let mut g = self.st.lock();
        if g.highlight == hl {
            return false;
        }
        g.highlight = hl;
        true
    }

    pub fn clear(&self) {
        let mut g = self.st.lock();
        g.last = None;
        g.highlight = None;
    }

    /// Shows a newly decoded subpicture (from `start` to `end`, media time).
    pub fn show(&self, spu: Spu, start: f64, end: f64, subs: &super::subs::SubStore) {
        let mut g = self.st.lock();
        let ev = render(&g, &spu, start, end);
        g.last = Some((spu, start, end));
        drop(g);
        subs.add_event(ev);
    }

    /// Redraws the latest subpicture (the highlight or palette changed).
    pub fn refresh(&self, subs: &super::subs::SubStore) {
        let g = self.st.lock();
        let Some((spu, start, end)) = &g.last else { return };
        let ev = render(&g, spu, *start, *end);
        drop(g);
        subs.replace_bitmap(ev);
    }
}

fn render(g: &OverlayState, spu: &Spu, start: f64, end: f64) -> SubEvent {
    let (w, h) = (spu.w as usize, spu.h as usize);
    let mut bgra = vec![0u8; w * h * 4];
    let lut = |color: [u8; 4], alpha: [u8; 4]| -> [[u8; 4]; 4] {
        let mut l = [[0u8; 4]; 4];
        for (i, e) in l.iter_mut().enumerate() {
            let [r, g2, b] = g.clut[color[i] as usize & 0xf];
            *e = [b, g2, r, (alpha[i] & 0xf) * 17];
        }
        l
    };
    let normal = lut(spu.color, spu.alpha);
    let hl = g.highlight.map(|h| {
        let p = h.palette;
        let color = [(p >> 16 & 0xf) as u8, (p >> 20 & 0xf) as u8, (p >> 24 & 0xf) as u8, (p >> 28 & 0xf) as u8];
        let alpha = [(p & 0xf) as u8, (p >> 4 & 0xf) as u8, (p >> 8 & 0xf) as u8, (p >> 12 & 0xf) as u8];
        (h, lut(color, alpha))
    });
    for y in 0..h {
        let cy = spu.y + y as u32;
        let row_hl = hl.filter(|(r, _)| cy >= r.sy && cy <= r.ey);
        for x in 0..w {
            let code = spu.codes[y * w + x] as usize & 3;
            let cx = spu.x + x as u32;
            let px = match row_hl {
                Some((r, l)) if cx >= r.sx && cx <= r.ex => l[code],
                _ => normal[code],
            };
            bgra[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&px);
        }
    }
    let bm = SubBitmap {
        id: IDS.fetch_add(1, Ordering::Relaxed),
        x: spu.x as i32,
        y: spu.y as i32,
        w: spu.w,
        h: spu.h,
        canvas_w: g.canvas.0,
        canvas_h: g.canvas.1,
        bgra,
    };
    SubEvent { start, end, content: SubContent::Bitmap(Arc::new(vec![bm])) }
}

/// The subtitle decoder thread of a DVD played with its menus.
pub fn run_decoder(
    ctx: Arc<SessionCtx>,
    overlay: Arc<Overlay>,
    q: Arc<PacketQueue>,
    tb: ff::AVRational,
    stop: Arc<AtomicBool>,
) {
    let mut serial = -1;
    let mut asm = Assembler::default();
    while ctx.alive() && !stop.load(Ordering::Acquire) {
        let Some(qp) = q.get(&stop) else { break };
        if qp.serial != serial {
            serial = qp.serial;
            ctx.subs.clear_embedded();
            asm.reset();
        }
        if qp.pkt.is_empty() {
            continue;
        }
        let data = unsafe {
            let p = &*qp.pkt.0;
            if p.data.is_null() || p.size <= 0 {
                continue;
            }
            std::slice::from_raw_parts(p.data, p.size as usize)
        };
        let pts = av::ts_to_secs(qp.pkt.pts(), tb);
        let Some((buf, pts)) = asm.push(data, pts) else { continue };
        let Some(spu) = parse(&buf) else { continue };
        let base = if pts.is_finite() { pts } else { ctx.position() + ctx.start_time.get() };
        let Some(start) = spu.start.map(|d| base + d) else {
            // Only a "stop display": clears what is showing.
            if let Some(d) = spu.stop {
                ctx.subs.end_bitmaps_at(base + d);
            }
            continue;
        };
        if overlay.forced_only.load(Ordering::Acquire) && !spu.forced {
            continue;
        }
        let end = spu.stop.map(|d| base + d).filter(|e| *e > start).unwrap_or(f64::INFINITY);
        if spu.w == 0 || spu.h == 0 {
            ctx.subs.end_bitmaps_at(start);
            continue;
        }
        ctx.subs.prune_bitmaps(ctx.position() + ctx.start_time.get());
        overlay.show(spu, start, end, &ctx.subs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an SPU packet: a 4×2 picture at (10, 20), top line code 1,
    /// bottom line code 2, forced, shown at once and hidden after 10 units.
    fn sample() -> Vec<u8> {
        // RLE: run 4 of code c → value (4 << 2 | c) = 0x10 | c, a 2-nibble
        // code; each line is one byte.
        let top = [0x11u8];
        let bottom = [0x12u8];
        let mut b = vec![0, 0, 0, 0];
        let top_off = b.len();
        b.extend(top);
        let bottom_off = b.len();
        b.extend(bottom);
        let dcsq1 = b.len();
        let dcsq2 = dcsq1 + 4 + 1 + 3 + 3 + 7 + 5 + 1;
        b.extend([0, 0, (dcsq2 >> 8) as u8, dcsq2 as u8]);
        b.push(0x00); // forced start
        b.extend([0x03, 0x32, 0x10]); // colours: code3=3 code2=2 code1=1 code0=0
        b.extend([0x04, 0xff, 0xf0]); // contrast: code0 transparent
        // area x 10..13, y 20..21
        b.extend([0x05, 0x00, 0xa0, 0x0d, 0x01, 0x40, 0x15]);
        b.extend([0x06, 0, top_off as u8, 0, bottom_off as u8]);
        b.push(0xff);
        assert_eq!(b.len(), dcsq2);
        b.extend([0, 10, (dcsq2 >> 8) as u8, dcsq2 as u8, 0x02, 0xff]);
        let n = b.len();
        b[0] = (n >> 8) as u8;
        b[1] = n as u8;
        b[2] = (dcsq1 >> 8) as u8;
        b[3] = dcsq1 as u8;
        b
    }

    #[test]
    fn parses_a_subpicture() {
        let s = parse(&sample()).unwrap();
        assert_eq!((s.x, s.y, s.w, s.h), (10, 20, 4, 2));
        assert_eq!(s.codes, vec![1, 1, 1, 1, 2, 2, 2, 2]);
        assert!(s.forced);
        assert_eq!(s.start, Some(0.0));
        assert!((s.stop.unwrap() - 10.0 * DELAY_UNIT).abs() < 1e-9);
        assert_eq!(s.color, [0, 1, 2, 3]);
        assert_eq!(s.alpha, [0, 15, 15, 15]);
    }

    #[test]
    fn reassembles_split_packets() {
        let p = sample();
        let mut a = Assembler::default();
        assert!(a.push(&p[..5], 1.5).is_none());
        let (buf, pts) = a.push(&p[5..], f64::NAN).unwrap();
        assert_eq!((buf, pts), (p, 1.5));
    }

    #[test]
    fn highlight_recolours_inside_the_button() {
        let ov = Overlay::default();
        let mut clut = [0u32; 16];
        clut[1] = 0x00eb_8080; // white
        clut[5] = 0x0051_5a5a; // something else
        ov.set_clut(&clut);
        let spu = parse(&sample()).unwrap();
        // Highlight columns 12..13: code 1 → colour 5, fully opaque.
        ov.set_highlight(Some(Highlight { sx: 12, sy: 0, ex: 13, ey: 100, palette: 0x0000_0050 | 0x0050_0000 }));
        let g = ov.st.lock();
        let ev = render(&g, &spu, 0.0, 1.0);
        let SubContent::Bitmap(b) = ev.content else { panic!() };
        let px = |x: usize| &b[0].bgra[x * 4..x * 4 + 4];
        assert_eq!(px(0)[3], 255, "outside: the subpicture's own contrast");
        assert_eq!(px(0)[0..3], ycrcb_to_rgb(0x00eb_8080).iter().rev().copied().collect::<Vec<_>>()[..]);
        assert_eq!(px(2)[0..3], ycrcb_to_rgb(0x0051_5a5a).iter().rev().copied().collect::<Vec<_>>()[..]);
        assert_eq!(px(2)[3], 5 * 17);
    }
}
