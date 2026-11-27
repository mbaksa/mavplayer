//! Rendering of the main window.

#[path = "subtitle_bitmap.rs"]
mod subtitle_bitmap;
#[path = "subtitle_fonts.rs"]
mod subtitle_fonts;
#[path = "subtitle_style.rs"]
mod subtitle_style;

use super::actions::*;
use super::keymap;
use super::menu::MenuItem as M;
use super::menus::MENUBAR;
use super::picture::{Chrome, picture_rect};
use super::player::{Aspect, Drag, PlayerView, SliderFn};
use super::theme::{Theme, overlay_theme};
use super::widgets::{format_speed, format_time, frac_to_speed, icon, speed_to_frac, tooltip};
use crate::engine::{MediaSource, Status, SubContent, TrackKind};
use crate::icons::Icon;
use crate::settings::{
    BarControl, PlaylistLabels, PlaylistThumbnails, RepeatMode, ScrollAction, SubStyle, TimelineVisualization,
};
use gpui::{
    AnyElement, AnyView, Bounds, ClickEvent, Context, Corners, CursorStyle, Decorations, Div, DragMoveEvent, ElementId,
    ExternalPaths, Focusable, Hsla, ImageSource, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point,
    RenderImage, ResizeEdge, Resource, ScrollDelta, ScrollWheelEvent, SharedString, Stateful, StyleRefinement, Window,
    WindowControlArea, canvas, deferred, div, img, point, prelude::*, px, relative, rgba, size,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) fn waveform_badge(color: Hsla) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for (i, h) in [3.0, 7.0, 11.0, 5.0, 8.0].into_iter().enumerate() {
                let origin = point(bounds.origin.x + px(i as f32 * 2.4), bounds.origin.y + px((12.0 - h) * 0.5));
                window.paint_quad(gpui::fill(Bounds::new(origin, size(px(1.5), px(h))), color));
            }
        },
    )
    .size(px(12.))
    .into_any_element()
}

fn subtitle_badge(color: Hsla) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for (x, y, w, h) in [(0., 1., 12., 1.), (0., 10., 12., 1.), (0., 2., 1., 8.), (11., 2., 1., 8.),
                (2., 5., 3., 1.), (6., 5., 4., 1.), (2., 7., 5., 1.), (8., 7., 2., 1.)] {
                window.paint_quad(gpui::fill(Bounds::new(point(bounds.left() + px(x), bounds.top() + px(y)), size(px(w), px(h))), color));
            }
        },
    )
    .size(px(12.))
    .into_any_element()
}

/// A cached audio-only waveform occupies the same preview slot as a video thumbnail.
/// The icon a playlist row shows in place of a picture or waveform it doesn't
/// have: what the file holds once probed, otherwise a guess from the source.
fn default_media_icon(it: &crate::playlist::PlaylistItem) -> Icon {
    if it.video_stream.is_some() {
        return Icon::Video;
    }
    if it.audio_stream.is_some() {
        return Icon::Audio;
    }
    let audio = match &it.source {
        MediaSource::Cd { .. } => true,
        MediaSource::File(path) => path
            .extension()
            .is_some_and(|e| crate::playlist::AUDIO_EXTS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str())),
        _ => false,
    };
    if audio { Icon::Audio } else { Icon::Video }
}

fn waveform_thumbnail(peaks: Arc<crate::engine::waveform::Peaks>, color: Hsla) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            if peaks.is_empty() {
                return;
            }
            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let columns = (w.ceil() as usize).clamp(1, 256);
            for x in 0..columns {
                let from = x * peaks.len() / columns;
                let to = ((x + 1) * peaks.len() / columns).max(from + 1).min(peaks.len());
                let slice = &peaks[from..to];
                let lo = slice.iter().map(|p| p[0]).min().unwrap_or(0) as f32 / 32768.0;
                let hi = slice.iter().map(|p| p[1]).max().unwrap_or(0) as f32 / 32768.0;
                let top = (h * (1.0 - hi) * 0.5).clamp(0.0, h);
                let bottom = (h * (1.0 - lo) * 0.5).clamp(top, h);
                window.paint_quad(gpui::fill(
                    Bounds::new(
                        point(bounds.origin.x + px(x as f32 * w / columns as f32), bounds.origin.y + px(top)),
                        size(px((w / columns as f32).max(1.0)), px((bottom - top).max(1.0))),
                    ),
                    color,
                ));
            }
        },
    )
    .w(px(72.))
    .h(px(44.))
    .flex_none()
    .into_any_element()
}

const TITLEBAR_H: f32 = 34.0;
/// Fullscreen control bar height: seek row (18) + gap (4) + buttons row (38)
/// + vertical padding (6 + 8). The extra 40 px above it is only a gradient.
pub(super) const FULLSCREEN_CONTROL_BAR_H: f32 = 74.0;
const RESIZE_EDGE: f32 = 6.0;
/// Playlist row height and the list's vertical padding (`py_1`).
const ROW_H: f32 = 34.0;
const LIST_PAD: f32 = 4.0;

/// A playlist row or selected group being dragged to a new position.
pub struct PlaylistDrag {
    pub ids: Vec<u64>,
    pub anchor: u64,
    pub title: SharedString,
}

/// What follows the pointer while a playlist row is dragged.
struct PlaylistDragPreview {
    title: SharedString,
    theme: Theme,
}

impl Render for PlaylistDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .max_w(px(280.))
            .px_3()
            .py_1()
            .rounded(px(6.))
            .bg(t.surface)
            .border_1()
            .border_color(t.accent)
            .shadow_md()
            .text_size(px(13.))
            .text_color(t.text)
            .truncate()
            .child(self.title.clone())
    }
}

/// Where the picture goes inside `area`.
pub fn video_rect(area: Bounds<Pixels>, img: (u32, u32), aspect: Aspect, zoom: f32) -> Bounds<Pixels> {
    let (aw, ah) = (f32::from(area.size.width), f32::from(area.size.height));
    if img.0 == 0 || img.1 == 0 || aw <= 0.0 || ah <= 0.0 {
        return area;
    }
    let ia = img.0 as f32 / img.1 as f32;
    let (mut w, mut h) = match aspect {
        Aspect::Stretch => (aw, ah),
        Aspect::Fill => {
            let s = (aw / ia).max(ah);
            (s * ia, s)
        }
        Aspect::Auto | Aspect::Ratio(..) => {
            let a = if let Aspect::Ratio(r, _) = aspect { r as f32 } else { ia };
            if aw / ah > a { (ah * a, ah) } else { (aw, aw / a) }
        }
    };
    w *= zoom;
    h *= zoom;
    let x = f32::from(area.origin.x) + (aw - w) / 2.0;
    let y = f32::from(area.origin.y) + (ah - h) / 2.0;
    Bounds::new(point(px(x.round()), px(y.round())), size(px(w.round()), px(h.round())))
}

fn frac_in(bounds: Bounds<Pixels>, p: Point<Pixels>, vertical: bool) -> f32 {
    if vertical {
        let h = f32::from(bounds.size.height).max(1.0);
        (1.0 - (f32::from(p.y) - f32::from(bounds.origin.y)) / h).clamp(0.0, 1.0)
    } else {
        let w = f32::from(bounds.size.width).max(1.0);
        ((f32::from(p.x) - f32::from(bounds.origin.x)) / w).clamp(0.0, 1.0)
    }
}

/// Check GPUI's actual font selection, not its implicit fallback.
/// This does not register fonts or write configuration.
pub(super) fn installed_subtitle_font(family: &str, cx: &mut Context<PlayerView>) -> bool {
    subtitle_fonts::has_family(family, cx)
}

/// Reports the source selected for the default subtitle font.
pub(super) use subtitle_fonts::DefaultFontSource;

pub(super) fn default_subtitle_font_source(cx: &mut Context<PlayerView>) -> DefaultFontSource {
    subtitle_fonts::default_font_source(cx)
}

/// Font metadata for Preferences, independent of subtitle rendering.
pub(super) fn subtitle_available_weights(family: &str) -> Vec<u16> {
    subtitle_fonts::available_weights(family)
}

pub(super) fn subtitle_weight_supported(family: &str, weight: u16) -> u16 {
    subtitle_fonts::supported_weight(family, weight)
}

/// Available installed font families for the Preferences picker.
pub(super) fn subtitle_font_families() -> &'static [String] {
    subtitle_fonts::installed_families()
}

impl PlayerView {
    // ───────────────────────── reusable controls ─────────────────────────

