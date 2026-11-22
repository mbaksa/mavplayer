//! Rendering of the main window.

#[path = "outline_vector.rs"]
mod outline_vector;
#[path = "subtitle_style.rs"]
mod subtitle_style;

use subtitle_style::{FONT_FAMILY, FONT_WEIGHT};

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
use crate::settings::{BarControl, RepeatMode, ScrollAction, SubStyle};
use gpui::{
    AnyElement, AnyView, Bounds, ClickEvent, Context, Corners, CursorStyle, Decorations, Div, DragMoveEvent, ElementId,
    ExternalPaths, Hsla, ImageSource, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, RenderImage,
    ResizeEdge, Resource, ScrollDelta, ScrollWheelEvent, SharedString, Stateful, StyleRefinement, Window,
    WindowControlArea, canvas, div, img, point, prelude::*, px, relative, rgba, size,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TITLEBAR_H: f32 = 34.0;
const RESIZE_EDGE: f32 = 6.0;
/// Playlist row height and the list's vertical padding (`py_1`).
const ROW_H: f32 = 34.0;
const LIST_PAD: f32 = 4.0;

/// A playlist row being dragged to a new position.
pub struct PlaylistDrag {
    pub index: usize,
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

    fn handle_drag_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
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

    fn end_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = self.drag.take() {
            if let Some(r) = d.on_release {
                r(self, d.last, window, cx);
            }
            cx.notify();
        }
    }

    // ───────────────────────── window chrome ─────────────────────────

    fn titlebar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let controls = window.window_controls();
        let title: SharedString = self
            .player
            .with_state(|s| s.media.as_ref().map(|m| m.title.clone().unwrap_or_else(|| m.source_name.clone())))
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
                        this.save_state();
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

        let area = self.video_bounds.get();
        let rect = video_rect(area, self.frame_size, aspect, zoom);
        let local = |p: Point<Pixels>| point(p.x - area.origin.x, p.y - area.origin.y);
        let sub_rect = if self.frame.is_some() { rect } else { area };

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
                if this.settings.drag_video_to_move_window {
                    if let Some(start) = this.window_move_origin {
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
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    let items = this.context_menu(cx);
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

        // Text subtitles.
        if !texts.is_empty() {
            root = root.child(self.text_subtitles(&texts, sub_rect, area, window.scale_factor()));
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

        // In fullscreen the controls float over the picture.
        if fullscreen && show_controls {
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
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(self.control_bar(ot, true, window, cx)),
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
        let _ = local;
        root.into_any_element()
    }

    fn text_subtitles(
        &mut self,
        texts: &[String],
        rect: Bounds<Pixels>,
        area: Bounds<Pixels>,
        scale_factor: f32,
    ) -> AnyElement {
        let h = f32::from(rect.size.height).max(100.0);
        let font = subtitle_style::font_size(h, self.settings.sub_scale);
        let margin = f32::from(area.size.height)
            - (f32::from(rect.origin.y - area.origin.y) + f32::from(rect.size.height))
            + h * self.settings.sub_margin;
        let left = f32::from(rect.origin.x - area.origin.x);
        let width = f32::from(rect.size.width);
        let lines: Vec<String> = texts
            .iter()
            .flat_map(|t| t.lines().map(|l| l.to_string()).collect::<Vec<_>>())
            .filter(|l| !l.trim().is_empty())
            .collect();
        let style = self.settings.sub_style;
        let row_pad = font * subtitle_style::BOX_VERTICAL_PADDING_FACTOR;
        let mut col = div()
            .absolute()
            .left(px(left + width * 0.05))
            .w(px(width * 0.9))
            .bottom(px(margin.max(4.0)))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(font * subtitle_style::LINE_GAP_FACTOR))
            .text_size(px(font))
            .line_height(px(subtitle_style::line_height(font)))
            .font_family(FONT_FAMILY)
            .font_weight(FONT_WEIGHT);
        for line in lines {
            let line: SharedString = line.into();
            let el: AnyElement = match style {
                SubStyle::Box => div()
                    .px(px(font * subtitle_style::BOX_HORIZONTAL_PADDING_FACTOR))
                    .py(px(font * subtitle_style::BOX_VERTICAL_PADDING_FACTOR))
                    .bg(rgba(0x000000b0))
                    .rounded(px(4.))
                    .font_family(FONT_FAMILY)
                    .font_weight(FONT_WEIGHT)
                    .text_color(gpui::white())
                    .text_center()
                    .child(line)
                    .into_any_element(),
                SubStyle::Outline => {
                    // mavplayer-hybrid-outline: Vello paints only black stroke;
                    // native GPUI draws the white text on top at full sharpness.
                    let line_text = line.to_string();
                    let max_w = width * 0.9;
                    let dpi = scale_factor;
                    let key = outline_vector::cache_key(&line_text, font, max_w, dpi);
                    let outline_image = if let Some((image, age)) = self.sub_images.get_mut(&key) {
                        *age = 0;
                        Some(image.clone())
                    } else {
                        outline_vector::stroke_image(&line_text, font, max_w, dpi).map(|image| {
                            self.sub_images.insert(key, (image.clone(), 0));
                            image
                        })
                    };
                    let line_h = subtitle_style::line_height(font);
                    // Match the transparent overscan in outline_vector.rs.
                    let outline_pad = subtitle_style::outline_padding(font);

                    // Match Box's line-height + top/bottom padding, so
                    // bottom-aligned subtitles retain exactly the same baseline.
                    let mut d = div()
                        .relative()
                        .w_full()
                        .h(px(line_h + 2.0 * row_pad))
                        .font_family(FONT_FAMILY)
                        .font_weight(FONT_WEIGHT)
                        .text_center();
                    if let Some(outline_image) = outline_image {
                        d = d.child(
                            canvas(
                                |bounds, _, _| bounds,
                                move |bounds, _, window, _| {
                                    let _ =
                                        window.paint_image(bounds, Corners::default(), outline_image.clone(), 0, false);
                                },
                            )
                            .absolute()
                            .top(px(row_pad - outline_pad))
                            .h(px(line_h + 2.0 * outline_pad))
                            .w_full(),
                        );
                    }
                    d.child(
                        div()
                            .absolute()
                            .top(px(row_pad))
                            .w_full()
                            .font_family(FONT_FAMILY)
                            .font_weight(FONT_WEIGHT)
                            .text_color(gpui::white())
                            .child(line),
                    )
                    .into_any_element()
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

    fn seek_bar(&self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let dur = self.player.duration();
        let pos = self.player.position();
        let st = self.player.state();
        let seekable = st.media.as_ref().map(|m| m.seekable && m.duration > 0.0).unwrap_or(false);
        let shown = self.shown_position();
        let frac = if dur > 0.0 { (shown / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let cache = if dur > 0.0 { ((pos + st.cache_secs) / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let (b1, b2, b3) = (bounds.clone(), bounds.clone(), bounds.clone());
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
        div()
            .id("seek")
            .group("seek")
            .h(px(18.))
            .flex_1()
            .relative()
            .flex()
            .items_center()
            .when(seekable, |d| d.cursor_pointer())
            .child(canvas(move |b, _, _| b1.set(b), |_, _, _, _| {}).absolute().size_full())
            .child(track)
            .child(
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
            )
            .when_some(hover_label, |d, (f, label)| {
                d.child(
                    div().absolute().bottom(px(22.)).left(relative(f)).child(
                        div()
                            .ml(px(-40.))
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
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                if this.scrub.is_some() {
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
                        let _ = window;
                        cx.stop_propagation();
                    }),
                )
            })
            .into_any_element()
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
        let title = st
            .media
            .as_ref()
            .map(|m| m.title.clone().unwrap_or_else(|| m.source_name.clone()))
            .or_else(|| st.source.as_ref().map(|s| s.display_name()))
            .unwrap_or_default();
        let dim = t.text_dim;
        let time = |s: String| div().text_size(px(12.)).text_color(dim).min_w(px(52.)).flex().justify_center().child(s);

        let seek_row = div().flex().items_center().gap_2().child(time(time_left)).child(self.seek_bar(t, cx)).child(
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
                    .when(self.playlist_shown(), |d| d.bg(t.accent_soft))
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

        div()
            .id(if overlay { "controls-overlay" } else { "controls" })
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
            .into_any_element()
    }

    // ───────────────────────── playlist panel ─────────────────────────

    fn playlist_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let count = self.playlist.items.len();
        let total: f64 = self.playlist.items.iter().filter_map(|i| i.duration).sum();
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
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(div().text_size(px(13.)).font_weight(gpui::FontWeight::SEMIBOLD).child("Playlist"))
                    .child(div().text_size(px(11.)).text_color(t.text_dim).child(format!(
                        "{count} item{}{}",
                        if count == 1 { "" } else { "s" },
                        if total > 0.0 { format!(" · {}", format_time(total)) } else { String::new() }
                    ))),
            )
            .child(self.icon_button(
                "pl-add",
                Icon::Add,
                "Add files",
                t,
                |this, _, w, cx| {
                    let mode = this.settings.open_mode;
                    this.settings.open_mode = crate::settings::OpenMode::Append;
                    this.prompt_open(false, false, w, cx);
                    this.settings.open_mode = mode;
                },
                cx,
            ))
            .child(self.icon_button(
                "pl-folder",
                Icon::OpenFolder,
                "Add folder",
                t,
                |this, _, w, cx| {
                    let mode = this.settings.open_mode;
                    this.settings.open_mode = crate::settings::OpenMode::Append;
                    this.prompt_open(true, false, w, cx);
                    this.settings.open_mode = mode;
                },
                cx,
            ))
            .child(self.icon_button(
                "pl-save",
                Icon::Save,
                "Save playlist",
                t,
                |this, _, w, cx| this.save_playlist(w, cx),
                cx,
            ))
            .child(self.icon_button(
                "pl-clear",
                Icon::Clear,
                "Clear playlist",
                t,
                |this, _, w, cx| {
                    this.stop(w, cx);
                    this.playlist.clear();
                    cx.notify();
                },
                cx,
            ));

        let current = self.playlist.current;
        let selected = self.playlist_selected;
        let drop_at = self.pl_drop;
        let list = gpui::uniform_list(
            "playlist-items",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                let t = this.theme;
                range
                    .map(|i| {
                        let it = &this.playlist.items[i];
                        let is_cur = current == Some(i);
                        let title = it.display_title();
                        let dur = it.duration.map(format_time).unwrap_or_default();
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
                        let drag_value = PlaylistDrag { index: i, title: title.clone().into() };
                        div()
                            .id(("pl-row", i))
                            .relative()
                            .h(px(ROW_H))
                            .w_full()
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .when(selected == Some(i) && !is_cur, |d| d.bg(t.hover))
                            .when(is_cur, |d| d.bg(t.accent_soft))
                            .hover(|s| s.bg(t.hover))
                            .child(
                                div()
                                    .w(px(22.))
                                    .flex()
                                    .justify_center()
                                    .text_size(px(11.))
                                    .text_color(t.text_dim)
                                    .child(if is_cur {
                                        icon(Icon::Play, px(12.), t.accent).into_any_element()
                                    } else {
                                        div().child(format!("{}", i + 1)).into_any_element()
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.))
                                    .text_color(if it.failed {
                                        t.text_disabled
                                    } else if is_cur {
                                        t.accent
                                    } else {
                                        t.text
                                    })
                                    .child(title),
                            )
                            .child(div().text_size(px(11.)).text_color(t.text_dim).child(dur))
                            .when(dragging && drop_at == Some(i), |d| d.child(marker(true)))
                            .when(dragging && i + 1 == count && drop_at == Some(count), |d| d.child(marker(false)))
                            .on_drag(drag_value, move |d, _, _, cx| {
                                let title = d.title.clone();
                                cx.new(|_| PlaylistDragPreview { title, theme: t })
                            })
                            .on_click(cx.listener(move |this, ev: &ClickEvent, w, cx| {
                                this.playlist_selected = Some(i);
                                if ev.click_count() >= 2 {
                                    this.play_index(i, w, cx);
                                }
                                cx.notify();
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                    this.playlist_selected = Some(i);
                                    let items = this.playlist_item_menu(i);
                                    this.open_menu(items, ev.position, None, cx);
                                    cx.stop_propagation();
                                }),
                            )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.pl_scroll.clone())
        .flex_1()
        .px_1()
        .on_drag_move(cx.listener(|this, ev: &DragMoveEvent<ExternalPaths>, _, cx| {
            this.update_playlist_drop(ev.bounds, ev.event.position, cx);
        }))
        .on_drag_move(cx.listener(|this, ev: &DragMoveEvent<PlaylistDrag>, _, cx| {
            this.update_playlist_drop(ev.bounds, ev.event.position, cx);
        }))
        .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
            let at = this.pl_drop.take().unwrap_or(this.playlist.items.len());
            this.dropping = false;
            let paths = paths.paths().to_vec();
            if let Some(d) = paths.first().and_then(|p| p.parent()) {
                this.settings.last_dir = Some(d.to_path_buf());
            }
            let sources = crate::playlist::expand_paths(&paths);
            this.enqueue_at(at, sources, window, cx);
            cx.notify();
        }))
        .on_drop(cx.listener(|this, d: &PlaylistDrag, _, cx| {
            let at = this.pl_drop.take().unwrap_or(this.playlist.items.len());
            // `at` is an insertion point; once the item is taken out, the
            // points after it shift up by one.
            let to = if at > d.index { at - 1 } else { at }.min(this.playlist.items.len().saturating_sub(1));
            this.playlist.move_item(d.index, to);
            this.playlist_selected = Some(to);
            cx.notify();
        }));

        let body: AnyElement = if count == 0 {
            div()
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

        div()
            .id("playlist-panel")
            .w(px(320.))
            .flex_none()
            .flex()
            .flex_col()
            .bg(t.surface)
            .border_l_1()
            .border_color(t.chrome_border)
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// Works out where a drag over the playlist would insert (between rows,
    /// nearest gap to the pointer).
    fn update_playlist_drop(&mut self, bounds: Bounds<Pixels>, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let at = if bounds.contains(&pos) {
            let scroll = self.pl_scroll.0.borrow().base_handle.offset().y;
            let y = f32::from(pos.y - bounds.top() - scroll) - LIST_PAD;
            Some(((y / ROW_H).round().max(0.0) as usize).min(self.playlist.items.len()))
        } else {
            None
        };
        if at != self.pl_drop {
            self.pl_drop = at;
            cx.notify();
        }
    }

    fn playlist_item_menu(&self, i: usize) -> Vec<M> {
        let n = self.playlist.items.len();
        let mut v = vec![
            M::action("Play", move |this, w, cx| this.play_index(i, w, cx)).icon(Icon::Play),
            M::action("Move Up", move |this, _, _| {
                this.playlist.move_item(i, i.saturating_sub(1));
                this.playlist_selected = Some(i.saturating_sub(1));
            })
            .icon(Icon::Up)
            .enabled(i > 0),
            M::action("Move Down", move |this, _, _| {
                this.playlist.move_item(i, (i + 1).min(n - 1));
                this.playlist_selected = Some((i + 1).min(n - 1));
            })
            .icon(Icon::Down)
            .enabled(i + 1 < n),
            M::action("Remove", move |this, w, cx| {
                if this.playlist.current == Some(i) {
                    this.stop(w, cx);
                }
                this.playlist.remove(i);
            })
            .icon(Icon::Remove),
        ];
        if let Some(MediaSource::File(p)) = self.playlist.items.get(i).map(|it| it.source.clone()) {
            v.push(M::Separator);
            v.push(M::action("Show in File Manager", move |_, _, cx| cx.reveal_path(&p)).icon(Icon::OpenFolder));
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
        let show_menubar = self.settings.show_menubar && !fullscreen && !cfg!(target_os = "macos");
        let overlay_open = self.dialog.is_some() || self.menu.is_some() || self.editing_volume;

        let main = div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(self.video_area(window, cx))
            .when(self.playlist_shown(), |d| d.child(self.playlist_panel(cx)));

        let mut root = div()
            .id("root")
            .key_context(if overlay_open {
                "Overlay"
            } else if self.dvd_buttons() {
                // Both: the player's shortcuts, with the disc's buttons on top.
                "Player DvdMenu"
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
            .on_hover(cx.listener(|this, h: &bool, _, cx| {
                if !*h {
                    // When the pointer leaves the window over the fullscreen
                    // control bar, its hover-exit event may not be delivered.
                    // Clear the stale state so the autohide timer can work.
                    let had_hover = this.pointer_over_controls;
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
        if client_decor && !window.is_maximized() {
            root = root.child(self.resize_handles());
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
