//! Small reusable UI pieces.

use super::theme::Theme;
use crate::icons::Icon;
use gpui::{
    AnyView, App, Bounds, Context, DispatchPhase, HitboxBehavior, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, ScrollHandle, SharedString, Window, canvas, div, fill, point, prelude::*, px,
    svg,
};
use std::cell::RefCell;

pub fn icon(i: Icon, size: Pixels, color: Hsla) -> impl IntoElement {
    svg().path(i.path()).size(size).flex_none().text_color(color)
}

/// A simple text tooltip view.
pub struct Tooltip {
    text: SharedString,
    theme: Theme,
}

impl Render for Tooltip {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(5.))
            .bg(self.theme.popup)
            .border_1()
            .border_color(self.theme.border)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .shadow_md()
            .child(self.text.clone())
    }
}

pub fn tooltip(text: impl Into<SharedString>, theme: Theme) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_w, cx| cx.new(|_| Tooltip { text: text.clone(), theme }).into()
}

thread_local! {
    /// The scrollbar being dragged (by id) and where on its thumb it was grabbed.
    static SCROLL_DRAG: RefCell<Option<(SharedString, Pixels)>> = const { RefCell::new(None) };
}

/// A scrollbar thumb is being dragged (hover effects elsewhere should wait).
pub fn scrollbar_dragging() -> bool {
    SCROLL_DRAG.with(|d| d.borrow().is_some())
}

/// Width of the area a scrollbar takes at the right edge of its scroll area.
pub const SCROLLBAR_W: f32 = 10.0;

/// The thumb's place in `track` for a viewport of `view` over `max` more
/// pixels of content, scrolled `scrolled` down: (top, height).
fn thumb_geometry(track: Bounds<Pixels>, view: Pixels, max: Pixels, scrolled: Pixels) -> (Pixels, Pixels) {
    let len = track.size.height;
    let h = (len * (view / (view + max))).max(px(24.)).min(len);
    let frac = (scrolled / max).clamp(0., 1.);
    (track.top() + (len - h) * frac, h)
}

/// A vertical scrollbar for the scroll area tracked by `handle`: a thumb
/// that can be dragged, with clicks on the track jumping there. Put it in a
/// `relative()` parent next to the scroll area (not inside it, or it would
/// scroll away); it fills that parent's right edge and draws nothing while
/// everything fits. Wheel scrolling still reaches the area under it.
pub fn scrollbar(id: impl Into<SharedString>, handle: &ScrollHandle, theme: Theme) -> impl IntoElement {
    scrollbar_with_tolerance(id, handle, theme, px(0.5))
}

pub(super) fn scrollbar_with_tolerance(id: impl Into<SharedString>, handle: &ScrollHandle, theme: Theme, tolerance: Pixels) -> impl IntoElement {
    let id: SharedString = id.into();
    let handle = handle.clone();
    let h2 = handle.clone();
    canvas(
        move |bounds, window, _| {
            // The scroll area is an earlier sibling, so its size is current.
            (h2.max_offset().height > tolerance)
                .then(|| window.insert_hitbox(bounds, HitboxBehavior::BlockMouseExceptScroll))
        },
        move |bounds, hitbox, window, _| {
            let Some(hitbox) = hitbox else { return };
            let max = handle.max_offset().height;
            let view = handle.bounds().size.height;
            if max <= tolerance || view <= px(0.) {
                return;
            }
            let track = Bounds::new(
                point(bounds.left(), bounds.top() + px(3.)),
                gpui::size(bounds.size.width, bounds.size.height - px(6.)),
            );
            let offset = handle.offset();
            let (top, h) = thumb_geometry(track, view, max, -offset.y);
            let dragging = SCROLL_DRAG.with(|d| d.borrow().as_ref().is_some_and(|(i, _)| *i == id));
            let hovered = hitbox.is_hovered(window);
            let w = if hovered || dragging { px(8.) } else { px(5.) };
            let alpha = if dragging {
                0.75
            } else if hovered {
                0.6
            } else {
                0.4
            };
            let thumb = Bounds::new(point(bounds.right() - w - px(2.), top), gpui::size(w, h));
            window.paint_quad(fill(thumb, theme.text_dim.opacity(alpha)).corner_radii(w / 2.));

            // Moves the thumb's top to `y - grab`.
            let scroll_to = {
                let handle = handle.clone();
                move |y: Pixels, grab: Pixels| {
                    let room = track.size.height - h;
                    let frac = if room > px(0.) { ((y - grab - track.top()) / room).clamp(0., 1.) } else { 0. };
                    handle.set_offset(point(offset.x, -(max * frac)));
                }
            };
            let (id1, id2, id3) = (id.clone(), id.clone(), id.clone());
            let hb = hitbox.clone();
            let down = scroll_to.clone();
            window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left || !hb.is_hovered(window) {
                    return;
                }
                let y = ev.position.y;
                let grab = if y >= top && y <= top + h { y - top } else { h / 2. };
                SCROLL_DRAG.with(|d| *d.borrow_mut() = Some((id1.clone(), grab)));
                down(y, grab);
                window.refresh();
                cx.stop_propagation();
            });
            let hb = hitbox.clone();
            window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let grab = SCROLL_DRAG.with(|d| d.borrow().as_ref().filter(|(i, _)| *i == id2).map(|(_, g)| *g));
                match grab {
                    Some(_) if !ev.dragging() => {
                        SCROLL_DRAG.with(|d| *d.borrow_mut() = None);
                        window.refresh();
                    }
                    Some(grab) => {
                        scroll_to(ev.position.y, grab);
                        window.refresh();
                        cx.stop_propagation();
                    }
                    None if hb.is_hovered(window) != hovered => window.refresh(),
                    None => {}
                }
            });
            window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble
                    && SCROLL_DRAG.with(|d| d.borrow().as_ref().is_some_and(|(i, _)| *i == id3))
                {
                    SCROLL_DRAG.with(|d| *d.borrow_mut() = None);
                    window.refresh();
                    cx.stop_propagation();
                }
            });
        },
    )
    .absolute()
    .top_0()
    .right_0()
    .bottom_0()
    .w(px(SCROLLBAR_W))
}