    pub fn icon_button(
        &self,
        id: impl Into<ElementId>,
        i: Icon,
        tip: &str,
        theme: Theme,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(32.))
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover))
            .active(|s| s.bg(theme.pressed))
            .child(icon(i, px(18.), theme.text))
            .tooltip(tooltip(tip.to_string(), theme))
            .on_click(cx.listener(on_click))
    }

    pub fn text_button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        primary: bool,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = self.theme;
        div()
            .id(id)
            .px_3()
            .h(px(30.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.))
            .cursor_pointer()
            .text_size(px(13.))
            .when(primary, |d| d.bg(t.brand(135.)).text_color(t.on_accent).hover(|s| s.opacity(0.9)))
            .when(!primary, |d| d.bg(t.surface_alt).border_1().border_color(t.border).hover(|s| s.bg(t.hover)))
            .child(label.into())
            .on_click(cx.listener(on_click))
    }

    /// A horizontal (or vertical) slider. `value` is 0..=1.
    pub fn slider(
        &self,
        id: impl Into<ElementId>,
        value: f32,
        vertical: bool,
        theme: Theme,
        on_change: impl Fn(&mut Self, f32, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let b2 = bounds.clone();
        let on_change: SliderFn = Rc::new(on_change);
        let v = value.clamp(0.0, 1.0);
        let track =
            if vertical {
                div().w(px(4.)).h_full().rounded_full().bg(theme.track).relative().child(
                    div().absolute().bottom_0().left_0().w_full().h(relative(v)).rounded_full().bg(theme.brand(0.)),
                )
            } else {
                div().h(px(4.)).w_full().rounded_full().bg(theme.track).relative().child(
                    div().absolute().top_0().left_0().h_full().w(relative(v)).rounded_full().bg(theme.brand(90.)),
                )
            };
        let thumb = div().absolute().size(px(12.)).rounded_full().bg(gpui::white()).shadow_sm();
        let thumb = if vertical { thumb.bottom(relative(v)).mb(px(-6.)) } else { thumb.left(relative(v)).ml(px(-6.)) };
        // The track is inset by half the thumb so the thumb never pokes out
        // of the slider (over neighbouring labels) at either end.
        let inner = div()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .child(canvas(move |b, _, _| b2.set(b), |_, _, _, _| {}).absolute().size_full())
            .child(track)
            .child(thumb);
        div()
            .id(id)
            .flex()
            .cursor_pointer()
            .when(vertical, |d| d.w(px(20.)).h_full().py(px(6.)))
            .when(!vertical, |d| d.h(px(20.)).w_full().px(px(6.)))
            .child(inner)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    let f = frac_in(bounds.get(), ev.position, vertical);
                    on_change(this, f, window, cx);
                    this.drag = Some(Drag {
                        bounds: bounds.clone(),
                        vertical,
                        on_change: on_change.clone(),
                        on_release: None,
                        last: f,
                    });
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
    }

    pub(super) fn handle_drag_move(
        &mut self,
        ev: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(d) = self.drag.as_ref() else { return false };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.end_drag(window, cx);
            return true;
        }
        let f = frac_in(d.bounds.get(), ev.position, d.vertical);
        let cb = d.on_change.clone();
        if let Some(d) = self.drag.as_mut() {
            d.last = f;
        }
        cb(self, f, window, cx);
        cx.notify();
        true
    }

    pub(super) fn end_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.timeline_edge_drag = false;
        if let Some(d) = self.drag.take() {
            if let Some(r) = d.on_release {
                r(self, d.last, window, cx);
            }
            if self.menu.as_ref().is_some_and(|m| m.timeline_settings)
                || matches!(self.dialog, Some(super::dialogs::Dialog::Settings(1)))
            {
                self.settings.save();
            }
            cx.notify();
        }
    }

    // ───────────────────────── window chrome ─────────────────────────

    fn titlebar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let controls = window.window_controls();
        let title: SharedString = self
            .playlist
            .current
            .and_then(|i| self.playlist.items.get(i))
            .and_then(|item| item.custom_title.clone())
            .or_else(|| {
                self.player.with_state(|s| s.media.as_ref().map(|m| m.title.clone().unwrap_or_else(|| m.source_name.clone())))
            })
            .map(|n| format!("{n} — MavPlayer"))
            .unwrap_or_else(|| "MavPlayer".into())
            .into();
        let btn = |id: &'static str, i: Icon, close: bool| {
            div()
                .id(id)
                .size(px(26.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .when(!close, |d| d.hover(|s| s.bg(t.hover)))
                .when(close, |d| d.hover(|s| s.bg(t.danger)))
                .child(icon(i, px(14.), t.text))
        };
        div()
            .id("titlebar")
            .h(px(TITLEBAR_H))
            .flex_none()
            .flex()
            .items_center()
            .px_2()
            .gap_2()
            .bg(t.chrome)
            .border_b_1()
            .border_color(t.chrome_border)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(MouseButton::Left, |ev, window, _| {
                if ev.click_count == 2 {
                    window.zoom_window();
                } else {
                    window.start_window_move();
                }
            })
            .on_mouse_down(MouseButton::Right, |ev, window, _| window.show_window_menu(ev.position))
            .child(img(ImageSource::Resource(Resource::Embedded("app-icon.svg".into()))).size(px(18.)).flex_none())
            .child(div().flex_1().truncate().text_size(px(13.)).text_color(t.text_dim).child(title))
            .when(controls.minimize, |d| {
                d.child(
                    btn("win-min", Icon::WindowMinimize, false)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(|_, w, _| w.minimize_window()),
                )
            })
            .when(controls.maximize, |d| {
                d.child(
                    btn(
                        "win-max",
                        if window.is_maximized() { Icon::WindowRestore } else { Icon::WindowMaximize },
                        false,
                    )
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, w, _| w.zoom_window()),
                )
            })
            .child(
                btn("win-close", Icon::WindowClose, true)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, w, _| {
                        // Like the compositor's close button: quits, or waits in the tray.
                        this.close_state();
                        w.remove_window();
                    })),
            )
            .into_any_element()
    }

    fn resize_handles(&self) -> AnyElement {
        let e = px(RESIZE_EDGE);
        let edge = |id: &'static str, edge: ResizeEdge, cursor: CursorStyle| {
            div().id(id).absolute().cursor(cursor).occlude().on_mouse_down(MouseButton::Left, move |_, w, cx| {
                w.start_window_resize(edge);
                cx.stop_propagation();
            })
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(edge("rz-t", ResizeEdge::Top, CursorStyle::ResizeUpDown).top_0().left(e).right(e).h(e))
            .child(edge("rz-b", ResizeEdge::Bottom, CursorStyle::ResizeUpDown).bottom_0().left(e).right(e).h(e))
            .child(edge("rz-l", ResizeEdge::Left, CursorStyle::ResizeLeftRight).left_0().top(e).bottom(e).w(e))
            .child(edge("rz-r", ResizeEdge::Right, CursorStyle::ResizeLeftRight).right_0().top(e).bottom(e).w(e))
            .child(edge("rz-tl", ResizeEdge::TopLeft, CursorStyle::ResizeUpLeftDownRight).top_0().left_0().size(e * 2.))
            .child(
                edge("rz-tr", ResizeEdge::TopRight, CursorStyle::ResizeUpRightDownLeft).top_0().right_0().size(e * 2.),
            )
            .child(
                edge("rz-bl", ResizeEdge::BottomLeft, CursorStyle::ResizeUpRightDownLeft)
                    .bottom_0()
                    .left_0()
                    .size(e * 2.),
            )
            .child(
                edge("rz-br", ResizeEdge::BottomRight, CursorStyle::ResizeUpLeftDownRight)
                    .bottom_0()
                    .right_0()
                    .size(e * 2.),
            )
            .into_any_element()
    }

    fn menubar(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let open_idx = self.menu.as_ref().and_then(|m| m.menubar_index);
        let bar = self.menubar_bounds.clone();
        let mut row = div()
            .id("menubar")
            .relative()
            .h(px(28.))
            .flex_none()
            .flex()
            .items_center()
            .px_1()
            .gap(px(2.))
            .bg(t.chrome)
            .text_size(px(13.))
            .child(canvas(move |b, _, _| bar.set(b), |_, _, _, _| {}).absolute().size_full())
            // An open menu leaves the menu bar uncovered (see `render_menu`):
            // a click on its empty part closes the menu.
            .on_any_mouse_down(cx.listener(|this, _, _, cx| {
                if this.menu.is_some() {
                    this.close_menu(cx);
                    cx.stop_propagation();
                }
            }));
        for (i, name) in MENUBAR.iter().enumerate() {
            let bounds = Rc::new(Cell::new(Bounds::default()));
            let (b1, b2, b3) = (bounds.clone(), bounds.clone(), bounds.clone());
            row = row.child(
                div()
                    .id(SharedString::from(format!("menubar-{i}")))
                    .relative()
                    .px_2()
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .when(open_idx == Some(i), |d| d.bg(t.pressed))
                    .hover(|s| s.bg(t.hover))
                    .child(canvas(move |b, _, _| b1.set(b), |_, _, _, _| {}).absolute().size_full())
                    .child(*name)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if this.menu.as_ref().and_then(|m| m.menubar_index) == Some(i) {
                                this.close_menu(cx);
                            } else {
                                let items = this.menubar_menu(i, cx);
                                this.open_menu(items, super::menu::below(b2.get()), Some(i), cx);
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered
                            && let Some(cur) = this.menu.as_ref().and_then(|m| m.menubar_index)
                            && cur != i
                        {
                            let items = this.menubar_menu(i, cx);
                            this.open_menu(items, super::menu::below(b3.get()), Some(i), cx);
                        }
                    })),
            );
        }
        row.into_any_element()
    }

    // ───────────────────────── video area ─────────────────────────

    fn video_area(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let fullscreen = window.is_fullscreen();
        let st = self.player.state();
        let active = self.player.is_active();
        let show_controls = self.controls_visible();
        let has_video = st.media.as_ref().map(|m| m.has_video()).unwrap_or(false);

        let frame = self.frame.clone();
        let fsize = self.frame_size;
        let aspect = self.aspect;
        let zoom = self.zoom;
        let bcell = self.video_bounds.clone();
        let set_target = self.player.target_size_setter();

        // Subtitles at the current position.
        let pos = self.player.position() - self.sub_delay;
        let mut subs = std::mem::take(&mut self.subs);
        // A DVD with menus draws its subpictures itself (buttons and forced
        // subtitles show even with subtitles off).
        let dvd_nav = self.player.with_state(|s| s.dvd.is_some());
        if (self.subs_visible() || dvd_nav) && active {
            self.player.subtitles_at(pos, &mut subs);
        } else {
            subs.clear();
        }
        // (image, x, y, w, h, canvas w, canvas h)
        #[allow(clippy::type_complexity)]
        let mut bitmaps: Vec<(Arc<RenderImage>, f32, f32, f32, f32, u32, u32)> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        for ev in &subs {
            match &ev.content {
                SubContent::Text(s) => texts.push(s.clone()),
                SubContent::Bitmap(bms) => {
                    for b in bms.iter() {
                        let entry = self.sub_images.entry(b.id).or_insert_with(|| {
                            let img = image::RgbaImage::from_raw(b.w, b.h, b.bgra.clone()).unwrap_or_default();
                            (Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(img)])), 0)
                        });
                        entry.1 = 0;
                        bitmaps.push((
                            entry.0.clone(),
                            b.x as f32,
                            b.y as f32,
                            b.w as f32,
                            b.h as f32,
                            b.canvas_w,
                            b.canvas_h,
                        ));
                    }
                }
            }
        }
        self.subs = subs;
        let (vw, vh) =
            self.player.with_state(|s| s.video.as_ref().map(|v| (v.width as u32, v.height as u32))).unwrap_or(fsize);
        let canvas_size = self.player.subtitle_canvas();

        let picture = canvas(
            move |b, window, _| {
                bcell.set(b);
                // Tell the engine the size as soon as layout knows it, so a
                // paused picture is redone for a resized window right away.
                let s = window.scale_factor() * zoom;
                let (w, h) = ((f32::from(b.size.width) * s).round(), (f32::from(b.size.height) * s).round());
                if w > 16.0 && h > 16.0 {
                    set_target(w as u32, h as u32);
                }
                b
            },
            move |b, _, window, _| {
                // The picture itself is drawn beneath, by `picture::Picture`.
                if frame.is_some() {
                    let r = picture_rect(b, fsize, aspect, zoom, window.scale_factor());
                    // Bitmap subtitles (PGS/DVD/DVB) are positioned on the video frame.
                    for (img, x, y, w, h, cw, ch) in bitmaps {
                        let (cw, ch) = if cw > 0 {
                            (cw, ch)
                        } else if canvas_size.0 > 0 {
                            canvas_size
                        } else {
                            (vw.max(1), vh.max(1))
                        };
                        let sx = f32::from(r.size.width) / cw as f32;
                        let sy = f32::from(r.size.height) / ch as f32;
                        let rb = Bounds::new(
                            point(r.origin.x + px(x * sx), r.origin.y + px(y * sy)),
                            size(px(w * sx), px(h * sy)),
                        );
                        let _ = window.paint_image(rb, Corners::default(), img, 0, false);
                    }
                }
            },
        )
        .absolute()
        .size_full();

        let mut root = div()
            .id("video-area")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .overflow_hidden()
            .child(picture)
            .when(active && !show_controls && self.menu.is_none(), |d| d.cursor(CursorStyle::None))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                this.note_activity(cx);
                this.playlist_previews.pointer = Some(ev.position);
                this.update_playlist_autohide(window, cx);
                if this.settings.drag_video_to_move_window
                    && let Some(start) = this.window_move_origin
                {
                    if ev.pressed_button != Some(MouseButton::Left) {
                        this.window_move_origin = None;
                    } else if !window.is_fullscreen() && this.drag.is_none() && this.scrub.is_none() {
                        let dx = f32::from(ev.position.x - start.x);
                        let dy = f32::from(ev.position.y - start.y);
                        if dx * dx + dy * dy >= 25.0 {
                            this.window_move_origin = None;
                            this.window_move_started = true;
                            window.start_window_move();
                            return;
                        }
                    }
                }
                // DVD menus: the button under the pointer lights up.
                if this.dvd_buttons()
                    && let Some((x, y)) = this.dvd_point(ev.position)
                {
                    this.dvd(crate::engine::DvdCommand::Hover(x, y), cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, _| {
                    this.window_move_started = false;
                    this.window_move_origin = if this.settings.drag_video_to_move_window
                        && !window.is_fullscreen()
                        && !this.dvd_buttons()
                        && this.menu.is_none()
                        && this.dialog.is_none()
                        && this.drag.is_none()
                        && this.scrub.is_none()
                    {
                        Some(ev.position)
                    } else {
                        None
                    };
                    if ev.click_count <= 1 {
                        this.click_origin = this.load_counter;
                    }
                }),
            )
            .on_click(cx.listener(|this, ev: &ClickEvent, window, cx| {
                if ev.is_right_click() {
                    return;
                }
                if this.window_move_started {
                    this.window_move_started = false;
                    return;
                }
                // A click on a DVD menu button presses it (instead of pausing).
                if this.dvd_buttons()
                    && let Some((x, y)) = this.dvd_point(ev.position())
                    && this.player.dvd_command(crate::engine::DvdCommand::Click(x, y))
                {
                    window.focus(&this.focus);
                    cx.notify();
                    return;
                }
                // The click (or a double-click's first click) opened what is
                // now playing, from the start screen: it isn't meant for the video.
                if this.click_origin != this.load_counter {
                    window.focus(&this.focus);
                    return;
                }
                if ev.click_count() == 2 && this.settings.double_click_fullscreen {
                    window.toggle_fullscreen();
                    // Undo the first click's pause toggle by restoring the
                    // state from before it. Toggling again would leave the
                    // video paused whenever the first click didn't toggle
                    // (a menu swallowed it, the file was still loading).
                    if let Some((was_paused, at)) = this.click_pause.take()
                        && at.elapsed() < Duration::from_millis(600)
                    {
                        this.player.set_paused(was_paused);
                    }
                } else if ev.click_count() == 1 && this.settings.single_click_pause && this.player.is_active() {
                    this.click_pause = Some((this.player.paused(), Instant::now()));
                    this.player.toggle_pause();
                }
                window.focus(&this.focus);
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    let items = this.context_menu(this.menubar_visible(window), cx);
                    this.open_menu(items, ev.position, None, cx);
                }),
            )
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                let dy = match ev.delta {
                    ScrollDelta::Lines(p) => p.y,
                    ScrollDelta::Pixels(p) => f32::from(p.y) / 40.0,
                };
                if dy.abs() < 0.2 {
                    return;
                }
                match this.settings.scroll_action {
                    ScrollAction::Volume => {
                        let v = this.player.volume() + dy.signum() as f64 * this.settings.volume_step / 100.0;
                        this.set_volume(v, cx);
                    }
                    ScrollAction::Seek => this.seek_relative(dy.signum() as f64 * this.settings.seek_step, false, cx),
                    ScrollAction::Nothing => {}
                }
            }));

        // Text subtitles need the current layout, not the previous video_bounds.
        if !texts.is_empty() {
            let player = cx.entity().downgrade();
            let has_frame = self.frame.is_some();
            root = root.child(
                canvas(
                    move |area, window, cx| {
                        if area.size.width <= px(0.) || area.size.height <= px(0.) {
                            return None;
                        }
                        let dpi = window.scale_factor();
                        let rect = if has_frame { picture_rect(area, fsize, aspect, zoom, dpi) } else { area };
                        let subtitles = player
                            .update(cx, |this, cx| {
                                this.text_subtitles(&texts, rect, area, fullscreen && show_controls, dpi, cx)
                            })
                            .ok()?;
                        let mut overlay = div().relative().size_full().child(subtitles).into_any_element();
                        overlay.prepaint_as_root(
                            area.origin,
                            size(
                                gpui::AvailableSpace::Definite(area.size.width),
                                gpui::AvailableSpace::Definite(area.size.height),
                            ),
                            window,
                            cx,
                        );
                        Some(overlay)
                    },
                    |_, overlay, window, cx| {
                        if let Some(mut overlay) = overlay {
                            overlay.paint(window, cx);
                        }
                    },
                )
                .absolute()
                .size_full(),
            );
        }

        // Idle / audio-only / loading screens.
        if self.frame.is_none() {
            let placeholder: AnyElement = if self.reading_disc {
                self.status_screen(Icon::Disc, "Reading disc…", None)
            } else if !active {
                self.idle_screen(cx)
            } else {
                match &st.status {
                    Status::Loading => {
                        self.status_screen(Icon::Video, "Opening…", st.source.as_ref().map(|s| s.display_name()))
                    }
                    Status::Error(e) => self.status_screen(Icon::Info, "Could not play this file", Some(e.clone())),
                    _ if !has_video => {
                        let m = st.media.clone();
                        let title = m
                            .as_ref()
                            .and_then(|m| m.title.clone())
                            .or_else(|| st.source.as_ref().map(|s| s.display_name()));
                        let sub = m
                            .as_ref()
                            .map(|m| {
                                [m.artist.clone(), m.album.clone()]
                                    .into_iter()
                                    .flatten()
                                    .collect::<Vec<_>>()
                                    .join(" — ")
                            })
                            .filter(|s| !s.is_empty());
                        self.status_screen(Icon::Audio, &title.unwrap_or_default(), sub)
                    }
                    _ => div().into_any_element(),
                }
            };
            root = root.child(placeholder);
        }

        // OSD message.
        if let Some((msg, _)) = &self.osd {
            root = root.child(
                div()
                    .absolute()
                    .top(px(16.))
                    .left(px(16.))
                    .max_w(relative(0.8))
                    .px_3()
                    .py(px(6.))
                    .rounded(px(8.))
                    .bg(t.osd_bg)
                    .text_color(gpui::white())
                    .text_size(px(15.))
                    .child(msg.clone()),
            );
        }
        if st.buffering && active {
            root = root.child(
                div()
                    .absolute()
                    .top(px(16.))
                    .right(px(16.))
                    .px_3()
                    .py(px(6.))
                    .rounded(px(8.))
                    .bg(t.osd_bg)
                    .text_color(gpui::white())
                    .text_size(px(13.))
                    .child("Buffering…"),
            );
        }

        if self.dropping {
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .m_3()
                    .rounded(px(12.))
                    .border_2()
                    .border_color(t.accent)
                    .bg(t.accent_soft)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(gpui::white())
                    .text_size(px(18.))
                    .child("Drop to play"),
            );
        }
        root.into_any_element()
    }

    fn text_subtitles(
        &mut self,
        texts: &[String],
        rect: Bounds<Pixels>,
        area: Bounds<Pixels>,
        fullscreen_controls_visible: bool,
        dpi: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let h = f32::from(rect.size.height).max(100.0);
        let font = subtitle_style::font_size(h, self.settings.sub_scale);
        let picture_bottom = f32::from(rect.origin.y - area.origin.y) + f32::from(rect.size.height);
        let area_height = f32::from(area.size.height);
        let distance = h * self.settings.sub_margin;
        let left = f32::from(rect.origin.x - area.origin.x);
        let width = f32::from(rect.size.width);
        let lines: Vec<String> = texts
            .iter()
            .flat_map(|t| t.lines().map(|l| l.to_string()).collect::<Vec<_>>())
            .filter(|l| !l.trim().is_empty())
            .collect();
        let style = self.settings.sub_style;
        let font_weight = self.settings.sub_font_weight.clamp(100, 900);
        let gpui_weight = gpui::FontWeight(f32::from(font_weight));
        // Lazy embedded fallback registration, only when a text subtitle needs it.
        let font_family = subtitle_fonts::resolve_family(&self.settings.sub_font_family, cx);
        let line_height = subtitle_style::line_height(font);
        let row_pad = font * subtitle_style::BOX_VERTICAL_PADDING_FACTOR;
        let line_gap = font
            * if style == SubStyle::Outline {
                subtitle_style::OUTLINE_LINE_GAP_FACTOR
            } else {
                subtitle_style::LINE_GAP_FACTOR
            };
        // Render Outline rows before positioning: soft wrapping inside Cosmic Text
        // can turn one source line into multiple visual lines.
        let outline_scale = (dpi * subtitle_bitmap::supersampling()).clamp(0.5, 8.0);
        let outline_pad = subtitle_bitmap::padding(font, outline_scale) as f32 / outline_scale;
        let mut outline_bitmaps = Vec::new();
        let mut block_height = 0.0;
        for line in &lines {
            if style == SubStyle::Outline {
                let key = subtitle_bitmap::cache_key(line, &font_family, font_weight, font, width * 0.9, outline_scale);
                let bitmap = if let Some((image, image_w, image_h, age)) = self.outlined_sub_images.get_mut(&key) {
                    *age = 0;
                    Some((image.clone(), *image_w, *image_h))
                } else {
                    subtitle_bitmap::render_line(line, &font_family, font_weight, font, width * 0.9, outline_scale).map(
                        |(image, image_w, image_h)| {
                            self.outlined_sub_images.insert(key, (image.clone(), image_w, image_h, 0));
                            (image, image_w, image_h)
                        },
                    )
                };
                // Bitmap includes top/bottom stroke padding. The layout row does not:
                // the painted image starts one padding amount above its row.
                let row_height = bitmap.as_ref().map_or(line_height, |(_, _, image_h)| {
                    (*image_h as f32 / outline_scale - 2.0 * outline_pad).max(line_height)
                });
                block_height += row_height;
                outline_bitmaps.push((bitmap, row_height));
            } else {
                block_height += line_height + 2.0 * row_pad;
            }
        }
        block_height += line_gap * lines.len().saturating_sub(1) as f32;
        let margin = subtitle_style::bottom_margin(
            self.settings.sub_position,
            picture_bottom,
            area_height,
            distance,
            block_height,
        );
        // Fullscreen controls overlay the video. Keep the entire subtitle block
        // at least `distance` above the control bar, regardless of position mode.
        // Windowed controls are outside the video area and need no adjustment.
        let margin = subtitle_style::margin_above_controls(
            margin,
            distance,
            if fullscreen_controls_visible {
                FULLSCREEN_CONTROL_BAR_H
                    + if self.player.with_state(|st| self.thumbnails.has_previews(st, &self.settings, true)) {
                        self.settings.visualization_height(true) as f32 + 2.0
                    } else {
                        0.0
                    }
            } else {
                0.0
            },
        );
        // The outline texture extends below the layout row by its stroke padding.
        // Reserve that space too, especially above fullscreen controls.
        let margin = margin + if style == SubStyle::Outline { outline_pad } else { 0.0 };
        let mut col = div()
            .absolute()
            .left(px(left + width * 0.05))
            .w(px(width * 0.9))
            .bottom(px(margin))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(line_gap))
            .text_size(px(font))
            .line_height(px(subtitle_style::line_height(font)))
            .font_family(font_family.to_owned())
            .font_weight(gpui_weight);
        for (index, line) in lines.into_iter().enumerate() {
            let line: SharedString = line.into();
            let el: AnyElement = match style {
                SubStyle::Box => div()
                    .px(px(font * subtitle_style::BOX_HORIZONTAL_PADDING_FACTOR))
                    .py(px(font * subtitle_style::BOX_VERTICAL_PADDING_FACTOR))
                    .bg(rgba(0x000000b0))
                    .rounded(px(4.))
                    .font_family(font_family.to_owned())
                    .font_weight(gpui_weight)
                    .text_color(gpui::white())
                    .text_center()
                    .child(line)
                    .into_any_element(),
                SubStyle::Outline => {
                    // EXPERIMENT: both white fill and black stroke come from one
                    // Cosmic Text alpha mask, avoiding GPUI/Cosmic glyph mismatch.
                    let scale = outline_scale;
                    let (bitmap, row_height) = &outline_bitmaps[index];
                    if let Some((image, image_w, image_h)) = bitmap {
                        let logical_w = *image_w as f32 / scale;
                        let logical_h = *image_h as f32 / scale;
                        let pad = outline_pad;
                        let image = image.clone();
                        div()
                            .relative()
                            .w_full()
                            .h(px(*row_height))
                            .child(
                                canvas(
                                    |bounds, _, _| bounds,
                                    move |bounds, _, window, _| {
                                        let x = bounds.origin.x + px((f32::from(bounds.size.width) - logical_w) * 0.5);
                                        let y = bounds.origin.y - px(pad);
                                        let target = Bounds::new(point(x, y), size(px(logical_w), px(logical_h)));
                                        let _ = window.paint_image(target, Corners::default(), image.clone(), 0, false);
                                    },
                                )
                                .absolute()
                                .size_full(),
                            )
                            .into_any_element()
                    } else {
                        // Retain native rendering as a fallback if bitmap creation fails.
                        // Keep stroke and fill in GPUI, so both use the exact same
                        // resolved font face and weight (including fallback rules).
                        let offset = (font / 16.0).max(1.2);
                        // Unlike Box, Outline has no extra per-row vertical
                        // padding. Natural line height keeps consecutive lines compact.
                        let mut d = div().relative().w_full().h(px(line_height)).text_center();
                        for (dx, dy) in [
                            (-offset, 0.0),
                            (offset, 0.0),
                            (0.0, -offset),
                            (0.0, offset),
                            (-offset, -offset),
                            (offset, offset),
                            (-offset, offset),
                            (offset, -offset),
                        ] {
                            d = d.child(
                                div()
                                    .absolute()
                                    .left(px(dx))
                                    .top(px(dy))
                                    .w_full()
                                    .text_color(rgba(0x000000e6))
                                    .child(line.clone()),
                            );
                        }
                        d.child(div().absolute().top_0().w_full().text_color(gpui::white()).child(line))
                            .into_any_element()
                    }
                }
            };
            col = col.child(el);
        }
        col.into_any_element()
    }

    fn status_screen(&self, i: Icon, title: &str, sub: Option<String>) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .text_color(gpui::white())
            .child(icon(i, px(72.), rgba(0xffffff55).into()))
            .child(div().text_size(px(20.)).max_w(relative(0.8)).text_center().child(title.to_string()))
            .when_some(sub, |d, s| {
                d.child(
                    div().text_size(px(14.)).max_w(relative(0.8)).text_center().text_color(rgba(0xffffffaa)).child(s),
                )
            })
            .into_any_element()
    }

    fn idle_screen(&self, cx: &mut Context<Self>) -> AnyElement {
        let ot = overlay_theme(&self.theme);
        let tile = |this: &Self,
                    id: &'static str,
                    i: Icon,
                    label: &'static str,
                    f: fn(&mut Self, &mut Window, &mut Context<Self>),
                    cx: &mut Context<Self>| {
            let _ = this;
            div()
                .id(id)
                .w(px(116.))
                .h(px(92.))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .rounded(px(12.))
                .bg(rgba(0xffffff0d))
                .border_1()
                .border_color(rgba(0xffffff14))
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0xffffff1c)))
                .occlude()
                .child(icon(i, px(26.), ot.text))
                .child(div().text_size(px(13.)).text_color(ot.text).child(label))
                .on_click(cx.listener(move |this, _, w, cx| f(this, w, cx)))
        };
        let mut recent = div().flex().flex_col().gap(px(2.)).w(px(480.)).max_w(relative(0.9));
        if !self.settings.recent.is_empty() {
            recent = recent.child(div().text_size(px(12.)).text_color(ot.text_dim).mb_1().child("Recent"));
        }
        for (n, r) in self.settings.recent.iter().take(6).enumerate() {
            let src = r.clone();
            let ic = match r {
                MediaSource::Dvd { .. } | MediaSource::Cd { .. } => Icon::Disc,
                MediaSource::Url(_) => Icon::Url,
                MediaSource::File(p)
                    if crate::playlist::AUDIO_EXTS.contains(
                        &p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default().as_str(),
                    ) =>
                {
                    Icon::Audio
                }
                _ => Icon::Video,
            };
            recent = recent.child(
                div()
                    .id(SharedString::from(format!("recent-{n}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(30.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgba(0xffffff12)))
                    .occlude()
                    .child(icon(ic, px(16.), ot.text_dim))
                    .child(div().flex_1().truncate().text_size(px(13.)).text_color(ot.text).child(r.display_name()))
                    .on_click(cx.listener(move |this, _, w, cx| this.open_sources(vec![src.clone()], w, cx))),
            );
        }
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(img(ImageSource::Resource(Resource::Embedded("app-icon.svg".into()))).size(px(84.)))
            .child(
                div().text_size(px(26.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(ot.text).child("MavPlayer"),
            )
            .child(
                div().text_size(px(14.)).text_color(ot.text_dim).child("Drop files here, or choose something to play"),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_center()
                    .gap_3()
                    .mt_2()
                    .child(tile(
                        self,
                        "idle-file",
                        Icon::OpenFile,
                        "Open File",
                        |t, w, cx| t.prompt_open(false, false, w, cx),
                        cx,
                    ))
                    .child(tile(
                        self,
                        "idle-folder",
                        Icon::OpenFolder,
                        "Open Folder",
                        |t, w, cx| t.prompt_open(true, false, w, cx),
                        cx,
                    ))
                    .child(tile(
                        self,
                        "idle-url",
                        Icon::Url,
                        "Open URL",
                        |t, w, cx| t.show_dialog(super::dialogs::Dialog::OpenUrl, w, cx),
                        cx,
                    ))
                    .child(tile(
                        self,
                        "idle-disc",
                        Icon::Disc,
                        "Play Disc",
                        |t, w, cx| t.show_dialog(super::dialogs::Dialog::OpenDisc, w, cx),
                        cx,
                    )),
            )
            .child(recent)
            .into_any_element()
    }

    // ───────────────────────── control bar ─────────────────────────

    fn seek_bar(&self, t: Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let dur = self.player.duration();
        let pos = self.player.position();
        let st = self.player.state();
        let seekable = st.media.as_ref().map(|m| m.seekable && m.duration > 0.0).unwrap_or(false);
        let shown = self.shown_position();
        let frac = if dur > 0.0 { (shown / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        // How far the file has been read, never behind the playhead.
        let read = if st.cache_end.is_finite() { st.cache_end.max(pos) } else { pos + st.cache_secs };
        let cache = if dur > 0.0 { (read / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let (b1, b2, b3, b4) = (bounds.clone(), bounds.clone(), bounds.clone(), bounds.clone());
        let chapters: Vec<f32> = st
            .media
            .as_ref()
            .map(|m| m.chapters.iter().skip(1).filter(|_| dur > 0.0).map(|c| (c.start / dur) as f32).collect())
            .unwrap_or_default();
        let mut track = div()
            .h(px(4.))
            .w_full()
            .rounded_full()
            .bg(t.track)
            .relative()
            .group_hover("seek", |s| s.h(px(6.)))
            .child(div().absolute().top_0().left_0().h_full().w(relative(cache)).rounded_full().bg(t.track))
            .child(div().absolute().top_0().left_0().h_full().w(relative(frac)).rounded_full().bg(t.brand(90.)));
        for c in chapters {
            track = track.child(div().absolute().top_0().left(relative(c)).w(px(2.)).h_full().bg(rgba(0x000000a0)));
        }
        if let (Some(a), b) = self.ab_loop
            && dur > 0.0
        {
            let fa = (a / dur) as f32;
            let fb = b.map(|b| (b / dur) as f32).unwrap_or(fa + 0.002);
            track = track.child(
                div()
                    .absolute()
                    .top(px(-2.))
                    .bottom(px(-2.))
                    .left(relative(fa))
                    .w(relative((fb - fa).max(0.002)))
                    .bg(rgba(0xffd54f90)),
            );
        }
        let hover_label = self.seek_hover.filter(|_| dur > 0.0).map(|f| {
            let t_at = f as f64 * dur;
            let ch = st
                .media
                .as_ref()
                .and_then(|m| m.chapters.iter().rev().find(|c| c.start <= t_at).map(|c| c.title.clone()));
            (
                f,
                match ch {
                    Some(c) => format!("{}  ·  {}", format_time(t_at), c),
                    None => format_time(t_at),
                },
            )
        });
        let hover_preview = self.seek_hover.filter(|_| seekable).and_then(|f| {
            self.thumbnails
                .nearest(&st, &self.settings, f64::from(f) * dur, window.is_fullscreen())
                .map(|preview| (f, preview))
        });
        let mode = self.thumbnails.available_visualization(&st, &self.settings, window.is_fullscreen());
        let filmstrip = seekable && mode != TimelineVisualization::None;
        let preview_fraction =
            self.seek_hover.filter(|_| filmstrip && self.settings.timeline_hover_playhead && dur > 0.0).and_then(|f| {
                if let Some((_, preview)) = hover_preview {
                    return Some((preview.time / dur).clamp(0.0, 1.0) as f32);
                }
                if !mode.uses_video() {
                    return Some(f);
                }
                self.thumbnails
                    .nearest_sample(&st, &self.settings, f64::from(f) * dur)
                    .map(|p| (p.time / dur).clamp(0.0, 1.0) as f32)
            });
        let height = super::thumbnails::filmstrip_height(&self.settings, window, cx);
        let extra = if filmstrip { height + 2.0 } else { 0.0 };
        let resizing = self.drag.is_some()
            && (self.timeline_edge_drag
                || matches!(self.dialog, Some(super::dialogs::Dialog::Settings(1)))
                || self.menu.as_ref().is_some_and(|m| m.timeline_settings));
        div()
            .id("seek")
            .group("seek")
            .h(px(18. + extra))
            .flex_1()
            .relative()
            .flex()
            .items_center()
            .when(seekable, |d| d.cursor_pointer())
            .child(canvas(move |b, _, _| b1.set(b), |_, _, _, _| {}).absolute().size_full())
            .when(filmstrip, |d| {
                d.child(self.thumbnails.visualization(
                    mode,
                    dur,
                    height,
                    self.settings.visualization_opacity(window.is_fullscreen()) as f32 / 100.0,
                    resizing,
                ))
            })
            .when(filmstrip && self.settings.timeline_hover_playhead, |d| {
                d.when_some(preview_fraction, |d, f| {
                    d.child(
                        div()
                            .absolute()
                            .left(relative(f))
                            .top_0()
                            .h(px(height))
                            .w(px(1.))
                            .bg(rgba(0xffffffcc))
                            .shadow_sm(),
                    )
                })
            })
            .child(
                div().absolute().bottom_0().left_0().w_full().h(px(18.)).flex().items_center().child(track).child(
                    div()
                        .absolute()
                        .left(relative(frac))
                        .ml(px(-7.))
                        .size(px(14.))
                        .rounded_full()
                        .bg(gpui::white())
                        .shadow_md()
                        .opacity(if self.scrub.is_some() { 1.0 } else { 0.0 })
                        .group_hover("seek", |s| s.opacity(1.0)),
                ),
            )
            .when_some(hover_preview, |d, (f, preview)| {
                d.child(super::thumbnails::hover_preview(
                    f,
                    preview,
                    Hsla { a: 1.0, ..t.osd_bg },
                    extra,
                    match st.media.as_ref().and_then(|m| m.chapters.iter().rev().find(|c| c.start <= preview.time)) {
                        Some(c) => format!("{}  ·  {}", format_time(preview.time), c.title),
                        None => format_time(preview.time),
                    },
                ))
            })
            .when_some(hover_label.filter(|_| hover_preview.is_none()), |d, (f, label)| {
                d.child(
                    // A wide box centred on the pointer, with the label centred in it,
                    // so labels of any width sit centred above the pointer.
                    div()
                        .absolute()
                        .bottom(px(22. + extra))
                        .left(relative(f))
                        .ml(px(-200.))
                        .w(px(400.))
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .flex_none()
                                .px_2()
                                .py(px(3.))
                                .rounded(px(5.))
                                .bg(t.osd_bg)
                                .text_color(gpui::white())
                                .text_size(px(12.))
                                .whitespace_nowrap()
                                .child(label),
                        ),
                )
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    if this.menu.as_ref().is_some_and(|m| m.timeline_settings) {
                        this.close_menu(cx);
                    } else {
                        this.seek_hover = None;
                        this.playlist_previews.hover = None;
                        window.focus(&this.focus);
                        let bar = this.control_bounds.get();
                        let top = if bar.size.height > px(0.) { bar.top() } else { b4.get().top() };
                        this.open_menu(Vec::new(), point(ev.position.x, top), None, cx);
                        if let Some(menu) = this.menu.as_mut() {
                            menu.timeline_settings = true;
                        }
                    }
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                if this.scrub.is_some() || this.timeline_edge_drag {
                    return; // the drag keeps the label under the pointer
                }
                let f = frac_in(b2.get(), ev.position, false);
                if this.seek_hover != Some(f) {
                    this.seek_hover = Some(f);
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, h: &bool, _, cx| {
                if !*h && this.scrub.is_none() {
                    this.seek_hover = None;
                    cx.notify();
                }
            }))
            .when(seekable, |d| {
                d.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        window.focus(&this.focus);
                        let f = frac_in(b3.get(), ev.position, false);
                        this.scrub_start(f, cx);
                        let on_change: SliderFn = Rc::new(|this, f, _, cx| this.scrub_move(f, cx));
                        let on_release: SliderFn = Rc::new(|this, f, _, cx| this.scrub_end(f, cx));
                        this.drag = Some(Drag {
                            bounds: b3.clone(),
                            vertical: false,
                            on_change,
                            on_release: Some(on_release),
                            last: f,
                        });
                        cx.stop_propagation();
                    }),
                )
            })
            .into_any_element()
    }

    fn background_activity(&self, t: Theme) -> AnyElement {
        let Some(progress) = self.thumbnails.progress() else {
            return div().into_any_element();
        };
        super::activity::indicator(
            "thumbnail-progress",
            t,
            move || {
                let (count, percent, extracted) = progress.snapshot();
                format!("Generating up to {count} thumbnails ({percent}% done, {extracted} generated)")
            },
            None,
        )
    }

    pub fn control_bar(&self, t: Theme, overlay: bool, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let st = self.player.state();
        let active = self.player.is_active();
        let paused = self.player.paused() || st.status == Status::Ended;
        let dur = self.player.duration();
        // The time follows the progress bar while it is dragged.
        let pos = self.shown_position();
        let time_left = format_time(pos);
        let time_right = if self.settings.show_remaining_time && dur > 0.0 {
            format!("-{}", format_time(dur - pos))
        } else {
            format_time(dur)
        };
        let vol = self.player.volume();
        let muted = self.player.muted();
        let vol_icon = if muted || vol <= 0.001 {
            Icon::VolumeMuted
        } else if vol < 0.34 {
            Icon::VolumeLow
        } else if vol < 0.67 {
            Icon::VolumeMedium
        } else {
            Icon::VolumeHigh
        };
        let max_vol = self.settings.max_volume as f32 / 100.0;
        let has_subs = st.media.as_ref().map(|m| m.tracks_of(TrackKind::Subtitle).count() > 0).unwrap_or(false);
        let audio_tracks = st.media.as_ref().map(|m| m.tracks_of(TrackKind::Audio).count()).unwrap_or(0);
        let speed = self.player.speed();
        let repeat_icon = match self.playlist.repeat {
            RepeatMode::Off => Icon::RepeatOff,
            RepeatMode::All => Icon::RepeatAll,
            RepeatMode::One => Icon::RepeatOne,
        };
        let fullscreen = window.is_fullscreen();
        let title = self
            .playlist
            .current
            .and_then(|i| self.playlist.items.get(i))
            .and_then(|item| item.custom_title.clone())
            .or_else(|| st.media.as_ref().map(|m| m.title.clone().unwrap_or_else(|| m.source_name.clone())))
            .or_else(|| st.source.as_ref().map(|s| s.display_name()))
            .unwrap_or_default();
        let dim = t.text_dim;
        let time = |s: String| div().text_size(px(12.)).text_color(dim).min_w(px(52.)).flex().justify_center().child(s);

        let seek_row =
            div().flex().items_center().gap_2().child(time(time_left)).child(self.seek_bar(t, window, cx)).child(
                div()
                    .id("time-right")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings.show_remaining_time = !this.settings.show_remaining_time;
                        cx.notify();
                    }))
                    .child(time(time_right)),
            );

        let pp_icon = if paused || !active { Icon::Play } else { Icon::Pause };
        let play = div()
            .id("btn-play")
            .size(px(38.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .bg(t.brand(180.))
            .shadow_sm()
            .hover(|s| s.opacity(0.9))
            .child(icon(pp_icon, px(18.), t.on_accent))
            .tooltip(tooltip("Play / Pause (Space)", t))
            .on_click(cx.listener(|this, _, w, cx| {
                if this.player.is_active() {
                    this.player.toggle_pause();
                } else if let Some(i) =
                    this.playlist.current.or(if this.playlist.items.is_empty() { None } else { Some(0) })
                {
                    this.play_index(i, w, cx);
                } else {
                    this.prompt_open(false, false, w, cx);
                }
                cx.notify();
            }));

        let center = div().flex_1().min_w_0().px_2().truncate().text_size(px(13.)).text_color(t.text_dim).child(title);

        let popup_btn =
            |this: &Self, id: &'static str, i: Icon, tip: &str, items: fn(&Self) -> Vec<M>, cx: &mut Context<Self>| {
                this.icon_button(
                    id,
                    i,
                    tip,
                    t,
                    move |this, ev, _, cx| {
                        let pos = ev.position();
                        let list = items(this);
                        this.open_menu(list, point(pos.x - px(120.), pos.y - px(10.)), None, cx);
                    },
                    cx,
                )
            };

        // The chosen controls that apply to what is playing, minus any that
        // don't fit (the window's minimum width normally leaves room for all).
        let wanted: Vec<BarControl> = self
            .settings
            .bar_controls
            .iter()
            .copied()
            .filter(|&c| match c {
                BarControl::Subtitles => has_subs,
                BarControl::AudioTrack => audio_tracks > 1,
                _ => true,
            })
            .collect();
        let shown = super::bar::fit(&wanted, f32::from(window.viewport_size().width));

        let mut left = div().flex().items_center().gap_1().child(play);
        let mut right = div().flex().items_center().gap_1();
        if self.thumbnails.generating() {
            right = right.child(self.background_activity(t));
        }
        if let Some(activity) = self.thumbnails.waveform.progress() {
            right = right.child(super::activity::indicator("waveform-progress", t, move || activity.label(), None));
        }
        if let Some(activity) = self.playlist_previews.progress() {
            right = right.child(super::activity::indicator(
                "playlist-thumbnail-progress",
                t,
                move || activity.label(),
                None,
            ));
        }
        for c in shown {
            let el: AnyElement = match c {
                BarControl::Stop => self
                    .icon_button("btn-stop", Icon::Stop, "Stop", t, |this, _, w, cx| this.stop(w, cx), cx)
                    .into_any_element(),
                BarControl::Previous => self
                    .icon_button(
                        "btn-prev",
                        Icon::Previous,
                        &keymap::hint("Previous", &PlayPrevious),
                        t,
                        |this, _, w, cx| this.prev_item(w, cx),
                        cx,
                    )
                    .into_any_element(),
                BarControl::Next => self
                    .icon_button(
                        "btn-next",
                        Icon::Next,
                        &keymap::hint("Next", &PlayNext),
                        t,
                        |this, _, w, cx| this.next_item(w, cx),
                        cx,
                    )
                    .into_any_element(),
                BarControl::Subtitles => {
                    popup_btn(self, "btn-subs", Icon::Subtitles, "Subtitles", |s| s.subtitle_popup(), cx)
                        .into_any_element()
                }
                BarControl::AudioTrack => {
                    popup_btn(self, "btn-audio", Icon::Audio, "Audio track", |s| s.audio_popup(), cx).into_any_element()
                }
                BarControl::Speed => div()
                    .id("speed")
                    .flex()
                    .flex_none()
                    .items_center()
                    .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                        let dy = match ev.delta {
                            ScrollDelta::Lines(p) => p.y,
                            ScrollDelta::Pixels(p) => f32::from(p.y) / 40.0,
                        };
                        if dy.abs() >= 0.2 {
                            let s = this.player.speed() + 0.05 * dy.signum() as f64;
                            this.set_speed((s / 0.05).round() * 0.05, cx);
                        }
                        cx.stop_propagation();
                    }))
                    .child(
                        div()
                            .id("btn-speed")
                            .w(px(super::bar::SPEED_LABEL))
                            .h(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(7.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(if (speed - 1.0).abs() > 1e-3 { t.accent } else { t.text })
                            .hover(|s| s.bg(t.hover))
                            .child(format_speed(speed))
                            .tooltip(tooltip("Playback speed: click for normal speed", t))
                            .on_click(cx.listener(|this, _, _, cx| this.set_speed(1.0, cx))),
                    )
                    .child(div().w(px(super::bar::SPEED_SLIDER)).child(self.slider(
                        "speed-slider",
                        speed_to_frac(speed),
                        false,
                        t,
                        |this, f, _, cx| this.set_speed(frac_to_speed(f), cx),
                        cx,
                    )))
                    .into_any_element(),
                BarControl::Repeat => self
                    .icon_button(
                        "btn-repeat",
                        repeat_icon,
                        &keymap::hint("Repeat", &CycleRepeat),
                        t,
                        |this, _, _, cx| this.cycle_repeat(cx),
                        cx,
                    )
                    .when(self.playlist.repeat != RepeatMode::Off, |d| d.bg(t.accent_soft))
                    .into_any_element(),
                BarControl::Shuffle => self
                    .icon_button(
                        "btn-shuffle",
                        Icon::Shuffle,
                        &keymap::hint("Shuffle", &ToggleShuffle),
                        t,
                        |this, _, _, cx| {
                            let s = !this.playlist.shuffle();
                            this.playlist.set_shuffle(s);
                            this.settings.shuffle = s;
                            this.osd(if s { "Shuffle: on" } else { "Shuffle: off" }, cx);
                        },
                        cx,
                    )
                    .when(self.playlist.shuffle(), |d| d.bg(t.accent_soft))
                    .into_any_element(),
                BarControl::Playlist => self
                    .icon_button(
                        "btn-playlist",
                        Icon::Playlist,
                        &keymap::hint("Playlist", &TogglePlaylist),
                        t,
                        |this, _, w, cx| this.toggle_playlist(w, cx),
                        cx,
                    )
                    .when(self.playlist_enabled(), |d| d.bg(t.accent_soft))
                    .into_any_element(),
                BarControl::Settings => self
                    .icon_button(
                        "btn-settings",
                        Icon::Settings,
                        "Preferences",
                        t,
                        |this, _, w, cx| this.show_dialog(super::dialogs::Dialog::Settings(0), w, cx),
                        cx,
                    )
                    .into_any_element(),
                BarControl::Fullscreen => self
                    .icon_button(
                        "btn-fs",
                        if fullscreen { Icon::ExitFullscreen } else { Icon::Fullscreen },
                        &keymap::hint("Fullscreen", &ToggleFullscreen),
                        t,
                        |_, _, w, _| w.toggle_fullscreen(),
                        cx,
                    )
                    .into_any_element(),
                BarControl::Volume => div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .pl(px(super::bar::VOLUME_LEAD))
                    .child(self.icon_button(
                        "btn-mute",
                        vol_icon,
                        &keymap::hint("Mute", &ToggleMute),
                        t,
                        |this, _, _, cx| {
                            let m = !this.player.muted();
                            this.player.set_muted(m);
                            this.settings.muted = m;
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(div().w(px(super::bar::VOLUME_SLIDER)).child(self.slider(
                        "volume",
                        if muted { 0.0 } else { vol as f32 / max_vol },
                        false,
                        t,
                        move |this, f, _, cx| this.set_volume(f as f64 * max_vol as f64, cx),
                        cx,
                    )))
                    .child(
                        div().flex_none().pl(px(super::bar::VOLUME_TEXT_LEAD)).child(
                            match self.inputs.get("volume").filter(|_| self.editing_volume) {
                                Some(input) => div()
                                    .w(px(44.))
                                    .h(px(22.))
                                    .px_1()
                                    .flex()
                                    .items_center()
                                    .rounded(px(5.))
                                    .bg(t.surface_alt)
                                    .border_1()
                                    .border_color(t.accent)
                                    .text_size(px(11.))
                                    .child(input.clone())
                                    .into_any_element(),
                                None => div()
                                    .id("volume-text")
                                    .w(px(44.))
                                    .text_size(px(11.))
                                    .text_color(t.text_dim)
                                    .cursor_pointer()
                                    .child(format!("{:.0}%", vol * 100.0))
                                    .tooltip(tooltip("Double-click to type a volume", t))
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .on_click(cx.listener(|this, ev: &ClickEvent, window, cx| {
                                        cx.stop_propagation();
                                        if ev.click_count() >= 2 {
                                            this.start_volume_edit(window, cx);
                                        }
                                    }))
                                    .into_any_element(),
                            },
                        ),
                    )
                    .into_any_element(),
            };
            if c.on_left() {
                left = left.child(el);
            } else {
                right = right.child(el);
            }
        }

        let filmstrip = dur > 0.0
            && st.media.as_ref().is_some_and(|m| m.seekable)
            && self.thumbnails.available_visualization(&st, &self.settings, fullscreen) != TimelineVisualization::None;
        let height = super::thumbnails::filmstrip_height(&self.settings, window, cx);
        let control_bounds = self.control_bounds.clone();
        div()
            .id(if overlay { "controls-overlay" } else { "controls" })
            .relative()
            .child(
                canvas(move |b, _, _| control_bounds.set(b), |_, _, _, _| {}).absolute().top_0().left_0().size_full(),
            )
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .pt(px(6.))
            .pb_2()
            .when(!overlay, |d| d.bg(t.chrome).border_t_1().border_color(t.chrome_border))
            .text_color(t.text)
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, cx| this.note_activity(cx)))
            .child(seek_row)
            .child(div().flex().items_center().gap_2().child(left).child(center).child(right))
            .when(filmstrip, |d| {
                d.child(
                    div()
                        .id("timeline-resize")
                        .absolute()
                        .top_0()
                        .left_0()
                        .w_full()
                        .h(px(10.))
                        .cursor(CursorStyle::ResizeUpDown)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                window.focus(&this.focus);
                                let start = height.clamp(12., 96.);
                                let bottom = ev.position.y + px(start - 12.);
                                let bounds = Rc::new(Cell::new(Bounds::new(
                                    point(px(0.), bottom - px(84.)),
                                    size(px(1.), px(84.)),
                                )));
                                this.timeline_edge_drag = true;
                                this.seek_hover = None;
                                this.drag = Some(Drag {
                                    bounds,
                                    vertical: true,
                                    last: (start - 12.) / 84.,
                                    on_change: Rc::new(move |this, f, _, _| {
                                        let n = (12. + f * 84.).round() as u32;
                                        if n == start.round() as u32
                                            && ((!fullscreen && this.settings.timeline_adaptive)
                                                || (fullscreen && this.settings.timeline_fullscreen_height.is_none()))
                                        {
                                            return;
                                        }
                                        if fullscreen {
                                            this.settings.timeline_fullscreen_height = Some(n);
                                        } else {
                                            this.settings.timeline_adaptive = false;
                                            this.settings.timeline_filmstrip_height = n;
                                            this.settings.timeline_filmstrip_min_height =
                                                this.settings.timeline_filmstrip_min_height.min(n);
                                        }
                                    }),
                                    on_release: Some(Rc::new(|this, _, _, _| this.settings.save())),
                                });
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                )
            })
            .into_any_element()
    }

    // ───────────────────────── playlist panel ─────────────────────────

    pub(super) fn playlist_row_height(&self) -> f32 {
        if super::playlist_thumbnails::PlaylistPreviews::mode(&self.settings).inline() { 52.0 } else { ROW_H }
    }

    fn playlist_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.playlist_row_bounds.borrow_mut().clear();
        self.playlist_scrub_bounds.borrow_mut().clear();
        let t = self.theme;
        let viewport = window.viewport_size();
        let max_width = (f32::from(viewport.width) - 120.).max(120.);
        let width = self.settings.playlist_width.clamp(120., max_width);
        let total_count = self.playlist.items.len();
        let filter_input = if self.playlist_filter_open {
            Some(self.input("playlist_filter", "Filter playlist…", window, cx))
        } else {
            None
        };
        let filtered = self.visible_playlist_indices(cx);
        let count = filtered.len();
        let total: f64 = self.playlist.items.iter().filter_map(|i| i.duration).sum();
        let selected_count = self.playlist_selection.ids(&self.playlist.items).len();
        let switch_bounds = Rc::new(Cell::new(Bounds::default()));
        let measured_switch = switch_bounds.clone();
        let collection_bounds = Rc::new(Cell::new(Bounds::default()));
        let measured_collection = collection_bounds.clone();
        let collection_header = div()
            .id("collection-switch")
            .relative()
            .h(px(30.))
            .flex_none()
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .border_b_1()
            .border_color(t.chrome_border)
            .cursor_pointer()
            .hover(|s| s.bg(t.hover))
            .child(canvas(move |b, _, _| measured_collection.set(b), |_, _, _, _| {}).absolute().size_full())
            .child(icon(Icon::OpenFolder, px(16.), t.text_dim))
            .child(div().flex_1().min_w_0().truncate().text_size(px(13.)).child(self.playlist_library.collection_label().to_owned()))
            .child(icon(Icon::DropDown, px(12.), t.text_dim))
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                if !ev.is_right_click() {
                    this.open_collection_switch(super::menu::below(collection_bounds.get()), window, cx);
                    cx.stop_propagation();
                }
            }));
        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(40.))
            .border_b_1()
            .border_color(t.chrome_border)
            .child(
                div()
                    .id("playlist-switch")
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .child(canvas(move |b, _, _| measured_switch.set(b), |_, _, _, _| {}).absolute().size_full())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(self.playlist_library.name().to_owned()),
                            )
                            .child(div().text_size(px(11.)).text_color(t.text_dim).truncate().child(format!(
                                "{total_count} item{}{}{}",
                                if total_count == 1 { "" } else { "s" },
                                if total > 0.0 { format!(" · {}", format_time(total)) } else { String::new() },
                                if selected_count > 1 {
                                    format!(" · {selected_count} selected")
                                } else {
                                    String::new()
                                }
                            ))),
                    )
                    .child(icon(Icon::DropDown, px(12.), t.text_dim))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        if ev.is_right_click() {
                            return;
                        }
                        window.focus(&this.playlist_focus);
                        this.open_playlist_switch(super::menu::below(switch_bounds.get()), window, cx);
                        cx.stop_propagation();
                    })),
            )
            .child(self.icon_button(
                "pl-search",
                Icon::Search,
                &keymap::hint("Filter playlist", &PlaylistFilter),
                t,
                |this, _, window, cx| {
                    this.playlist_filter_open = !this.playlist_filter_open;
                    if this.playlist_filter_open {
                        let input = this.input("playlist_filter", "Filter playlist…", window, cx);
                        window.focus(&input.focus_handle(cx));
                    } else {
                        window.focus(&this.playlist_focus);
                    }
                    cx.notify();
                },
                cx,
            ))
            .child(self.icon_button(
                "pl-add",
                Icon::Add,
                "Add files",
                t,
                |this, _, w, cx| {
                    this.prompt_playlist_add(false, w, cx);
                },
                cx,
            ))
            .child(self.icon_button(
                "pl-folder",
                Icon::OpenFolder,
                "Add folder",
                t,
                |this, _, w, cx| {
                    this.prompt_playlist_add(true, w, cx);
                },
                cx,
            ))
            .child(self.icon_button(
                "pl-settings",
                Icon::Settings,
                "Playlist settings",
                t,
                |this, _, w, cx| this.open_playlist_settings(w, cx),
                cx,
            ));

        let search_row = filter_input.map(|input| {
            div().px_2().py_1().border_b_1().border_color(t.chrome_border).child(input).into_any_element()
        });
        let current = self.playlist.current;
        let drop_at = self.pl_drop;
        let filter_active = self.playlist_filter_open
            && self.inputs.get("playlist_filter").is_some_and(|input| !input.read(cx).text().trim().is_empty());
        let list = gpui::uniform_list(
            "playlist-items",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                this.playlist_row_bounds.borrow_mut().clear();
                this.playlist_scrub_bounds.borrow_mut().clear();
                let t = this.theme;
                let badge_entries = range
                    .clone()
                    .filter_map(|i| {
                        filtered
                            .get(i)
                            .and_then(|&j| this.playlist.items.get(j))
                            .and_then(|it| super::cache_badges::Entry::new(it, &this.settings))
                    })
                    .collect();
                this.cache_badges.retain_visible(badge_entries);
                if super::playlist_thumbnails::PlaylistPreviews::mode(&this.settings).inline() {
                    let visible: Vec<_> = range
                        .clone()
                        .filter_map(|i| {
                            filtered
                                .get(i)
                                .and_then(|&j| this.playlist.items.get(j))
                                .and_then(|it| super::playlist_thumbnails::Key::new(it, &this.settings))
                        })
                        .collect();
                    this.playlist_previews.retain_visible(&visible);
                }
                range
                    .map(|display_index| {
                        let i = filtered[display_index];
                        let it = &this.playlist.items[i];
                        let is_cur = current == Some(i);
                        let mode = super::playlist_thumbnails::PlaylistPreviews::mode(&this.settings);
                        let labels = if mode.inline() {
                            this.settings.playlist_inline_labels
                        } else if this.settings.playlist_compact_prefer_title {
                            PlaylistLabels::Title
                        } else {
                            PlaylistLabels::FileName
                        };
                        let (title, secondary_title) = it.labels(labels);
                        let dur = if mode == PlaylistThumbnails::Inline {
                            this.playlist_previews
                                .hover
                                .as_ref()
                                .filter(|h| h.id == it.id)
                                .map(|h| format_time(h.duration * f64::from(h.fraction)))
                                .or_else(|| it.duration.map(format_time))
                                .unwrap_or_default()
                        } else {
                            it.duration.map(format_time).unwrap_or_default()
                        };
                        let dragging = cx.has_active_drag();
                        let marker = |top: bool| {
                            div()
                                .absolute()
                                .left_1()
                                .right_1()
                                .h(px(2.))
                                .rounded_full()
                                .bg(t.accent)
                                .when(top, |d| d.top(px(-1.)))
                                .when(!top, |d| d.bottom(px(-1.)))
                        };
                        let item_id = it.id;
                        let selected = this.playlist_selection.contains(item_id);
                        let drag_ids =
                            if selected { this.playlist_selection.ids(&this.playlist.items) } else { vec![item_id] };
                        let drag_title =
                            if drag_ids.len() > 1 { format!("{} items", drag_ids.len()) } else { title.clone() };
                        let drag_value = PlaylistDrag { ids: drag_ids, anchor: item_id, title: drag_title.into() };
                        let failed = it.failed;
                        let duration = it.duration.unwrap_or(0.0);
                        let key = super::playlist_thumbnails::Key::new(it, &this.settings);
                        let entry = super::cache_badges::Entry::new(it, &this.settings);
                        let cached = entry.as_ref().map(|entry| this.cache_badges.get(entry)).unwrap_or_default();
                        let audio_preview = if it.video_stream.is_none() && mode.inline() {
                            entry.as_ref().and_then(|entry| this.cache_badges.waveform(entry))
                        } else {
                            None
                        };
                        // Scrubbing reads cached filmstrip samples. Without them a row has
                        // nothing to scrub through, so it behaves like a static thumbnail.
                        let panel_item = this.menu.as_ref().and_then(|m| m.playlist_item);
                        let can_scrub = (mode.scrubbing() || panel_item == Some(item_id))
                            && key.as_ref().is_some_and(|key| {
                                cached.video
                                    || this.thumbnails.for_playlist(&key.path, key.stream, key.count, 0.0).is_some()
                                    || this.playlist_previews.scrub(key, 0.0).is_some()
                            });
                        let bounds = Rc::new(Cell::new(Bounds::default()));
                        let measured = bounds.clone();
                        let scrub_bounds = this.playlist_scrub_bounds.clone();
                        let image = if mode.inline() {
                            key.as_ref().and_then(|key| {
                                let representative = key.representative_time(&this.settings, duration);
                                let scrub = mode == PlaylistThumbnails::Inline
                                    && (this.menu.is_none() || panel_item == Some(item_id))
                                    && this.playlist_previews.hover.as_ref().is_some_and(|h| h.id == item_id);
                                let time = if scrub {
                                    this.playlist_previews
                                        .hover
                                        .as_ref()
                                        .map_or(representative, |h| duration * f64::from(h.fraction))
                                } else {
                                    representative
                                };
                                this.thumbnails
                                    .for_playlist(&key.path, key.stream, key.count, time)
                                    .cloned()
                                    .or_else(|| {
                                        if scrub { this.playlist_previews.scrub(key, time).cloned() } else { None }
                                    })
                                    .or_else(|| {
                                        this.playlist_previews.inline_pair(
                                            key,
                                            representative,
                                            duration * crate::engine::thumbnails::representative_fraction(&key.path),
                                        )
                                    })
                            })
                        } else {
                            None
                        };
                        let hovered = this.playlist_hovered == Some(item_id) && this.menu.is_none() && !dragging;
                        // GPUI reports hover only when the mouse moves. When a removal slides
                        // another row under a still cursor, this check at paint time makes that
                        // row hovered at once, so its remove button can be clicked right away.
                        let marked_hovered = this.playlist_hovered == Some(item_id);
                        let view = cx.entity().downgrade();
                        let row_bounds = this.playlist_row_bounds.clone();
                        let hover_sync = canvas(
                            move |b, window, _| {
                                let mut rows = row_bounds.borrow_mut();
                                if let Some((_, bounds)) = rows.iter_mut().find(|(id, _)| *id == item_id) {
                                    *bounds = b;
                                } else {
                                    rows.push((item_id, b));
                                }
                                window.insert_hitbox(b, gpui::HitboxBehavior::Normal)
                            },
                            move |_, hitbox, window, cx| {
                                let over = hitbox.is_hovered(window) && !cx.has_active_drag();
                                if over == marked_hovered {
                                    return;
                                }
                                cx.defer(move |cx| {
                                    view.update(cx, |this, cx| {
                                        if this.menu.is_some() {
                                            return;
                                        }
                                        if over && this.playlist_hovered != Some(item_id) {
                                            this.playlist_hovered = Some(item_id);
                                            cx.notify();
                                        } else if !over && this.playlist_hovered == Some(item_id) {
                                            this.playlist_hovered = None;
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                                });
                            },
                        )
                        .absolute()
                        .size_full();
                        let hover_playhead = this
                            .playlist_previews
                            .hover
                            .as_ref()
                            .filter(|h| {
                                (hovered && mode.scrubbing() || panel_item == Some(item_id))
                                    && this.dialog.is_none() && h.id == item_id
                            })
                            .and_then(|h| {
                                if h.duration <= 0.0 {
                                    return None;
                                }
                                let time = h.duration * f64::from(h.fraction);
                                let preview = this
                                    .thumbnails
                                    .for_playlist(&h.key.path, h.key.stream, h.key.count, time)
                                    .or_else(|| this.playlist_previews.scrub(&h.key, time));
                                preview.map(|p| (p.time / h.duration).clamp(0.0, 1.0) as f32)
                            });
                        let playing = is_cur
                            && this.player.is_active()
                            && !this.player.paused()
                            && this.player.with_state(|s| s.status != Status::Ended);
                        let play = (hovered || is_cur).then(|| {
                            this.icon_button(
                                ("pl-play", item_id),
                                if playing { Icon::Pause } else { Icon::Play },
                                if playing { "Pause" } else { "Play" },
                                t,
                                move |this, ev, w, cx| {
                                    if ev.is_right_click() {
                                        return;
                                    }
                                    cx.stop_propagation();
                                    w.focus(&this.playlist_focus);
                                    if let Some(index) = this.playlist.items.iter().position(|it| it.id == item_id) {
                                        if this.playlist.current == Some(index) && this.player.is_active() {
                                            this.player.toggle_pause();
                                        } else {
                                            this.play_index(index, w, cx);
                                        }
                                        cx.notify();
                                    }
                                },
                                cx,
                            )
                            .size(px(22.))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .into_any_element()
                        });
                        // Keep the play/pause button outside the preview area in every mode.
                        let row_play = play;
                        div()
                            .id(("pl-row", item_id))
                            .relative()
                            .h(px(this.playlist_row_height()))
                            .w_full()
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .when(is_cur, |d| d.bg(t.accent_soft))
                            .when(selected && !is_cur, |d| d.bg(t.pressed))
                            .when(selected && is_cur, |d| d.bg(t.accent_soft).border_1().border_color(t.accent))
                            .when(this.menu.as_ref().and_then(|m| m.playlist_item) == Some(item_id), |d| d.border_1().border_color(t.accent))
                            .when(this.menu.as_ref().and_then(|m| m.playlist_item).is_some_and(|id| id != item_id), |d| d.opacity(0.55))
                            .hover(|s| s.bg(t.hover))
                            .child(hover_sync)
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if *hovered {
                                    if this.playlist_hovered != Some(item_id) {
                                        this.playlist_hovered = Some(item_id);
                                        cx.notify();
                                    }
                                } else {
                                    if this.playlist_hovered == Some(item_id) {
                                        this.playlist_hovered = None;
                                        cx.notify();
                                    }
                                    if this.menu.is_none()
                                        && this.playlist_previews.hover.as_ref().is_some_and(|h| h.id == item_id)
                                    {
                                        this.playlist_previews.hover = None;
                                        cx.notify();
                                    }
                                }
                            }))
                            .child(
                                div()
                                    .w(px(22.))
                                    .flex_none()
                                    .flex()
                                    .justify_center()
                                    .text_size(px(11.))
                                    .text_color(t.text_dim)
                                    .items_center()
                                    .child(row_play.unwrap_or_else(|| {
                                        if is_cur {
                                            icon(Icon::Play, px(12.), t.accent).into_any_element()
                                        } else {
                                            div().child(format!("{}", i + 1)).into_any_element()
                                        }
                                    })),
                            )
                            .when(
                                mode == PlaylistThumbnails::None
                                    && (it.video_stream.is_some() || it.audio_stream.is_some()),
                                |d| {
                                    d.child(icon(
                                        if it.video_stream.is_some() { Icon::Video } else { Icon::Audio },
                                        px(14.),
                                        t.text_dim,
                                    ))
                                },
                            )
                            .child(
                                div()
                                    .id(("pl-scrub", item_id))
                                    .relative()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .overflow_hidden()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .when(can_scrub, |d| {
                                        d.child(
                                            canvas(move |b, _, _| {
                                                measured.set(b);
                                                let mut rows = scrub_bounds.borrow_mut();
                                                if let Some((_, bounds)) = rows.iter_mut().find(|(id, _)| *id == item_id) {
                                                    *bounds = b;
                                                } else {
                                                    rows.push((item_id, b));
                                                }
                                            }, |_, _, _, _| {})
                                                .absolute()
                                                .size_full(),
                                        )
                                        .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                                            if cx.has_active_drag() || this.menu.is_some() || this.dialog.is_some() {
                                                return;
                                            }
                                            let Some(key) = key.clone() else {
                                                return;
                                            };
                                            let b = bounds.get();
                                            if !b.contains(&ev.position) || b.size.width <= px(0.) {
                                                return;
                                            }
                                            let fraction = frac_in(b, ev.position, false);
                                            this.playlist_previews.hover = Some(super::playlist_thumbnails::Hover {
                                                id: item_id,
                                                key,
                                                duration,
                                                fraction,
                                                bounds: b,
                                            });
                                            this.refresh_playlist_hover(cx);
                                            cx.notify();
                                        }))
                                        .on_hover(cx.listener(
                                            move |this, hovered: &bool, _, cx| {
                                                if !*hovered
                                                    && this.menu.is_none()
                                                    && this
                                                        .playlist_previews
                                                        .hover
                                                        .as_ref()
                                                        .is_some_and(|h| h.id == item_id)
                                                {
                                                    this.playlist_previews.hover = None;
                                                    this.refresh_playlist_hover(cx);
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                    })
                                    .when(mode.inline(), |d| {
                                        d.child(
                                            div()
                                                .relative()
                                                .w(px(72.))
                                                .h(px(44.))
                                                .flex_none()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .when(image.is_none() && audio_preview.is_none(), |d| {
                                                    d.rounded(px(4.)).bg(t.hover).child(icon(
                                                        default_media_icon(it),
                                                        px(20.),
                                                        t.text_dim,
                                                    ))
                                                })
                                                .when_some(image, |d, p| d.child(p.element(72., 44.)))
                                                .when_some(audio_preview, |d, peaks| {
                                                    d.child(waveform_thumbnail(peaks, t.accent))
                                                })
                                                .when(cached.video, |d| {
                                                    d.child(
                                                        div()
                                                            .absolute()
                                                            .bottom_0()
                                                            .left_0()
                                                            .rounded(px(2.))
                                                            .bg(rgba(0x000000b0))
                                                            .p(px(2.))
                                                            .child(icon(Icon::Video, px(12.), gpui::white())),
                                                    )
                                                })
                                                .when(cached.audio && it.video_stream.is_some(), |d| {
                                                    d.child(
                                                        div()
                                                            .absolute()
                                                            .bottom_0()
                                                            .right_0()
                                                            .rounded(px(2.))
                                                            .bg(rgba(0x000000b0))
                                                            .p(px(2.))
                                                            .child(waveform_badge(gpui::white())),
                                                    )
                                                }),
                                        )
                                    })
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .justify_center()
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(13.))
                                                    .text_color(if failed {
                                                        t.text_disabled
                                                    } else if is_cur {
                                                        t.accent
                                                    } else {
                                                        t.text
                                                    })
                                                    .child(title),
                                            )
                                            .when_some(secondary_title, |d, name| {
                                                d.child(
                                                    div().truncate().text_size(px(11.)).text_color(t.text_dim).child(name),
                                                )
                                            }),
                                    )
                                    .when(
                                        (!mode.inline() && (cached.video || cached.audio))
                                            || (it.video_stream.is_some() && it.has_subtitles),
                                        |d| {
                                            d.child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(2.))
                                                    .flex_none()
                                                    .when(!mode.inline() && cached.video, |d| {
                                                        d.child(icon(Icon::Video, px(12.), t.text_dim))
                                                    })
                                                    .when(!mode.inline() && cached.audio, |d| {
                                                        d.child(waveform_badge(t.text_dim))
                                                    })
                                                    .when(it.video_stream.is_some() && it.has_subtitles, |d| {
                                                        d.child(subtitle_badge(t.text_dim))
                                                    }),
                                            )
                                        },
                                    )
                                    .child(div().flex_none().text_size(px(11.)).text_color(t.text_dim).child(dur))
                                    .when_some(hover_playhead, |d, f| {
                                        d.child(
                                            div()
                                                .absolute()
                                                .left(relative(f))
                                                .top_0()
                                                .bottom_0()
                                                .w(px(1.))
                                                .bg(rgba(0x409cffff))
                                                .shadow_sm(),
                                        )
                                    }),
                            )
                            .child(div().w(px(22.)).flex_none().when(hovered, |d| {
                                d.child(
                                    this.icon_button(
                                        ("pl-remove", item_id),
                                        Icon::Delete,
                                        "Remove from playlist",
                                        t,
                                        move |this, ev, w, cx| {
                                            if ev.is_right_click() {
                                                return;
                                            }
                                            cx.stop_propagation();
                                            w.focus(&this.playlist_focus);
                                            this.remove_playlist_items(&[item_id], w, cx);
                                        },
                                        cx,
                                    )
                                    .size(px(22.))
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                                )
                            }))
                            .when(dragging && drop_at == Some(i), |d| d.child(marker(true)))
                            .when(dragging && i + 1 == count && drop_at == Some(count), |d| d.child(marker(false)))
                            .when(!filter_active, |d| {
                                d.on_drag(drag_value, move |d, _, _, cx| {
                                    let title = d.title.clone();
                                    cx.new(|_| PlaylistDragPreview { title, theme: t })
                                })
                            })
                            .on_click(cx.listener(move |this, ev: &ClickEvent, w, cx| {
                                if ev.is_right_click() {
                                    return;
                                }
                                w.focus(&this.playlist_focus);
                                let modifiers = ev.modifiers();
                                let control = modifiers.control || modifiers.platform;
                                let shift = modifiers.shift;
                                let Some(index) = this.playlist.items.iter().position(|it| it.id == item_id) else {
                                    return;
                                };
                                let visible = this.visible_playlist_indices(cx);
                                this.playlist_selection.click_visible(
                                    &this.playlist.items,
                                    &visible,
                                    index,
                                    control,
                                    shift,
                                );
                                if !control && !shift && ev.click_count() == 2 {
                                    this.play_index(index, w, cx);
                                }
                                cx.notify();
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
                                    this.open_playlist_item_actions(item_id, ev.position, w, cx);
                                    cx.stop_propagation();
                                }),
                            )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.pl_scroll.clone())
        .flex_1()
        .pl_1()
        .pr(px(super::widgets::SCROLLBAR_W))
        .on_drag_move(cx.listener(|this, ev: &DragMoveEvent<ExternalPaths>, _, cx| {
            if this.playlist_filter_open
                && this.inputs.get("playlist_filter").is_some_and(|input| !input.read(cx).text().trim().is_empty())
            {
                return;
            }
            this.update_playlist_drop(ev.bounds, ev.event.position, cx);
        }))
        .on_drag_move(cx.listener(|this, ev: &DragMoveEvent<PlaylistDrag>, _, cx| {
            if this.playlist_filter_open
                && this.inputs.get("playlist_filter").is_some_and(|input| !input.read(cx).text().trim().is_empty())
            {
                return;
            }
            let drag = ev.drag(cx);
            let at = if drag.ids.len() > 1 && ev.bounds.contains(&ev.event.position) {
                let scroll = this.pl_scroll.0.borrow().base_handle.offset().y;
                let y = f32::from(ev.event.position.y - ev.bounds.top() - scroll) - LIST_PAD;
                this.playlist.group_drop_gap(
                    &drag.ids,
                    drag.anchor,
                    (y / this.playlist_row_height()).floor().max(0.0) as usize,
                )
            } else if drag.ids.len() == 1 {
                this.update_playlist_drop(ev.bounds, ev.event.position, cx);
                return;
            } else {
                None
            };
            if at != this.pl_drop {
                this.pl_drop = at;
                cx.notify();
            }
        }))
        .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
            let filtered = this.playlist_filter_open
                && this.inputs.get("playlist_filter").is_some_and(|input| !input.read(cx).text().trim().is_empty());
            let at = if filtered {
                this.playlist.items.len()
            } else {
                this.pl_drop.take().unwrap_or(this.playlist.items.len())
            };
            this.dropping = false;
            let paths = paths.paths().to_vec();
            this.enqueue_paths_at(at, paths, window, cx);
            cx.stop_propagation();
            cx.notify();
        }))
        .on_drop(cx.listener(|this, d: &PlaylistDrag, _, cx| {
            let Some(at) = this.pl_drop.take() else {
                return;
            };
            if d.ids.len() == 1 && !this.playlist_selection.contains(d.ids[0]) {
                this.playlist_selection.single(d.ids[0]);
            }
            this.drop_playlist_items(&d.ids, at, cx);
        }));

        let body: AnyElement = if count == 0 && total_count == 0 {
            div()
                .id("empty-playlist-drop")
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(t.text_dim)
                .text_size(px(13.))
                .child(icon(Icon::Playlist, px(32.), t.text_disabled))
                .child("The playlist is empty")
                .child(div().text_size(px(12.)).child("Drop files here to add them"))
                .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                    this.dropping = false;
                    this.pl_drop = None;
                    this.enqueue_paths_at(0, paths.paths().to_vec(), window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }))
                .drag_over::<ExternalPaths>(|s, _, _, _| s)
                .into_any_element()
        } else if count == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .text_color(t.text_dim)
                .child("No matching playlist items")
                .into_any_element()
        } else {
            let bar = self.pl_scroll.0.borrow().base_handle.clone();
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(list.py_1())
                .child(super::widgets::scrollbar("playlist-bar", &bar, t))
                .into_any_element()
        };

        let playlist_bounds = self.playlist_bounds.clone();
        div()
            .id("playlist-panel")
            .child(canvas(move |b, _, _| playlist_bounds.set(b), |_, _, _, _| {}).absolute().size_full())
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                this.playlist_previews.pointer = Some(ev.position);
                this.update_playlist_autohide(window, cx);
            }))
            .relative()
            .w(px(width))
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(t.surface)
            .border_l_1()
            .border_color(t.chrome_border)
            .child(collection_header)
            .child(header)
            .when_some(search_row, |d, row| d.child(row))
            .child(
                div()
                    .id("playlist-list-focus")
                    .track_focus(&self.playlist_focus)
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(body)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, w, cx| {
                            w.focus(&this.playlist_focus);
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .id("playlist-resize")
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(5.))
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            let start_x = f32::from(ev.position.x);
                            let bounds = Rc::new(Cell::new(Bounds::new(point(px(0.), px(0.)), viewport)));
                            this.drag = Some(Drag {
                                bounds,
                                vertical: false,
                                last: 0.,
                                on_change: Rc::new(move |this, fraction, _, _| {
                                    this.settings.playlist_width =
                                        (width + start_x - fraction * f32::from(viewport.width)).clamp(120., max_width);
                                }),
                                on_release: Some(Rc::new(|this, _, _, _| this.settings.save())),
                            });
                            cx.stop_propagation();
                        }),
                    ),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.open_playlist_settings(window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, _: &ScrollWheelEvent, _, cx| {
                if this.menu.is_none() && this.playlist_previews.hover.take().is_some() {
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    /// Works out where a drag over the playlist would insert (between rows,
    /// nearest gap to the pointer).
    fn update_playlist_drop(&mut self, bounds: Bounds<Pixels>, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let at = if bounds.contains(&pos) {
            let scroll = self.pl_scroll.0.borrow().base_handle.offset().y;
            let y = f32::from(pos.y - bounds.top() - scroll) - LIST_PAD;
            Some(((y / self.playlist_row_height()).round().max(0.0) as usize).min(self.playlist.items.len()))
        } else {
            None
        };
        if at != self.pl_drop {
            self.pl_drop = at;
            cx.notify();
        }
    }

    pub(super) fn playlist_panel_menu(&self) -> Vec<M> {
        vec![
            M::Header("PLAYLIST".into()),
            M::action("Rename playlist…", |this, w, cx| {
                this.show_dialog(super::dialogs::Dialog::NamePlaylist(true), w, cx)
            }),
            M::action("Delete playlist…", |this, w, cx| {
                this.show_dialog(super::dialogs::Dialog::DeletePlaylist, w, cx)
            })
            .icon(Icon::Delete)
            .enabled(self.playlist_library.can_delete()),
            M::action("Duplicate playlist as…", |this, w, cx| {
                this.show_dialog(super::dialogs::Dialog::DuplicatePlaylist, w, cx)
            })
            .enabled(self.playlist_library.writable),
            M::action("New playlist…", |this, w, cx| {
                this.show_dialog(super::dialogs::Dialog::NamePlaylist(false), w, cx)
            }),
            M::Header("SAVE".into()),
            M::action("Save current playlist state", |this, _, cx| this.save_playlist_workspace(cx))
                .icon(Icon::Save)
                .enabled(self.playlist_library.writable),
            M::Header("CONTENTS".into()),
            M::action("Add files", |this, w, cx| {
                this.prompt_playlist_add(false, w, cx);
            })
            .icon(Icon::Add),
            M::action("Add folder", |this, w, cx| {
                this.prompt_playlist_add(true, w, cx);
            })
            .icon(Icon::OpenFolder),
            M::action("Import playlist from file…", |this, w, cx| this.prompt_playlist_import(w, cx))
                .icon(Icon::OpenFolder),
            M::action("Export playlist (M3U)", |this, w, cx| this.export_playlist(w, cx)).icon(Icon::Export),
            M::action("Clear playlist", |this, w, cx| {
                this.stop(w, cx);
                this.playlist.clear();
                this.playlist_selection.clear();
                this.playlist_hovered = None;
                cx.notify();
            })
            .icon(Icon::Clear),
            M::Header("GENERATE".into()),
            M::action("Generate missing playlist thumbnails", |this, _, cx| {
                this.generate_playlist_representatives(None, cx)
            })
            .enabled(self.playlist.items.iter().any(|it| {
                matches!(it.source, MediaSource::File(_))
                    && (it.video_stream.is_some() || !it.probed || it.duration.is_none())
            })),
            M::action("Generate missing filmstrip thumbnails", |this, _, cx| {
                this.generate_playlist_items(None, true, cx)
            })
            .enabled(
                self.settings.timeline_thumbnails
                    && self.playlist.items.iter().any(|it| {
                        matches!(it.source, MediaSource::File(_))
                            && (it.video_stream.is_some() || !it.probed || it.duration.is_none())
                    }),
            ),
            M::action("Generate missing waveforms", |this, _, cx| this.generate_playlist_waveforms(None, true, cx))
                .enabled(self.settings.timeline_waveform),
            M::action("Regenerate audio waveforms", |this, _, cx| this.generate_playlist_waveforms(None, false, cx))
                .enabled(self.settings.timeline_waveform),
        ]
    }

    /// Hit-test measured rows, including filtered and partially visible rows.
    pub(super) fn playlist_item_at(&self, pos: Point<Pixels>) -> Option<(u64, Pixels)> {
        if !self.playlist_shown() || !self.playlist_bounds.get().contains(&pos) {
            return None;
        }
        let bounds = self.pl_scroll.0.borrow().base_handle.bounds();
        if !bounds.contains(&pos) || pos.x >= bounds.right() - px(super::widgets::SCROLLBAR_W) {
            return None;
        }
        self.playlist_row_bounds.borrow().iter().rev().find_map(|(id, row)| {
            row.contains(&pos).then_some((*id, row.top().max(bounds.top())))
        })
    }

    pub(super) fn open_playlist_item_actions(
        &mut self, item_id: u64, pos: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>,
    ) {
        let Some(index) = self.playlist.items.iter().position(|it| it.id == item_id) else { return };
        let row_top = self.playlist_item_at(pos).filter(|(id, _)| *id == item_id).map_or(pos.y, |(_, y)| y);
        window.focus(&self.playlist_focus);
        if !self.playlist_selection.contains(item_id) {
            self.playlist_selection.single(item_id);
        }
        self.playlist_previews.pointer = Some(pos);
        if self.playlist_previews.hover.as_ref().is_some_and(|h| h.id != item_id) {
            self.playlist_previews.hover = None;
        }
        let items = self.playlist_item_menu(index);
        let width = px(400.).min((window.viewport_size().width - px(8.)).max(px(100.)));
        let x = (self.playlist_bounds.get().left() - width - px(4.)).max(px(4.));
        self.open_menu(items, point(x, row_top), None, cx);
        if let Some(menu) = self.menu.as_mut() {
            menu.playlist_context = true;
            menu.playlist_item = Some(item_id);
            menu.min_width = px(400.);
        }
        self.scrub_playlist_panel_item(pos, cx);
    }

    pub(super) fn scrub_playlist_panel_item(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.menu.as_ref().and_then(|m| m.playlist_item) else { return false };
        if self.playlist_item_at(pos).is_none_or(|(row, _)| row != id) {
            self.clear_playlist_panel_scrub(cx);
            return false;
        }
        let bounds = self.playlist_scrub_bounds.borrow().iter()
            .find(|(row, b)| *row == id && b.contains(&pos)).map(|(_, b)| *b);
        let Some(bounds) = bounds else {
            self.clear_playlist_panel_scrub(cx);
            return false;
        };
        let Some(item) = self.playlist.items.iter().find(|it| it.id == id) else { return false };
        let Some(key) = super::playlist_thumbnails::Key::new(item, &self.settings) else { return false };
        let fraction = frac_in(bounds, pos, false);
        if self.playlist_previews.hover.as_ref().is_some_and(|h| h.id == id && h.fraction == fraction && h.key == key) {
            return true;
        }
        self.playlist_previews.hover = Some(super::playlist_thumbnails::Hover {
            id,
            key,
            duration: item.duration.unwrap_or_default(),
            fraction,
            bounds,
        });
        self.refresh_playlist_hover(cx);
        cx.notify();
        true
    }

    pub(super) fn clear_playlist_panel_scrub(&mut self, cx: &mut Context<Self>) {
        if self.menu.as_ref().and_then(|m| m.playlist_item).is_some() && self.playlist_previews.hover.take().is_some() {
            self.refresh_playlist_hover(cx);
            cx.notify();
        }
    }

    pub(super) fn playlist_panel_scrub_preview(&self, id: u64) -> Option<(super::playlist_thumbnails::Key, super::thumbnails::Preview)> {
        let h = self.playlist_previews.hover.as_ref().filter(|h| h.id == id)?;
        let time = h.duration * f64::from(h.fraction);
        let preview = self.thumbnails.for_playlist(&h.key.path, h.key.stream, h.key.count, time)
            .or_else(|| self.playlist_previews.scrub(&h.key, time))?;
        Some((h.key.clone(), preview.clone()))
    }

    pub(super) fn save_playlist_panel_thumbnail(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some((key, preview)) = self.playlist_panel_scrub_preview(id) {
            self.set_playlist_panel_thumbnail(key, preview, cx);
        }
    }

    fn set_playlist_panel_thumbnail(
        &mut self, key: super::playlist_thumbnails::Key, preview: super::thumbnails::Preview, cx: &mut Context<Self>,
    ) {
        self.settings.playlist_thumbnail_times.insert(key.selection_key(), preview.time);
        self.playlist_previews.remember_selection(key, preview.time, preview);
        self.settings.save();
        cx.notify();
    }

    pub(super) fn playlist_panel_reset_action(&self, id: u64) -> Option<M> {
        let item = self.playlist.items.iter().find(|it| it.id == id)?;
        let key = super::playlist_thumbnails::Key::new(item, &self.settings)?;
        let cached = super::cache_badges::Entry::new(item, &self.settings)
            .is_some_and(|entry| self.cache_badges.get(&entry).video);
        if !cached && self.thumbnails.for_playlist(&key.path, key.stream, key.count, 0.0).is_none()
            && self.playlist_previews.scrub(&key, 0.0).is_none()
        {
            return None;
        }
        let has_override = self.settings.playlist_thumbnail_times.contains_key(&key.selection_key());
        Some(M::action("Reset thumbnail", move |this, _, cx| {
            this.settings.playlist_thumbnail_times.remove(&key.selection_key());
            this.settings.save();
            cx.notify();
        }).icon(Icon::Reset).enabled(has_override))
    }

    fn playlist_item_menu(&self, i: usize) -> Vec<M> {
        let Some(item) = self.playlist.items.get(i) else {
            return Vec::new();
        };
        let item_id = item.id;
        let ids = if self.playlist_selection.contains(item_id) {
            self.playlist_selection.ids(&self.playlist.items)
        } else {
            vec![item_id]
        };
        let count = ids.len();
        let up = ids.clone();
        let down = ids.clone();
        let remove = ids.clone();
        let mut v = vec![
            M::action(if count > 1 { "Play this item" } else { "Play" }, move |this, w, cx| {
                if let Some(index) = this.playlist.items.iter().position(|it| it.id == item_id) {
                    this.play_index(index, w, cx);
                }
            })
            .icon(Icon::Play),
            M::action("Edit title", move |this, w, cx| {
                this.show_dialog(super::dialogs::Dialog::EditPlaylistItemTitle(item_id), w, cx)
            })
            .enabled(count == 1),
            M::Header("ORGANIZE".into()),
            M::action("Move Up", move |this, _, cx| this.move_playlist_items(&up, true, cx))
                .icon(Icon::Up)
                .enabled(self.playlist.can_move_selected(&ids, true)),
            M::action("Move Down", move |this, _, cx| this.move_playlist_items(&down, false, cx))
                .icon(Icon::Down)
                .enabled(self.playlist.can_move_selected(&ids, false)),
            M::action(if count > 1 { format!("Remove {count} items") } else { "Remove".into() }, move |this, w, cx| {
                this.remove_playlist_items(&remove, w, cx)
            })
            .icon(Icon::Remove),
        ];
        let can_representative = self.playlist.items.iter().any(|it| {
            ids.contains(&it.id)
                && matches!(it.source, MediaSource::File(_))
                && (it.video_stream.is_some() || !it.probed || it.duration.is_none())
        });
        let can_generate = self.settings.timeline_thumbnails && can_representative;
        let can_waveform = self.settings.timeline_waveform
            && self.playlist.items.iter().any(|it| {
                ids.contains(&it.id)
                    && matches!(it.source, MediaSource::File(_))
                    && (it.audio_stream.is_some() || !it.probed || it.duration.is_none())
            });
        let waveform_missing = ids.clone();
        let waveform_force = ids.clone();
        let mut waveforms = vec![
            M::action("Generate missing waveforms", move |this, _, cx| {
                this.generate_playlist_waveforms(Some(&waveform_missing), true, cx)
            })
            .enabled(can_waveform),
            M::action(
                if count > 1 { "Regenerate waveforms for selected items" } else { "Regenerate audio waveform" },
                move |this, _, cx| this.generate_playlist_waveforms(Some(&waveform_force), false, cx),
            )
            .enabled(can_waveform),
        ];
        if count > 1 {
            let missing = ids.clone();
            v.push(M::Header("GENERATE".into()));
            let representatives = ids.clone();
            v.push(
                M::action("Generate missing playlist thumbnails for selected items", move |this, _, cx| {
                    this.generate_playlist_representatives(Some(&representatives), cx)
                })
                .enabled(can_representative),
            );
            v.push(
                M::action("Generate missing filmstrip thumbnails for selected items", move |this, _, cx| {
                    this.generate_playlist_items(Some(&missing), true, cx)
                })
                .enabled(can_generate),
            );
            v.push(
                M::action("Regenerate filmstrip thumbnails for selected items", move |this, _, cx| {
                    this.generate_playlist_items(Some(&ids), false, cx)
                })
                .enabled(can_generate),
            );
            v.append(&mut waveforms);
            return v;
        }
        if let MediaSource::File(p) = &item.source {
            v.push(M::Header("SOURCE".into()));
            let p = p.clone();
            v.push(M::action("Show in File Manager", move |_, _, cx| cx.reveal_path(&p)).icon(Icon::OpenFolder));
            v.push(M::Header("GENERATE".into()));
            let missing = ids.clone();
            let representatives = ids.clone();
            v.push(
                M::action("Generate playlist thumbnail", move |this, _, cx| {
                    this.generate_playlist_representatives(Some(&representatives), cx)
                })
                .enabled(can_representative),
            );
            v.push(
                M::action("Generate missing filmstrip thumbnails", move |this, _, cx| {
                    this.generate_playlist_items(Some(&missing), true, cx)
                })
                .enabled(can_generate),
            );
            v.push(
                M::action("Regenerate filmstrip thumbnails", move |this, _, cx| {
                    this.generate_playlist_items(Some(&ids), false, cx)
                })
                .enabled(can_generate),
            );
            v.append(&mut waveforms);
        }
        v
    }
}

/// The window's root: the picture, with everything else on top of it as a
/// cached view (see `picture`). A new video frame re-renders only this and
/// the picture.
impl Render for PlayerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.rendered {
            self.rendered = true;
            log::debug!("startup: first render at {:.0} ms", crate::ms_since_start());
        }
        self.ui_stats.1 += 1;
        self.avoid_direct_scanout(window);
        self.retire_textures(window);
        self.track_fullscreen(window);
        let (frame, size, pad, aspect, zoom) =
            (self.frame.clone(), self.frame_size, self.frame_pad, self.aspect, self.zoom);
        self.picture.update(cx, |p, _| {
            p.frame = frame;
            p.size = size;
            p.pad = pad;
            p.aspect = aspect;
            p.zoom = zoom;
        });
        let chrome = match &self.chrome {
            Some(c) => c.clone(),
            None => {
                let me = cx.entity();
                let c = cx.new(|cx| {
                    // Everything the chrome shows comes from the player view.
                    cx.observe(&me, |_, _, cx| cx.notify()).detach();
                    Chrome { player: me.downgrade() }
                });
                self.chrome = Some(c.clone());
                c
            }
        };
        div()
            .size_full()
            .relative()
            .bg(self.theme.chrome)
            .child(self.picture.clone())
            .child(AnyView::from(chrome).cached(StyleRefinement::default().size_full()))
    }
}

impl PlayerView {
    /// Everything in the window but the picture (see [`Chrome`]).
    pub fn render_chrome(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.age_sub_images();
        let t = self.theme;
        let fullscreen = window.is_fullscreen();
        let client_decor = matches!(window.window_decorations(), Decorations::Client { .. }) && !fullscreen;
        let show_menubar = self.menubar_visible(window);
        if self.menu.is_none()
            && self.dialog.is_none()
            && self.inputs.get("playlist_switch").is_some_and(|input| input.focus_handle(cx).is_focused(window))
        {
            window.focus(&self.playlist_focus);
        }
        let overlay_open = self.dialog.is_some() || self.menu.is_some() || self.editing_volume;
        let playlist_filter_focused = self.playlist_shown()
            && self.playlist_filter_open
            && self.inputs.get("playlist_filter").is_some_and(|input| input.focus_handle(cx).is_focused(window));
        let playlist_focused = self.playlist_shown() && self.playlist_focus.is_focused(window);
        if !self.playlist_shown()
            && (self.playlist_focus.is_focused(window)
                || self.inputs.get("playlist_filter").is_some_and(|input| input.focus_handle(cx).is_focused(window)))
        {
            window.focus(&self.focus);
        }

        let autohide = self.playlist_autohide();
        let main = div().relative().flex_1().min_h_0().flex().child(self.video_area(window, cx)).when(
            self.playlist_shown(),
            |d| {
                let reserve = if fullscreen && self.controls_visible() {
                    FULLSCREEN_CONTROL_BAR_H
                        + if self.player.with_state(|st| self.thumbnails.has_previews(st, &self.settings, true)) {
                            self.settings.visualization_height(true) as f32 + 2.
                        } else {
                            0.
                        }
                } else {
                    0.
                };
                d.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .pb(px(reserve))
                        .when(autohide, |d| d.absolute().right_0().top_0().bottom_0().occlude())
                        .child(self.playlist_panel(window, cx)),
                )
            },
        );