/// Playback speed slider range. The scale is logarithmic so 1× sits in the
/// middle, with as much room for slowing down as for speeding up.
pub const SPEED_MIN: f64 = 0.25;
pub const SPEED_MAX: f64 = 4.0;

/// Slider position (0..=1) for a speed.
pub fn speed_to_frac(speed: f64) -> f32 {
    ((speed.clamp(SPEED_MIN, SPEED_MAX) / SPEED_MIN).ln() / (SPEED_MAX / SPEED_MIN).ln()) as f32
}

/// Speed for a slider position: 0.05 steps, catching at 1×.
pub fn frac_to_speed(f: f32) -> f64 {
    let raw = SPEED_MIN * (SPEED_MAX / SPEED_MIN).powf(f.clamp(0.0, 1.0) as f64);
    if (raw - 1.0).abs() < 0.04 {
        return 1.0;
    }
    ((raw / 0.05).round() * 0.05).clamp(SPEED_MIN, SPEED_MAX)
}

/// "1×", "1.25×", "0.5×".
pub fn format_speed(speed: f64) -> String {
    let s = format!("{speed:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}×")
}

pub fn format_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".into();
    }
    let s = secs.floor() as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

pub fn format_size(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", U[i]) }
}

/// Parses "1:23:45", "12:34", "90" or "1h2m3s" into seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains(['h', 'm', 's']) && !s.contains(':') {
        let mut total = 0.0;
        let mut num = String::new();
        for c in s.chars() {
            match c {
                '0'..='9' | '.' => num.push(c),
                'h' => total += std::mem::take(&mut num).parse::<f64>().ok()? * 3600.0,
                'm' => total += std::mem::take(&mut num).parse::<f64>().ok()? * 60.0,
                's' => total += std::mem::take(&mut num).parse::<f64>().ok()?,
                ' ' => {}
                _ => return None,
            }
        }
        if !num.is_empty() {
            total += num.parse::<f64>().ok()?;
        }
        return Some(total);
    }
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_slider_mapping() {
        assert!((speed_to_frac(1.0) - 0.5).abs() < 1e-6);
        assert_eq!(frac_to_speed(0.0), SPEED_MIN);
        assert_eq!(frac_to_speed(1.0), SPEED_MAX);
        assert_eq!(frac_to_speed(0.5), 1.0);
        assert_eq!(frac_to_speed(0.51), 1.0); // catches at 1×
        for s in [0.25, 0.5, 0.75, 1.25, 1.5, 2.0, 3.0] {
            assert!((frac_to_speed(speed_to_frac(s)) - s).abs() < 1e-9, "{s}");
        }
        assert_eq!(format_speed(1.0), "1×");
        assert_eq!(format_speed(1.25), "1.25×");
        assert_eq!(format_speed(0.5), "0.5×");
        assert_eq!(format_speed(1.1500000000000001), "1.15×");
    }
    #[test]
    fn scrollbar_thumb() {
        let track = Bounds::new(point(px(0.), px(100.)), gpui::size(px(10.), px(200.)));
        // Content twice the viewport: half-height thumb, top to bottom.
        assert_eq!(thumb_geometry(track, px(200.), px(200.), px(0.)), (px(100.), px(100.)));
        assert_eq!(thumb_geometry(track, px(200.), px(200.), px(200.)), (px(200.), px(100.)));
        // Very long content: the thumb keeps a usable size.
        assert_eq!(thumb_geometry(track, px(200.), px(100000.), px(0.)).1, px(24.));
    }

    #[test]
    fn time_roundtrip() {
        assert_eq!(format_time(3725.4), "1:02:05");
        assert_eq!(format_time(65.0), "01:05");
        assert_eq!(parse_time("1:02:05"), Some(3725.0));
        assert_eq!(parse_time("90"), Some(90.0));
        assert_eq!(parse_time("1h2m5s"), Some(3725.0));
        assert_eq!(parse_time("abc"), None);
    }
}