        let mut root = div()
            .id("root")
            .key_context(if overlay_open {
                if self.menu.as_ref().is_some_and(|menu| menu.playlist_switch.is_some()) {
                    "Overlay SearchCombo"
                } else if self.dialog == Some(super::dialogs::Dialog::MediaInfo) && self.menu.is_none() {
                    "Overlay MediaInfo"
                } else {
                    "Overlay"
                }
            } else if playlist_filter_focused {
                // Text input must not inherit playback or DVD shortcuts.
                "EditingText"
            } else if playlist_focused {
                "Player Playlist PlaylistVisible"
            } else if self.dvd_buttons() {
                // Both: the player's shortcuts, with the disc's buttons on top.
                if self.playlist_shown() { "Player DvdMenu PlaylistVisible" } else { "Player DvdMenu" }
            } else if self.playlist_shown() {
                "Player PlaylistVisible"
            } else {
                CTX
            })
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .text_color(t.text)
            .text_size(px(13.))
            .font_family(".SystemUIFont")
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                this.playlist_previews.pointer = Some(ev.position);
                this.update_playlist_autohide(window, cx);
                if this.menu.is_none()
                    && this.playlist_previews.hover.as_ref().is_some_and(|h| !h.bounds.contains(&ev.position))
                {
                    this.playlist_previews.hover = None;
                    this.refresh_playlist_hover(cx);
                    cx.notify();
                }
                this.handle_drag_move(ev, window, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.window_move_origin = None;
                    this.end_drag(window, cx);
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.dropping = false;
                let p = paths.paths().to_vec();
                this.open_paths(p, window, cx);
            }))
            .drag_over::<ExternalPaths>(|s, _, _, _| s)
            .on_hover(cx.listener(|this, h: &bool, window, cx| {
                if !*h {
                    // When the pointer leaves the window over the fullscreen
                    // control bar, its hover-exit event may not be delivered.
                    // Clear the stale state so the autohide timer can work.
                    // An occluding playlist overlay can also cause root hover exit.
                    if this.playlist_autohide()
                        && window.is_window_hovered()
                        && this.playlist_shown()
                        && this.playlist_bounds.get().contains(&window.mouse_position())
                    {
                        return;
                    }
                    this.playlist_previews.pointer = None;
                    this.update_playlist_autohide(window, cx);
                    let item_panel = this.menu.as_ref().and_then(|m| m.playlist_item).is_some();
                    let had_playlist_hover = (this.menu.is_none() || item_panel && !window.is_window_hovered())
                        && this.playlist_previews.hover.take().is_some();
                    if item_panel && had_playlist_hover {
                        this.refresh_playlist_hover(cx);
                    }
                    let had_hover = this.pointer_over_controls || had_playlist_hover;
                    let was_dropping = this.dropping;
                    this.pointer_over_controls = false;
                    this.dropping = false;
                    if had_hover || was_dropping {
                        cx.notify();
                    }
                }
            }));
        root = self.register_actions(root, cx);
        root = root
            .when(client_decor, |d| d.child(self.titlebar(window, cx)))
            .when(show_menubar, |d| d.child(self.menubar(cx)))
            .child(main)
            .when(!fullscreen, |d| d.child(self.control_bar(t, false, window, cx)));
        // In fullscreen the controls float over the picture.
        if fullscreen && self.controls_visible() {
            let ot = overlay_theme(&self.theme);
            root = root.child(
                div()
                    .id("overlay-controls")
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .pt(px(40.))
                    .bg(gpui::linear_gradient(
                        0.,
                        gpui::linear_color_stop(rgba(0x000000d0), 0.),
                        gpui::linear_color_stop(rgba(0x00000000), 1.),
                    ))
                    .on_hover(cx.listener(|this, h: &bool, _, cx| {
                        this.pointer_over_controls = *h;
                        cx.notify();
                    }))
                    // Clicks, right-clicks and the wheel over the controls
                    // are theirs, not the video's.
                    .occlude()
                    .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                        this.playlist_previews.pointer = Some(ev.position);
                        this.update_playlist_autohide(window, cx);
                        if this.handle_drag_move(ev, window, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if this.drag.is_some() {
                                this.end_drag(window, cx);
                                cx.stop_propagation();
                            }
                        }),
                    )
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(self.control_bar(ot, true, window, cx)),
            );
        }
        if client_decor && !window.is_maximized() {
            root = root.child(self.resize_handles());
        }
        if self.playlist_shown()
            && self.dialog.is_none()
            && self.menu.as_ref().is_none_or(|m| !m.playlist_context)
            && !self.editing_volume
            && !cx.has_active_drag()
            && super::playlist_thumbnails::PlaylistPreviews::mode(&self.settings).side()
            && let Some(h) = &self.playlist_previews.hover
            && self.playlist.items.iter().any(|it| it.id == h.id)
        {
            let time = h.duration * f64::from(h.fraction);
            if let Some(preview) = self
                .thumbnails
                .for_playlist(&h.key.path, h.key.stream, h.key.count, time)
                .or_else(|| self.playlist_previews.scrub(&h.key, time))
            {
                let label = format_time(preview.time);
                let preview = preview.clone();
                let row_top = h.bounds.top();
                let video_bounds = self.video_bounds.clone();
                let control_bounds = self.control_bounds.clone();
                let fullscreen_controls = fullscreen && self.controls_visible();
                let playlist_overlay_width = if autohide {
                    px(self
                        .settings
                        .playlist_width
                        .clamp(120., (f32::from(window.viewport_size().width) - 120.).max(120.)))
                } else {
                    px(0.)
                };
                // Prepaint after the normal layout so the limit includes the actual
                // visualization height, including adaptive sizing and fullscreen.
                root = root.child(
                    deferred(
                        canvas(
                            move |area, window, cx| {
                                let video = video_bounds.get();
                                let bottom =
                                    if fullscreen_controls { control_bounds.get().top() } else { video.bottom() };
                                let top = video.top().max(area.top()) + px(4.);
                                let right = video.right() - playlist_overlay_width - px(6.);
                                let width = px(246.).min(right - area.left() - px(4.));
                                let height = px(159.).min(bottom - top - px(4.));
                                if width < px(60.) || height < px(40.) {
                                    return None;
                                }
                                let y = row_top.clamp(top, bottom - height - px(4.));
                                let mut element = div()
                                    .w(width)
                                    .h(height)
                                    .flex()
                                    .flex_col()
                                    .p(px(3.))
                                    .rounded(px(5.))
                                    .bg(Hsla { a: 1.0, ..t.popup })
                                    .shadow_md()
                                    .child(preview.element(f32::from(width - px(6.)), f32::from(height - px(24.))))
                                    .child(
                                        div()
                                            .h(px(18.))
                                            .text_center()
                                            .text_size(px(12.))
                                            .text_color(t.text)
                                            .child(label.clone()),
                                    )
                                    .into_any_element();
                                element.prepaint_as_root(
                                    point(right - width, y),
                                    size(gpui::AvailableSpace::Definite(width), gpui::AvailableSpace::Definite(height)),
                                    window,
                                    cx,
                                );
                                Some(element)
                            },
                            |_, element, window, cx| {
                                if let Some(mut element) = element {
                                    element.paint(window, cx);
                                }
                            },
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .with_priority(5),
                );
            }
        }
        if let Some(m) = self.render_menu(window, cx) {
            root = root.child(m);
        }
        if let Some(d) = self.render_dialog(window, cx) {
            root = root.child(d);
        }
        root.into_any_element()
    }
}

#[allow(dead_code)]
fn _assert_types(_: Hsla) {}
