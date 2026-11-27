//! Modal dialogs: preferences, effects, media info, open URL / disc, etc.

use super::keymap::{self, Command};
use super::menu::MenuItem as M;
use super::player::{ModalDragOrigin, PlayerView};
use super::widgets::{SCROLLBAR_W, format_size, format_time, icon, parse_time, scrollbar};
use crate::engine::{
    self, Anime4KMode, Anime4KQuality, AudioFx, ChannelMode, Deinterlace, EQ_BANDS, HwDecode, ScaleQuality, TrackKind,
    Upscaler, VideoAdjust, disc,
};
use crate::icons::Icon;
use crate::playlist;
use crate::settings::{
    BUNDLED_SUB_FONT_FAMILY, BarControl, DEFAULT_SUB_FONT_FAMILY, IconTheme, OpenMode, RunningMode, ScrollAction,
    SeekDragMode, SubPosition, SubStyle, ThemeMode,
};
use gpui::{
    AnyElement, ClickEvent, Context, Focusable, ImageSource, MouseButton, MouseDownEvent, MouseMoveEvent, Resource,
    SharedString, Window, canvas, deferred, div, img, prelude::*, px, relative,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dialog {
    Settings(usize),
    Effects(usize),
    MediaInfo,
    OpenUrl,
    NamePlaylist(bool),
    NewCollection,
    EditPlaylistItemTitle(u64),
    DuplicatePlaylist,
    DeletePlaylist,
    OpenDisc,
    JumpTo,
    About,
    SubtitleFontLicense,
    Integrate,
}

const SETTINGS_TABS: &[&str] = &[
    "Interface",
    "Thumbnails",
    "Playlist",
    "Playback",
    "Video",
    "Audio",
    "Subtitles",
    "Keyboard Shortcuts",
    "Network & Discs",
    "Advanced",
];
/// The Preferences category that lists (and edits) every keyboard shortcut.
pub const SHORTCUTS_TAB: usize = 7;
const EFFECT_TABS: &[&str] = &["Equalizer", "Audio Effects", "Picture", "Synchronization"];

pub const EQ_PRESETS: &[(&str, [f32; 10])] = &[
    ("Flat", [0.0; 10]),
    ("Bass Boost", [7.0, 6.0, 5.0, 3.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Treble Boost", [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 3.0, 5.0, 6.0, 7.0]),
    ("Rock", [5.0, 4.0, 3.0, 1.0, -1.0, -1.0, 1.0, 3.0, 4.0, 5.0]),
    ("Pop", [-1.0, 1.0, 3.0, 4.0, 3.0, 1.0, -1.0, -1.0, -1.0, -1.0]),
    ("Jazz", [3.0, 2.0, 1.0, 2.0, -1.0, -1.0, 0.0, 1.0, 2.0, 3.0]),
    ("Classical", [4.0, 3.0, 2.0, 1.0, -1.0, -1.0, 0.0, 2.0, 3.0, 4.0]),
    ("Dance", [6.0, 5.0, 2.0, 0.0, 0.0, -2.0, -3.0, -3.0, 0.0, 0.0]),
    ("Vocal", [-2.0, -3.0, -3.0, 1.0, 4.0, 4.0, 3.0, 1.0, 0.0, -2.0]),
    ("Full Bass & Treble", [6.0, 5.0, 0.0, -4.0, -3.0, 1.0, 5.0, 7.0, 8.0, 8.0]),
    ("Headphones", [4.0, 8.0, 4.0, -2.0, -1.0, 1.0, 4.0, 7.0, 9.0, 10.0]),
    ("Large Hall", [8.0, 8.0, 4.0, 4.0, 0.0, -3.0, -3.0, -3.0, 0.0, 0.0]),
    ("Soft", [3.0, 1.0, 0.0, -1.0, 0.0, 2.0, 5.0, 6.0, 7.0, 8.0]),
    ("Live", [-4.0, 0.0, 3.0, 4.0, 4.0, 4.0, 3.0, 2.0, 2.0, 2.0]),
];

impl PlayerView {
    pub fn show_dialog(&mut self, d: Dialog, window: &mut Window, cx: &mut Context<Self>) {
        // Preserve the position when switching Preferences tabs.
        if self.dialog.is_none() {
            self.modal_offset = (0.0, 0.0);
        }
        self.modal_drag_origin = None;
        self.menu = None;
        self.dialog = Some(d);
        // Every dialog (and settings tab) opens scrolled to the top.
        self.modal_scroll.set_offset(gpui::Point::default());
        self.options_scroll.set_offset(gpui::Point::default());
        match d {
            Dialog::OpenUrl => {
                let input = self.input("url", "https://… or a file path", window, cx);
                let clip =
                    cx.read_from_clipboard().and_then(|c| c.text()).filter(|t| t.contains("://") && !t.contains('\n'));
                input.update(cx, |i, cx| i.set_text(clip.unwrap_or_default(), cx));
                window.focus(&input.focus_handle(cx));
            }
            Dialog::EditPlaylistItemTitle(id) => {
                let input = self.input("playlist_item_title", "Leave empty to use the media title", window, cx);
                let title = self
                    .playlist
                    .items
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.display_title())
                    .unwrap_or_default();
                input.update(cx, |i, cx| i.set_text(title, cx));
                window.focus(&input.focus_handle(cx));
            }
            Dialog::NewCollection => {
                let input = self.input("collection_name", "Collection name", window, cx);
                input.update(cx, |i, cx| i.set_text("", cx));
                window.focus(&input.focus_handle(cx));
            }
            Dialog::NamePlaylist(rename) => {
                let input = self.input("playlist_name", "Playlist name", window, cx);
                let name = if rename {
                    self.playlist_library.name().to_owned()
                } else {
                    self.playlist_library.suggested_name("Playlist")
                };
                input.update(cx, |i, cx| i.set_text(name, cx));
                window.focus(&input.focus_handle(cx));
            }
            Dialog::DuplicatePlaylist => {
                let input = self.input("playlist_name", "Playlist name", window, cx);
                let name = self.playlist_library.suggested_name(&format!("{} copy", self.playlist_library.name()));
                input.update(cx, |i, cx| i.set_text(name, cx));
                window.focus(&input.focus_handle(cx));
            }
            Dialog::JumpTo => {
                let input = self.input("jump", "h:mm:ss, mm:ss, seconds or 1h2m3s", window, cx);
                let now = format_time(self.player.position());
                input.update(cx, |i, cx| i.set_text(now, cx));
                window.focus(&input.focus_handle(cx));
            }
            Dialog::Settings(tab) => {
                for (k, v, ph) in [
                    ("audio_langs", self.settings.audio_langs.clone(), "e.g. en, ja"),
                    ("sub_langs", self.settings.sub_langs.clone(), "e.g. en, fr"),
                    ("sub_charset", self.settings.sub_charset.clone(), "auto (e.g. CP1251, Shift_JIS)"),
                    ("sub_font_family", self.settings.sub_font_family.clone(), DEFAULT_SUB_FONT_FAMILY),
                ] {
                    let i = self.input(k, ph, window, cx);
                    i.update(cx, |i, cx| i.set_text(v, cx));
                }
                self.refresh_audio_devices(cx);
                self.capturing_key = None;
                if tab == SHORTCUTS_TAB {
                    let input = self.input("shortcut_search", "Search commands or keys", window, cx);
                    input.update(cx, |i, cx| i.set_text(String::new(), cx));
                    self.shortcut_note = None;
                    window.focus(&input.focus_handle(cx));
                } else {
                    window.focus(&self.focus);
                }
            }
            Dialog::OpenDisc => {
                // Read the first drive (again: the disc may have changed).
                if let Some(d) = disc::optical_drives().first() {
                    self.scan_disc(d.path.clone(), cx);
                }
                window.focus(&self.focus);
            }
            _ => window.focus(&self.focus),
        }
        cx.notify();
    }

    pub fn refresh_audio_devices(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async { engine::audio_devices() });
        cx.spawn(async move |this, cx| {
            let devs = task.await;
            let _ = this.update(cx, |v, cx| {
                v.audio_devices = devs;
                cx.notify();
            });
        })
        .detach();
    }

    fn scan_disc(&mut self, path: String, cx: &mut Context<Self>) {
        if self.disc_scan.as_ref().is_some_and(|(p, c)| *p == path && c.is_none()) {
            return; // already reading it
        }
        self.disc_scan = Some((path.clone(), None));
        let cfg = self.settings.engine_config();
        let p = path.clone();
        let task = cx.background_executor().spawn(async move { disc::read_disc(&p, &cfg) });
        cx.spawn(async move |this, cx| {
            let content = task.await;
            let _ = this.update(cx, |v, cx| {
                if v.disc_scan.as_ref().is_none_or(|(p, _)| *p != path) {
                    return; // another drive was chosen meanwhile
                }
                if let disc::DiscContent::Dvd(titles) = &content {
                    v.dvd_titles = Some((path.clone(), titles.clone()));
                }
                v.disc_scan = Some((path, Some(content)));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Plays a whole DVD from the dialog: from its menu, or its main feature.
    fn play_disc_dvd(&mut self, path: String, menus: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.close_dialog(window, cx);
        if menus {
            self.save_current_position();
            self.playlist.clear();
            self.playlist.add(vec![engine::MediaSource::Dvd { path, title: 0, angle: 1 }]);
            self.play_index(0, window, cx);
            return;
        }
        self.playlist.clear();
        match self.dvd_titles.as_ref().filter(|(p, _)| *p == path).and_then(|(_, t)| disc::main_title(t)) {
            Some(n) => self.play_dvd_title(path, n, window, cx),
            None => self.open_dvd_auto(path, window, cx),
        }
    }

    pub fn on_input_submit(&mut self, key: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.inputs.get(key).map(|i| i.read(cx).text()).unwrap_or_default();
        match key {
            "url" => {
                let t = text.trim().to_string();
                if !t.is_empty() {
                    self.close_dialog(window, cx);
                    self.open_sources(vec![playlist::source_for_arg(&t)], window, cx);
                }
            }
            "playlist_item_title" => {
                if let Some(Dialog::EditPlaylistItemTitle(id)) = self.dialog {
                    self.set_playlist_item_title(id, &text, window, cx);
                }
            }
            "collection_name" => {
                if self.dialog == Some(Dialog::NewCollection) {
                    self.create_collection_workspace(text.trim(), window, cx);
                }
            }
            "playlist_name" => {
                if let Some(Dialog::NamePlaylist(rename)) = self.dialog {
                    self.name_playlist_workspace(text.trim().to_owned(), rename, window, cx);
                } else if self.dialog == Some(Dialog::DuplicatePlaylist) {
                    self.duplicate_playlist_workspace(text.trim().to_owned(), window, cx);
                }
            }
            "jump" => match parse_time(&text) {
                Some(secs) => {
                    self.close_dialog(window, cx);
                    self.player.seek(secs, false, true);
                }
                None => self.osd("Couldn't understand that time", cx),
            },
            "volume" => self.finish_volume_edit(true, window, cx),
            // Enter in a subtitle font field commits it without closing Preferences.
            "sub_font_family" => {
                // Valid values were already applied by on_input_changed.
                cx.notify();
            }
            "playlist_filter" => {
                if !self.visible_playlist_indices(cx).is_empty() {
                    self.playlist_focus_list(window, cx);
                    self.playlist_key_play(window, cx);
                }
            }
            "playlist_switch" => self.combo_submit(window, cx),
            "shortcut_search" => {}
            _ => self.close_dialog(window, cx),
        }
    }

    pub fn on_input_changed(&mut self, key: &'static str, cx: &mut Context<Self>) {
        let text = self.inputs.get(key).map(|i| i.read(cx).text()).unwrap_or_default();
        match key {
            "playlist_filter" => {
                self.pl_scroll.0.borrow().base_handle.set_offset(gpui::Point::default());
                self.playlist_previews.hover = None;
                self.refresh_playlist_hover(cx);
                cx.notify();
                return;
            }
            "playlist_switch" => {
                if let Some(menu) = self.menu.as_mut() {
                    if let Some(combo) = menu.playlist_switch.as_mut() {
                        combo.filter(&text);
                        menu.scroll(0).set_offset(gpui::Point::default());
                    }
                }
                cx.notify();
                return;
            }
            "shortcut_search" => {
                self.modal_scroll.set_offset(gpui::Point::default());
                cx.notify();
                return;
            }
            "audio_langs" => self.settings.audio_langs = text,
            "sub_langs" => self.settings.sub_langs = text,
            "sub_charset" => self.settings.sub_charset = text,
            "sub_font_family" => {
                // An incomplete name is only a draft: don't replace the active font
                // or write settings.json on every keystroke.
                let family = text.trim();
                // An empty field explicitly selects the default subtitle font.
                // Only complete installed family names (or empty input) are saved.
                if (family.is_empty() || super::render::installed_subtitle_font(family, cx))
                    && self.settings.sub_font_family != family
                {
                    self.settings.sub_font_family = family.to_owned();
                    self.settings.sub_font_weight =
                        super::render::subtitle_weight_supported(family, self.settings.sub_font_weight);
                    self.apply_settings(cx);
                } else {
                    // Refresh the inline validation message without saving.
                    cx.notify();
                }
                return;
            }
            _ => return,
        }
        self.player.set_config(self.engine_config());
    }

    // ───────────────────────── building blocks ─────────────────────────

    fn modal(
        &self,
        title: &str,
        width: f32,
        body: AnyElement,
        footer: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.modal_impl(title, width, body, footer, false, cx)
    }

    /// A modal with a category list on the left and that category's options on
    /// the right. The two columns scroll independently: GPUI scrolls every
    /// scroll container under the pointer at once, so the body must not sit
    /// inside the modal's own scroll area.
    fn tabbed_modal(
        &self,
        title: &str,
        width: f32,
        height: f32,
        (names, current, make): (&[&str], usize, fn(usize) -> Dialog),
        content: gpui::Div,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane = div()
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .child(
                div()
                    .id(SharedString::from(format!("{title}-options")))
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.options_scroll)
                    .child(content),
            )
            .child(scrollbar("options-bar", &self.options_scroll, self.theme))
            .into_any_element();
        self.tabbed_pane(title, width, height, (names, current, make), pane, cx)
    }

    /// A tabbed modal whose right-hand side lays itself out (and scrolls
    /// whatever part of it should scroll).
    fn tabbed_pane(
        &self,
        title: &str,
        width: f32,
        height: f32,
        (names, current, make): (&[&str], usize, fn(usize) -> Dialog),
        pane: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = div()
            .flex()
            .h(px(height))
            .min_h_0()
            .child(self.tabs(names, current, make, cx))
            .child(pane)
            .into_any_element();
        self.modal_impl(title, width, body, None, true, cx)
    }

    /// `self_scrolling`: the body handles its own scrolling, and shrinks to fit
    /// instead of the modal scrolling it as a whole.
    fn modal_impl(
        &self,
        title: &str,
        width: f32,
        body: AnyElement,
        footer: Option<AnyElement>,
        self_scrolling: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let (offset_x, offset_y) = self.modal_offset;
        let bounds_cell = self.modal_bounds.clone();
        deferred(
            div()
                .id("modal-backdrop")
                .absolute()
                .inset_0()
                .bg(t.backdrop)
                .flex()
                .items_center()
                .justify_center()
                // Nothing behind a dialog reacts to the mouse: no hover,
                // tooltips, right-click menus or wheel.
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, w, cx| this.close_dialog(w, cx)))
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                    if this.handle_drag_move(ev, window, cx) {
                        cx.stop_propagation();
                        return;
                    }
                    let Some(ModalDragOrigin { start, initial, limits: (min_x, max_x, min_y, max_y) }) =
                        this.modal_drag_origin
                    else {
                        return;
                    };
                    if ev.pressed_button != Some(MouseButton::Left) {
                        this.modal_drag_origin = None;
                        return;
                    }
                    // The bounds are snapshotted at mouse-down. Using the live,
                    // moved bounds here creates a feedback loop near the edges.
                    let dx = f32::from(ev.position.x - start.x);
                    let dy = f32::from(ev.position.y - start.y);
                    this.modal_offset = ((initial.0 + dx).clamp(min_x, max_x), (initial.1 + dy).clamp(min_y, max_y));
                    cx.notify();
                    cx.stop_propagation();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        if this.drag.is_some() {
                            this.end_drag(window, cx);
                            cx.stop_propagation();
                        }
                        this.modal_drag_origin = None;
                    }),
                )
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .id("modal")
                        .relative()
                        .left(px(offset_x))
                        .top(px(offset_y))
                        .w(px(width))
                        .max_w(relative(0.95))
                        .max_h(relative(0.9))
                        .flex()
                        .flex_col()
                        .bg(t.surface)
                        .text_color(t.text)
                        .rounded(px(12.))
                        .border_1()
                        .border_color(t.border)
                        .shadow_lg()
                        .overflow_hidden()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            canvas(
                                move |bounds, _, _| {
                                    bounds_cell.set(bounds);
                                    bounds
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .child(
                            div()
                                .id("modal-titlebar")
                                .flex()
                                .items_center()
                                .h(px(46.))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                                        let bounds = this.modal_bounds.get();
                                        let viewport = window.viewport_size();
                                        let center_x = f32::from(bounds.origin.x) - this.modal_offset.0;
                                        let center_y = f32::from(bounds.origin.y) - this.modal_offset.1;
                                        let limit_x =
                                            f32::from(viewport.width) - center_x - f32::from(bounds.size.width);
                                        let limit_y =
                                            f32::from(viewport.height) - center_y - f32::from(bounds.size.height);
                                        let limits = (
                                            (-center_x).min(limit_x),
                                            (-center_x).max(limit_x),
                                            (-center_y).min(limit_y),
                                            (-center_y).max(limit_y),
                                        );
                                        this.modal_drag_origin = Some(ModalDragOrigin {
                                            start: ev.position,
                                            initial: this.modal_offset,
                                            limits,
                                        });
                                        cx.stop_propagation();
                                    }),
                                )
                                .px_4()
                                .border_b_1()
                                .border_color(t.border)
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(px(15.))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(title.to_string()),
                                )
                                .child(self.icon_button(
                                    "modal-close",
                                    Icon::WindowClose,
                                    "Close (Esc)",
                                    t,
                                    |this, _, w, cx| this.close_dialog(w, cx),
                                    cx,
                                )),
                        )
                        .child(if self_scrolling {
                            div().id("modal-body").flex_1().min_h_0().flex().flex_col().child(body).into_any_element()
                        } else {
                            div()
                                .relative()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .id("modal-body")
                                        .flex_1()
                                        .min_h_0()
                                        .overflow_y_scroll()
                                        .track_scroll(&self.modal_scroll)
                                        .child(body),
                                )
                                .child(scrollbar("modal-bar", &self.modal_scroll, t))
                                .into_any_element()
                        })
                        .when_some(footer, |d, f| {
                            d.child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap_2()
                                    .px_4()
                                    .py_3()
                                    .border_t_1()
                                    .border_color(t.border)
                                    .child(f),
                            )
                        }),
                ),
        )
        .with_priority(5)
        .into_any_element()
    }

    fn section(&self, title: &str) -> AnyElement {
        div()
            .pt_4()
            .pb_1()
            .text_size(px(11.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(self.theme.accent)
            .child(title.to_uppercase())
            .into_any_element()
    }

    fn row(&self, label: &str, desc: Option<&str>, control: AnyElement) -> AnyElement {
        self.aligned_row(label, desc, control, false)
    }

    fn aligned_row(&self, label: &str, desc: Option<&str>, control: AnyElement, top: bool) -> AnyElement {
        self.option_row(label, desc, control, top, true, false)
    }

    /// A settings row. A disabled row keeps its layout and is only dimmed;
    /// a nested row is indented under the option it depends on.
    fn option_row(
        &self,
        label: &str,
        desc: Option<&str>,
        control: AnyElement,
        top: bool,
        enabled: bool,
        nested: bool,
    ) -> AnyElement {
        let t = self.theme;
        // No explicit width: a percentage width turns off the column's stretch,
        // and inside the scroll area it can resolve to the content width.
        div()
            .flex()
            .when(top, |d| d.items_start())
            .when(!top, |d| d.items_center())
            .gap_4()
            .py_2()
            .when(nested, |d| d.pl_5())
            .border_b_1()
            .border_color(t.border.opacity(0.5))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .when(!enabled, |d| d.opacity(0.45))
                    .child(div().text_size(px(13.)).child(label.to_string()))
                    .when_some(desc, |d, s| {
                        d.child(div().text_size(px(11.)).text_color(t.text_dim).child(s.to_string()))
                    }),
            )
            .child(div().flex_none().when(!enabled, |d| d.opacity(0.45)).child(control))
            .into_any_element()
    }

    fn toggle(
        &self,
        id: impl Into<SharedString>,
        on: bool,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        div()
            .id(id.into())
            .w(px(40.))
            .h(px(22.))
            .rounded_full()
            .p(px(3.))
            .flex()
            .cursor_pointer()
            .bg(if on { t.accent } else { t.track })
            .when(on, |d| d.justify_end())
            .child(div().size(px(16.)).rounded_full().bg(gpui::white()).shadow_sm())
            .on_click(cx.listener(move |this, _, _, cx| {
                f(this, cx);
                this.apply_settings(cx);
            }))
            .into_any_element()
    }

    fn toggle_row(
        &self,
        id: &str,
        label: &str,
        desc: Option<&str>,
        on: bool,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.row(label, desc, self.toggle(format!("tg-{id}"), on, f, cx))
    }

    /// A switch that can be disabled (it then shows the stored value but
    /// doesn't react) and nested under the option it depends on.
    #[allow(clippy::too_many_arguments)]
    fn option_toggle(
        &self,
        id: &str,
        label: &str,
        desc: &str,
        on: bool,
        enabled: bool,
        nested: bool,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let control = if enabled {
            self.toggle(format!("tg-{id}"), on, f, cx)
        } else {
            let t = self.theme;
            div()
                .w(px(40.))
                .h(px(22.))
                .rounded_full()
                .p(px(3.))
                .flex()
                .bg(if on { t.accent } else { t.track })
                .when(on, |d| d.justify_end())
                .child(div().size(px(16.)).rounded_full().bg(gpui::white()).shadow_sm())
                .into_any_element()
        };
        self.option_row(label, Some(desc), control, false, enabled, nested)
    }

    #[allow(clippy::too_many_arguments)]
    fn option_choice<T: Clone + PartialEq + 'static>(
        &self,
        id: &str,
        label: &str,
        desc: &str,
        options: Vec<(String, T)>,
        current: T,
        enabled: bool,
        f: impl Fn(&mut Self, T, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let control = if enabled {
            self.choice(id, options, current, f, cx)
        } else {
            let text = options.iter().find(|(_, v)| *v == current).map(|(l, _)| l.clone()).unwrap_or_default();
            div().text_size(px(13.)).child(text).into_any_element()
        };
        self.option_row(label, Some(desc), control, false, enabled, false)
    }

    fn timeline_visualization_row(&self, fullscreen: bool, cx: &mut Context<Self>) -> AnyElement {
        use super::timeline_settings::{BARCODES, MAIN};
        let s = &self.settings;
        let mut cards = div().w(px(300.)).flex().flex_col().gap_2();
        if fullscreen {
            cards = cards.child(self.toggle_row(
                "thumb-fs-mode-inherit",
                "Same as in a window",
                None,
                s.timeline_fullscreen_visualization.is_none(),
                |t, _| {
                    t.settings.timeline_fullscreen_visualization =
                        if t.settings.timeline_fullscreen_visualization.is_none() {
                            Some(t.settings.timeline_visualization(false))
                        } else {
                            None
                        }
                },
                cx,
            ));
        }
        let control = cards
            .child(self.timeline_cards(&MAIN, fullscreen, cx))
            .child(div().text_size(px(11.)).text_color(self.theme.text_dim).child("VIDEO BARCODES"))
            .child(self.timeline_cards(&BARCODES, fullscreen, cx))
            .into_any_element();
        self.aligned_row(
            "Visualization type",
            Some("Choose the visual representation of the media above the seekbar."),
            control,
            true,
        )
    }

    fn visualization_number_row(&self, fullscreen: bool, height: bool, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.settings;
        let current = if height { s.visualization_height(fullscreen) } else { s.visualization_opacity(fullscreen) };
        let (min, max) = if height { (12, 96) } else { (50, 100) };
        let id = if height {
            "thumb-fs-height"
        } else if fullscreen {
            "thumb-fs-opacity"
        } else {
            "thumb-opacity"
        };
        let label = if height { "Visualization height" } else { "Visualization opacity" };
        let desc = if height {
            "Height of the visualization in logical pixels."
        } else {
            "Transparency of the visualization (100% is opaque)."
        };
        let mut control = div().flex().flex_col().w(px(190.)).gap_1();
        if fullscreen {
            let inherited =
                if height { s.timeline_fullscreen_height.is_none() } else { s.timeline_fullscreen_opacity.is_none() };
            control = control.child(
                div().flex().items_center().justify_between().text_size(px(11.)).child("Same as in a window").child(
                    self.toggle(
                        format!("{id}-inherit"),
                        inherited,
                        move |t, _| {
                            if height {
                                t.settings.timeline_fullscreen_height =
                                    if inherited { Some(t.settings.visualization_height(false)) } else { None };
                            } else {
                                t.settings.timeline_fullscreen_opacity =
                                    if inherited { Some(t.settings.visualization_opacity(false)) } else { None };
                            }
                        },
                        cx,
                    ),
                ),
            );
        }
        control = control.child(div().text_size(px(12.)).child(if height {
            format!("{current} px")
        } else {
            format!("{current}%")
        }));
        if s.visualization_enabled(fullscreen) {
            control = control.child(self.slider(
                id,
                (current - min) as f32 / (max - min) as f32,
                false,
                self.theme,
                move |t, f, _, _| {
                    let n = min + (f * (max - min) as f32).round() as u32;
                    match (fullscreen, height) {
                        (true, true) => t.settings.timeline_fullscreen_height = Some(n),
                        (true, false) => t.settings.timeline_fullscreen_opacity = Some(n),
                        (false, false) => t.settings.timeline_opacity = n,
                        (false, true) => {}
                    }
                },
                cx,
            ));
        }
        self.option_row(
            label,
            Some(desc),
            control.into_any_element(),
            false,
            s.visualization_enabled(fullscreen),
            false,
        )
    }

    fn thumbnail_height_row(&self, minimum: bool, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.settings;
        let max = s.timeline_filmstrip_height.clamp(12, 96);
        let min = s.timeline_filmstrip_min_height.clamp(12, max);
        let (label, current, limit, enabled) = if minimum {
            ("Minimum visualization height", min, max, s.visualization_enabled(false) && s.timeline_adaptive)
        } else {
            ("Visualization height", max, 96, s.visualization_enabled(false))
        };
        let mut control = div().w(px(190.)).child(div().text_size(px(12.)).child(format!("{current} px")));
        if enabled && limit > 12 {
            control = control.child(self.slider(
                if minimum { "thumb-min-height" } else { "thumb-height" },
                (current - 12) as f32 / (limit - 12) as f32,
                false,
                self.theme,
                move |t, f, _, _| {
                    let n = 12 + (f * (limit - 12) as f32).round() as u32;
                    if minimum {
                        t.settings.timeline_filmstrip_min_height = n;
                    } else {
                        t.settings.timeline_filmstrip_height = n;
                        t.settings.timeline_filmstrip_min_height = t.settings.timeline_filmstrip_min_height.min(n);
                    }
                },
                cx,
            ));
        }
        let desc = if minimum {
            "Smallest height the visualization shrinks to in narrow windows."
        } else {
            "Height of the visualization in logical pixels."
        };
        self.option_row(label, Some(desc), control.into_any_element(), false, enabled, minimum)
    }

    /// Settings rows for enlarging video: the upscaler, and the options of
    /// the chosen one.
    fn upscaling_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let s = &self.settings;
        let gpu = engine::gpu_upscaling_available();
        let options: Vec<(String, Upscaler)> =
            Upscaler::ALL.into_iter().filter(|u| gpu || !u.needs_libplacebo()).map(|u| (u.label().into(), u)).collect();
        let desc = if gpu {
            "For pictures smaller than the window. All but the first three run on the GPU through libplacebo (Vulkan)"
        } else {
            "GPU upscalers and shaders need FFmpeg built with libplacebo"
        };
        let mut rows = vec![self.row(
            "Upscaling",
            Some(desc),
            self.choice("upscale", options, s.upscaler, |t, v, _| t.settings.upscaler = v, cx),
        )];
        if !gpu || !s.upscaler.needs_libplacebo() {
            return rows;
        }
        match s.upscaler {
            Upscaler::Anime4K => {
                let modes = Anime4KMode::ALL
                    .into_iter()
                    .map(|m| {
                        let what = match m {
                            Anime4KMode::A => "blurry, 1080p sources",
                            Anime4KMode::B => "720p, aliasing or ringing",
                            Anime4KMode::C => "480p and SD",
                            Anime4KMode::AA => "stronger A",
                            Anime4KMode::BB => "stronger B",
                            Anime4KMode::CA => "C, then A",
                        };
                        (format!("{} ({what})", m.name()), m)
                    })
                    .collect();
                rows.push(self.row(
                    "Anime4K mode",
                    Some("Try A first; pick whichever looks best for the show"),
                    self.choice("a4kmode", modes, s.anime4k_mode, |t, v, _| t.settings.anime4k_mode = v, cx),
                ));
                rows.push(self.row(
                    "Anime4K quality",
                    Some("Fast suits laptops and integrated graphics"),
                    self.choice(
                        "a4kq",
                        vec![("High".into(), Anime4KQuality::High), ("Fast".into(), Anime4KQuality::Fast)],
                        s.anime4k_quality,
                        |t, v, _| t.settings.anime4k_quality = v,
                        cx,
                    ),
                ));
            }
            Upscaler::CustomShader => {
                let file = s.custom_shader.as_ref().map(|p| p.display().to_string());
                rows.push(
                    self.row(
                        "Shader file",
                        Some(
                            file.as_deref()
                                .unwrap_or("An mpv-style .glsl or .hook file (FSRCNNX, NNEDI3, SSimSuperRes, ...)"),
                        ),
                        self.text_button(
                            "shaderfile",
                            "Choose…",
                            false,
                            |_this, _, w, cx| {
                                let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                                    files: true,
                                    directories: false,
                                    multiple: false,
                                    prompt: Some("Choose".into()),
                                });
                                cx.spawn_in(w, async move |this, cx| {
                                    if let Ok(Ok(Some(p))) = rx.await
                                        && let Some(p) = p.into_iter().next()
                                    {
                                        let _ = this.update(cx, |v, cx| {
                                            v.settings.custom_shader = Some(p);
                                            v.apply_settings(cx);
                                        });
                                    }
                                })
                                .detach();
                            },
                            cx,
                        )
                        .into_any_element(),
                    ),
                );
            }
            _ => {}
        }
        rows.push(self.toggle_row(
            "krig",
            "Better chroma (KrigBilateral)",
            Some("Sharper colour edges, using the brightness detail"),
            s.shader_chroma,
            |t, _| t.settings.shader_chroma = !t.settings.shader_chroma,
            cx,
        ));
        rows.push(self.toggle_row(
            "cas",
            "Sharpen (AMD CAS)",
            Some("Contrast-adaptive sharpening after upscaling"),
            s.shader_sharpen,
            |t, _| t.settings.shader_sharpen = !t.settings.shader_sharpen,
            cx,
        ));
        rows
    }

    /// A dropdown button; choosing an option calls `f(index)`.
    fn choice<T: Clone + PartialEq + 'static>(
        &self,
        id: &str,
        options: Vec<(String, T)>,
        current: T,
        f: impl Fn(&mut Self, T, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let label = options.iter().find(|(_, v)| *v == current).map(|(l, _)| l.clone()).unwrap_or_else(|| "—".into());
        let f = std::rc::Rc::new(f);
        div()
            .id(SharedString::from(format!("ch-{id}")))
            .min_w(px(170.))
            .h(px(30.))
            .px_3()
            .gap_2()
            .flex()
            .items_center()
            .rounded(px(7.))
            .bg(t.surface_alt)
            .border_1()
            .border_color(t.border)
            .cursor_pointer()
            .hover(|s| s.bg(t.hover))
            .child(div().flex_1().truncate().text_size(px(13.)).child(label))
            .child(icon(Icon::DropDown, px(12.), t.text_dim))
            .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                let items: Vec<M> = options
                    .iter()
                    .map(|(l, v)| {
                        let (v2, f2) = (v.clone(), f.clone());
                        M::action(l.clone(), move |this, _, cx| {
                            f2(this, v2.clone(), cx);
                            this.apply_settings(cx);
                        })
                        .selected(*v == current)
                    })
                    .collect();
                let p = ev.position();
                this.open_menu(items, gpui::point(p.x - px(20.), p.y + px(12.)), None, cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn stepper(
        &self,
        id: &str,
        text: String,
        f: impl Fn(&mut Self, i32, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let f = std::rc::Rc::new(f);
        let (f1, f2) = (f.clone(), f);
        let btn = |sid: String, label: &'static str| {
            div()
                .id(SharedString::from(sid))
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .bg(t.surface_alt)
                .border_1()
                .border_color(t.border)
                .cursor_pointer()
                .hover(|s| s.bg(t.hover))
                .child(label)
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(btn(format!("st-{id}-m"), "−").on_click(cx.listener(move |this, _, _, cx| {
                f1(this, -1, cx);
                this.apply_settings(cx);
            })))
            .child(div().min_w(px(70.)).flex().justify_center().text_size(px(13.)).child(text))
            .child(btn(format!("st-{id}-p"), "+").on_click(cx.listener(move |this, _, _, cx| {
                f2(this, 1, cx);
                this.apply_settings(cx);
            })))
            .into_any_element()
    }

    /// Editable font family field with a menu sourced from the cached fontdb catalog.
    /// The arrow shows all fonts when the field holds the selected family.
    /// A partially typed family filters the menu on the next arrow click.
    fn font_family_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let field = self.input_box("sub_font_family");
        let arrow = div()
            .id("subtitle-font-family-menu")
            .size(px(30.))
            .flex_none()
            .rounded(px(7.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_alt)
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .hover(|d| d.bg(theme.hover))
            .child(icon(Icon::DropDown, px(13.), theme.text_dim))
            .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                let entered = this.inputs.get("sub_font_family").map(|input| input.read(cx).text()).unwrap_or_default();
                // The input initially contains the selected font, not a search
                // query. On the first click show *all* families. Once the user
                // edits it to an incomplete name, treat that draft as a filter.
                let query = entered.trim();
                let filter = if query.eq_ignore_ascii_case(this.settings.sub_font_family.trim()) { "" } else { query };
                let mut items = vec![
                    M::action(format!("Default ({DEFAULT_SUB_FONT_FAMILY})"), |this, _, cx| {
                        this.select_subtitle_font("", cx)
                    })
                    .selected(this.settings.sub_font_family.is_empty()),
                ];

                // The existing GPUI menu already has a scrolling viewport,
                // so truncating to 80 families made the rest inaccessible.
                let matches = super::render::subtitle_font_families()
                    .iter()
                    .filter(|family| filter.is_empty() || family.to_lowercase().contains(&filter.to_lowercase()));
                for family in matches {
                    let chosen = family.clone();
                    let selected = this.settings.sub_font_family.eq_ignore_ascii_case(family);
                    items.push(
                        M::action(family.clone(), move |this, _, cx| {
                            this.select_subtitle_font(&chosen, cx);
                        })
                        .selected(selected),
                    );
                }
                let pos = ev.position();
                this.open_menu(items, gpui::point(pos.x - px(210.), pos.y + px(12.)), None, cx);
                cx.stop_propagation();
            }));
        div().flex().items_center().gap_1().child(field).child(arrow).into_any_element()
    }

    fn select_subtitle_font(&mut self, family: &str, cx: &mut Context<Self>) {
        if let Some(input) = self.inputs.get("sub_font_family").cloned() {
            // Update the visible field even when the value is unchanged.
            input.update(cx, |input, cx| input.set_text(family.to_owned(), cx));
        }
        if self.settings.sub_font_family != family {
            self.settings.sub_font_family = family.to_owned();
            self.settings.sub_font_weight =
                super::render::subtitle_weight_supported(family, self.settings.sub_font_weight);
            self.apply_settings(cx);
        } else {
            cx.notify();
        }
    }

    fn input_box(&self, key: &'static str) -> AnyElement {
        self.input_box_width(key, 220.)
    }

    fn input_box_width(&self, key: &'static str, width: f32) -> AnyElement {
        let t = self.theme;
        let Some(e) = self.inputs.get(key).cloned() else { return div().into_any_element() };
        div()
            .w(px(width))
            .h(px(30.))
            .px_2()
            .flex()
            .items_center()
            .rounded(px(7.))
            .bg(t.surface_alt)
            .border_1()
            .border_color(t.border)
            .child(e)
            .into_any_element()
    }

    fn tabs(&self, names: &[&str], current: usize, make: fn(usize) -> Dialog, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let mut col = div()
            .id("dialog-tabs")
            .w(px(170.))
            .flex_none()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p_2()
            .bg(t.surface_alt)
            .border_r_1()
            .border_color(t.border);
        for (i, n) in names.iter().enumerate() {
            col = col.child(
                div()
                    .id(SharedString::from(format!("tab-{n}")))
                    .px_3()
                    .h(px(32.))
                    // Keep full height in a short window; the list scrolls instead.
                    .flex_none()
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .when(i == current, |d| d.bg(t.accent_soft).text_color(t.accent))
                    .when(i != current, |d| d.hover(|s| s.bg(t.hover)))
                    .child(n.to_string())
                    .on_click(cx.listener(move |this, _, w, cx| this.show_dialog(make(i), w, cx))),
            );
        }
        col.into_any_element()
    }

    // ───────────────────────── dialogs ─────────────────────────

    pub fn render_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.dialog?;
        // Keep text input colours in sync with the theme.
        let t = self.theme;
        for e in self.inputs.values() {
            e.update(cx, |i, _| i.colors = (t.text, t.text_disabled, t.accent_soft, t.accent));
        }
        Some(match d {
            Dialog::Settings(tab) => self.settings_dialog(tab, cx),
            Dialog::Effects(tab) => self.effects_dialog(tab, cx),
            Dialog::MediaInfo => self.media_info_dialog(cx),
            Dialog::OpenUrl => {
                let body = div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(t.text_dim)
                            .child("Network stream or file path (HTTP, HTTPS, HLS, RTSP, RTMP, SRT, FTP, …):"),
                    )
                    .child(div().w_full().child(self.input_box("url")).child(div()))
                    .into_any_element();
                let footer = div()
                    .flex()
                    .gap_2()
                    .child(self.text_button(
                        "url-add",
                        "Add to Playlist",
                        false,
                        |this, _, w, cx| {
                            let t = this.inputs.get("url").map(|i| i.read(cx).text()).unwrap_or_default();
                            if !t.trim().is_empty() {
                                this.close_dialog(w, cx);
                                this.enqueue(vec![playlist::source_for_arg(t.trim())], w, cx);
                            }
                        },
                        cx,
                    ))
                    .child(self.text_button(
                        "url-open",
                        "Play",
                        true,
                        |this, _, w, cx| this.on_input_submit("url", w, cx),
                        cx,
                    ))
                    .into_any_element();
                self.modal("Open URL", 560., body, Some(footer), cx)
            }
            Dialog::EditPlaylistItemTitle(id) => {
                let body = div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("Set a title for this playlist item without changing the media file. Leave empty to reset.")
                    .child(self.input_box("playlist_item_title"))
                    .into_any_element();
                let footer = div()
                    .flex()
                    .gap_2()
                    .child(self.text_button(
                        "playlist-item-title-reset",
                        "Reset Title",
                        false,
                        move |this, _, w, cx| this.set_playlist_item_title(id, "", w, cx),
                        cx,
                    ))
                    .child(self.text_button(
                        "playlist-item-title-confirm",
                        "Apply",
                        true,
                        |this, _, w, cx| this.on_input_submit("playlist_item_title", w, cx),
                        cx,
                    ))
                    .into_any_element();
                self.modal("Edit item title", 420., body, Some(footer), cx)
            }
            Dialog::NewCollection => {
                let body = div().p_4().child(self.input_box("collection_name")).into_any_element();
                let footer = self.text_button(
                    "collection-name-confirm", "Create", true,
                    |this, _, w, cx| this.on_input_submit("collection_name", w, cx), cx,
                ).into_any_element();
                self.modal("New collection", 380., body, Some(footer), cx)
            }
            Dialog::NamePlaylist(rename) => {
                let body = div().p_4().child(self.input_box("playlist_name")).into_any_element();
                let footer = self
                    .text_button(
                        "playlist-name-confirm",
                        if rename { "Rename" } else { "Create" },
                        true,
                        |this, _, w, cx| this.on_input_submit("playlist_name", w, cx),
                        cx,
                    )
                    .into_any_element();
                self.modal(if rename { "Rename playlist" } else { "New playlist" }, 380., body, Some(footer), cx)
            }
            Dialog::DuplicatePlaylist => {
                let body = div().p_4().child(self.input_box("playlist_name")).into_any_element();
                let footer = self
                    .text_button(
                        "playlist-duplicate-confirm",
                        "Duplicate",
                        true,
                        |this, _, w, cx| this.on_input_submit("playlist_name", w, cx),
                        cx,
                    )
                    .into_any_element();
                self.modal("Duplicate playlist as…", 380., body, Some(footer), cx)
            }
            Dialog::DeletePlaylist => {
                let body = div()
                    .p_4()
                    .child(format!(
                        "Delete the saved playlist ‘{}’? This does not delete any media files.",
                        self.playlist_library.name()
                    ))
                    .into_any_element();
                let footer = div()
                    .flex()
                    .gap_2()
                    .child(self.text_button(
                        "playlist-delete-cancel",
                        "Cancel",
                        false,
                        |this, _, w, cx| this.close_dialog(w, cx),
                        cx,
                    ))
                    .child(self.text_button(
                        "playlist-delete-confirm",
                        "Delete",
                        true,
                        |this, _, w, cx| this.delete_playlist_workspace(w, cx),
                        cx,
                    ))
                    .into_any_element();
                self.modal("Delete playlist", 420., body, Some(footer), cx)
            }
            Dialog::JumpTo => {
                let dur = self.player.duration();
                let body = div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(t.text_dim)
                            .child(format!("Duration: {}", format_time(dur))),
                    )
                    .child(self.input_box("jump"))
                    .into_any_element();
                let footer = self
                    .text_button("jump-go", "Go", true, |this, _, w, cx| this.on_input_submit("jump", w, cx), cx)
                    .into_any_element();
                self.modal("Jump to Time", 380., body, Some(footer), cx)
            }
            Dialog::OpenDisc => self.disc_dialog(cx),
            Dialog::About => {
                let body = div()
                    .p_6()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .text_center()
                    .child(img(ImageSource::Resource(Resource::Embedded("app-icon.svg".into()))).size(px(80.)))
                    .child(div().text_size(px(22.)).font_weight(gpui::FontWeight::SEMIBOLD).child("MavPlayer"))
                    .child(div().text_color(t.text_dim).child(format!("Version {}", env!("CARGO_PKG_VERSION"))))
                    .child(
                        div().mt_2().text_size(px(13.)).child("A fast, native media player for video, music and DVDs."),
                    )
                    .child(div().mt_3().text_size(px(12.)).text_color(t.text_dim).child(format!(
                        "Playback engine: FFmpeg {} (libavcodec, libavformat, libavfilter, libswscale, libswresample)",
                        ffmpeg_version()
                    )))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(t.text_dim)
                            .child("User interface: GPUI · Audio output: cpal"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(t.text_dim)
                            .child("Icons: Papirus by the Papirus Development Team (GPL-3.0), or your desktop theme"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_1()
                            .text_size(px(12.))
                            .text_color(t.text_dim)
                            .child(format!(
                                "Bundled subtitle font: {} · © 2020 The Open Sans Project Authors ·",
                                BUNDLED_SUB_FONT_FAMILY
                            ))
                            .child(
                                div()
                                    .id("subtitle-font-license")
                                    .text_color(t.accent)
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(t.text))
                                    .child("SIL OFL 1.1")
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.show_dialog(Dialog::SubtitleFontLicense, w, cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_size(px(12.))
                            .text_color(t.text_dim)
                            .child("Licensed under the GNU General Public License v3 or later"),
                    )
                    .into_any_element();
                self.modal("About MavPlayer", 520., body, None, cx)
            }
            Dialog::SubtitleFontLicense => {
                let mut body = div().p_4().flex().flex_col().gap_1();
                for line in include_str!("../../assets/fonts/OFL.txt").lines() {
                    body = body.child(div().text_size(px(12.)).child(line.to_owned()));
                }
                let back = self
                    .text_button(
                        "font-license-back",
                        "Back to About",
                        false,
                        |this, _, w, cx| this.show_dialog(Dialog::About, w, cx),
                        cx,
                    )
                    .into_any_element();
                self.modal(
                    &format!("{BUNDLED_SUB_FONT_FAMILY} — SIL Open Font License 1.1"),
                    650.,
                    body.into_any_element(),
                    Some(back),
                    cx,
                )
            }
            Dialog::Integrate => {
                let body = div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_size(px(13.))
                    .child("Add MavPlayer to your application menu and to “Open With” for video, audio and disc files?")
                    .child(div().text_color(t.text_dim).child(
                        "This creates a launcher in ~/.local/share/applications that points at this AppImage. You can remove it later from the Media menu or with --remove-integration.",
                    ))
                    .into_any_element();
                let footer = div()
                    .flex()
                    .gap_2()
                    .child(self.text_button("int-no", "Not Now", false, |this, _, w, cx| this.close_dialog(w, cx), cx))
                    .child(self.text_button(
                        "int-yes",
                        "Integrate",
                        true,
                        |this, _, w, cx| {
                            match crate::integration::integrate() {
                                Ok(p) => this.osd(format!("Added to the application menu ({})", p.display()), cx),
                                Err(e) => this.osd(format!("Integration failed: {e}"), cx),
                            }
                            this.close_dialog(w, cx);
                        },
                        cx,
                    ))
                    .into_any_element();
                let _ = window;
                self.modal("Desktop Integration", 480., body, Some(footer), cx)
            }
        })
    }

    // ───────────────────────── keyboard shortcuts ─────────────────────────

    /// The Keyboard Shortcuts category: a search box over every command,
    /// with its keys to remove, add or reset.
    fn shortcuts_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let query = self.inputs.get("shortcut_search").map(|i| i.read(cx).text()).unwrap_or_default();
        let q = query.trim().to_lowercase();
        let overrides = self.settings.key_bindings.clone();
        let mut list = div().flex().flex_col().px_4().pb_3();
        let mut group = "";
        let mut shown = 0;
        for c in keymap::COMMANDS {
            let keys = keymap::keys_for(c, &overrides);
            let hit = q.is_empty()
                || c.label.to_lowercase().contains(&q)
                || c.group.to_lowercase().contains(&q)
                || keys.iter().any(|k| keymap::shortcut(k).to_lowercase().contains(&q) || k.to_lowercase() == q);
            if !hit {
                continue;
            }
            if c.group != group {
                group = c.group;
                list = list.child(self.section(group));
            }
            shown += 1;
            list = list.child(self.shortcut_row(c, keys, overrides.contains_key(c.id), cx));
        }
        if shown == 0 {
            list = list.child(
                div()
                    .py_8()
                    .flex()
                    .justify_center()
                    .text_color(t.text_dim)
                    .child(format!("Nothing matches “{}”", query.trim())),
            );
        }
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .flex_1()
                    .h(px(30.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(7.))
                    .bg(t.surface_alt)
                    .border_1()
                    .border_color(t.border)
                    .children(self.inputs.get("shortcut_search").cloned().map(|e| div().flex_1().child(e))),
            )
            .when(!overrides.is_empty(), |d| {
                d.child(self.text_button(
                    "keys-reset-all",
                    "Reset All",
                    false,
                    |this, _, _, cx| {
                        this.settings.key_bindings.clear();
                        this.keymap_changed(Some("All shortcuts are back to their defaults".into()), cx);
                    },
                    cx,
                ))
            });
        let footer_text = match (&self.shortcut_note, self.capturing_key) {
            (_, Some(_)) => "Press the new key combination, or Esc to cancel".to_string(),
            (Some(n), None) => n.clone(),
            (None, None) => "+ adds a key, × removes one. Double-click the video for fullscreen.".to_string(),
        };
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id("shortcut-list")
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.modal_scroll)
                            .child(list),
                    )
                    .child(scrollbar("shortcut-bar", &self.modal_scroll, t)),
            )
            .child(
                div()
                    .flex_none()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(t.border)
                    .text_size(px(12.))
                    .text_color(if self.capturing_key.is_some() { t.accent } else { t.text_dim })
                    .child(footer_text),
            )
            .into_any_element()
    }

    fn shortcut_row(
        &self,
        c: &'static Command,
        keys: Vec<String>,
        changed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let capturing = self.capturing_key == Some(c.id);
        let small_btn = |id: String, ic: Icon, size: f32| {
            div()
                .id(SharedString::from(id))
                .size(px(size))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(t.hover))
                .child(icon(ic, px(size * 0.6), t.text_dim))
        };
        let mut chips = div().flex().flex_wrap().items_center().justify_end().gap_1();
        for k in keys {
            let label = keymap::shortcut(&k);
            let k2 = k.clone();
            chips = chips.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .h(px(24.))
                    .pl_2()
                    .pr(px(2.))
                    .rounded(px(5.))
                    .bg(t.surface_alt)
                    .border_1()
                    .border_color(t.border)
                    .text_size(px(12.))
                    .child(label)
                    .child(
                        small_btn(format!("key-rm-{}-{k}", c.id), Icon::WindowClose, 18.)
                            .tooltip(super::widgets::tooltip("Remove this shortcut", t))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_key(c, &k2, cx))),
                    ),
            );
        }
        if capturing {
            chips = chips.child(
                div()
                    .h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(t.accent)
                    .bg(t.accent_soft)
                    .text_color(t.accent)
                    .text_size(px(12.))
                    .child("Press a key…"),
            );
        } else {
            chips = chips.child(
                small_btn(format!("key-add-{}", c.id), Icon::Add, 24.)
                    .tooltip(super::widgets::tooltip("Add a shortcut", t))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.capturing_key = Some(c.id);
                        this.shortcut_note = None;
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .items_center()
            .gap_3()
            .py(px(4.))
            .pr(px(SCROLLBAR_W))
            .border_b_1()
            .border_color(t.border.opacity(0.4))
            .when(capturing, |d| d.bg(t.accent_soft.opacity(0.5)))
            .child(div().flex_1().min_w_0().text_size(px(13.)).child(c.label))
            .child(chips)
            .child(div().w(px(24.)).flex_none().when(changed, |d| {
                d.child(
                    small_btn(format!("key-reset-{}", c.id), Icon::Reset, 24.)
                        .tooltip(super::widgets::tooltip("Back to the default", t))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings.key_bindings.remove(c.id);
                            let k = c.keys.iter().map(|k| keymap::shortcut(k)).collect::<Vec<_>>().join(", ");
                            let k = if k.is_empty() { "no keys".to_string() } else { k };
                            this.keymap_changed(Some(format!("“{}” is back to {k}", c.label)), cx);
                        })),
                )
            }))
            .into_any_element()
    }

    fn remove_key(&mut self, c: &'static Command, key: &str, cx: &mut Context<Self>) {
        let mut keys = keymap::keys_for(c, &self.settings.key_bindings);
        keys.retain(|k| k != key);
        keymap::set_keys(c, keys, &mut self.settings.key_bindings);
        self.keymap_changed(Some(format!("Removed {} from “{}”", keymap::shortcut(key), c.label)), cx);
    }

    /// The keystroke recorded for the command the dialog is waiting on. The
    /// key moves there from any command that had it.
    pub fn capture_keystroke(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) {
        let Some(c) = self.capturing_key.take().and_then(keymap::command) else { return };
        let m = &ks.modifiers;
        if ks.key == "escape" && !(m.control || m.alt || m.shift || m.platform) {
            self.shortcut_note = None;
            cx.notify();
            return;
        }
        let key = ks.unparse();
        let mut moved_from = Vec::new();
        for other in keymap::COMMANDS.iter().filter(|o| o.id != c.id) {
            let mut keys = keymap::keys_for(other, &self.settings.key_bindings);
            let before = keys.len();
            keys.retain(|k| !keymap::same_keys(k, &key));
            if keys.len() != before {
                keymap::set_keys(other, keys, &mut self.settings.key_bindings);
                moved_from.push(format!("“{}”", other.label));
            }
        }
        let mut keys = keymap::keys_for(c, &self.settings.key_bindings);
        if !keys.iter().any(|k| keymap::same_keys(k, &key)) {
            keys.push(key.clone());
        }
        keymap::set_keys(c, keys, &mut self.settings.key_bindings);
        let mut note = format!("{} now does “{}”", keymap::shortcut(&key), c.label);
        if !moved_from.is_empty() {
            note.push_str(&format!(" (taken from {})", moved_from.join(", ")));
        }
        self.keymap_changed(Some(note), cx);
    }

    fn keymap_changed(&mut self, note: Option<String>, cx: &mut Context<Self>) {
        keymap::apply(cx, &self.settings.key_bindings);
        // The native menu reads its key equivalents when it is built.
        #[cfg(target_os = "macos")]
        super::actions::set_native_menus(cx);
        self.settings.save();
        self.shortcut_note = note;
        cx.notify();
    }

    fn settings_dialog(&mut self, tab: usize, cx: &mut Context<Self>) -> AnyElement {
        if tab == SHORTCUTS_TAB {
            let pane = self.shortcuts_pane(cx);
            return self.tabbed_pane("Preferences", 820., 520., (SETTINGS_TABS, tab, Dialog::Settings), pane, cx);
        }
        let s = self.settings.clone();
        let system: Vec<String> = engine::system_languages().iter().map(|l| engine::language_name(l)).collect();
        let sub_langs_hint = format!(
            "Most preferred first, then your system's ({}). Shown by themselves when the audio is in another language",
            if system.is_empty() { "none found".to_string() } else { system.join(", ") }
        );
        let mut c = div().flex_1().min_w_0().px_5().pb_4().flex().flex_col();
        match tab {
            0 => {
                c = c
                    .child(self.section("Appearance"))
                    .child(self.row(
                        "Theme",
                        None,
                        self.choice(
                            "theme",
                            vec![
                                ("Follow system".into(), ThemeMode::System),
                                ("Dark".into(), ThemeMode::Dark),
                                ("Light".into(), ThemeMode::Light),
                            ],
                            s.theme,
                            |this, v, cx| {
                                this.settings.theme = v;
                                this.theme = match v {
                                    ThemeMode::Dark => super::theme::Theme::dark(),
                                    ThemeMode::Light => super::theme::Theme::light(),
                                    ThemeMode::System => this.theme,
                                };
                                cx.notify();
                            },
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Icons",
                        Some("Use your desktop's icon theme, or always the bundled Papirus icons"),
                        self.choice(
                            "icons",
                            vec![("Desktop theme".into(), IconTheme::System), ("Papirus".into(), IconTheme::Papirus)],
                            s.icon_theme,
                            |this, v, _| this.settings.icon_theme = v,
                            cx,
                        ),
                    ))
                    .child(self.toggle_row(
                        "menubar",
                        "Show menu bar",
                        Some("Hidden menus stay reachable with right-click"),
                        s.show_menubar,
                        |t, _| t.settings.show_menubar = !t.settings.show_menubar,
                        cx,
                    ))
                    .child(self.toggle_row(
                        "osd",
                        "On-screen messages",
                        Some("Volume, seek position, track changes…"),
                        s.osd_messages,
                        |t, _| t.settings.osd_messages = !t.settings.osd_messages,
                        cx,
                    ))
                    .child(self.row(
                        "Hide controls after",
                        Some("In fullscreen and while the mouse is idle"),
                        self.stepper(
                            "autohide",
                            format!("{:.1} s", s.controls_autohide_secs),
                            |t, d, _| {
                                t.settings.controls_autohide_secs =
                                    (t.settings.controls_autohide_secs + d as f64 * 0.5).clamp(0.5, 10.0)
                            },
                            cx,
                        ),
                    ))
                    .child(self.toggle_row(
                        "remaining",
                        "Show remaining time",
                        None,
                        s.show_remaining_time,
                        |t, _| t.settings.show_remaining_time = !t.settings.show_remaining_time,
                        cx,
                    ))
                    .child(self.section("Window"))
                    .child(self.toggle_row(
                        "fit",
                        "Resize window to fit video",
                        Some("Grows the window for larger videos; never shrinks it"),
                        s.fit_window_to_video,
                        |t, _| t.settings.fit_window_to_video = !t.settings.fit_window_to_video,
                        cx,
                    ))
                    .child(self.toggle_row(
                        "startfs",
                        "Start in fullscreen",
                        None,
                        s.start_fullscreen,
                        |t, _| t.settings.start_fullscreen = !t.settings.start_fullscreen,
                        cx,
                    ))
                    .child(self.section("Mouse"))
                    .child(self.toggle_row(
                        "clickpause",
                        "Click video to play/pause",
                        None,
                        s.single_click_pause,
                        |t, _| t.settings.single_click_pause = !t.settings.single_click_pause,
                        cx,
                    ))
                    .child(self.toggle_row(
                        "dragvideo",
                        "Drag video to move window",
                        Some("Move the player window by dragging the video area"),
                        s.drag_video_to_move_window,
                        |t, _| {
                            t.settings.drag_video_to_move_window = !t.settings.drag_video_to_move_window;
                            t.window_move_origin = None;
                        },
                        cx,
                    ))
                    .child(self.toggle_row(
                        "dblfs",
                        "Double-click for fullscreen",
                        None,
                        s.double_click_fullscreen,
                        |t, _| t.settings.double_click_fullscreen = !t.settings.double_click_fullscreen,
                        cx,
                    ))
                    .child(self.row(
                        "Mouse wheel",
                        None,
                        self.choice(
                            "wheel",
                            vec![
                                ("Change volume".into(), ScrollAction::Volume),
                                ("Seek".into(), ScrollAction::Seek),
                                ("Nothing".into(), ScrollAction::Nothing),
                            ],
                            s.scroll_action,
                            |t, v, _| t.settings.scroll_action = v,
                            cx,
                        ),
                    ));
                #[cfg(target_os = "linux")]
                if crate::integration::is_appimage() {
                    let integrated = crate::integration::is_integrated();
                    c = c.child(self.section("Desktop integration")).child(
                        self.row(
                            if integrated {
                                "MavPlayer is in your application menu"
                            } else {
                                "Add MavPlayer to your application menu"
                            },
                            Some("Creates or removes a launcher for this AppImage"),
                            self.text_button(
                                "integrate",
                                if integrated { "Remove" } else { "Integrate" },
                                !integrated,
                                move |this, _, _, cx| {
                                    let r = if integrated {
                                        crate::integration::unintegrate().map(|_| "Integration removed".to_string())
                                    } else {
                                        crate::integration::integrate().map(|p| format!("Added {}", p.display()))
                                    };
                                    match r {
                                        Ok(m) => this.osd(m, cx),
                                        Err(e) => this.osd(format!("Failed: {e}"), cx),
                                    }
                                    cx.notify();
                                },
                                cx,
                            )
                            .into_any_element(),
                        ),
                    );
                }
                c = c.child(self.bar_controls_section(cx));
            }
            1 => {
                let video = s.timeline_thumbnails;
                let audio = s.timeline_waveform;
                let shown = s.visualization_enabled(false) || s.visualization_enabled(true);
                c = c.w_full()
                    .child(self.section("Video thumbnails"))
                    .child(self.option_toggle(
                        "thumb-enabled", "Video thumbnails",
                        "Frames sampled from a video, used for the timeline preview, filmstrip and video barcodes, chapter pictures and playlist scrubbing.",
                        video, true, false, |t, _| t.settings.timeline_thumbnails = !t.settings.timeline_thumbnails, cx,
                    ))
                    .child(self.option_toggle(
                        "filmstrip-auto-video", "Create automatically for playlist items",
                        "Sample videos as they are added or probed in the playlist, without waiting for playback.",
                        s.filmstrip_auto_video, video, true, |t, _| t.settings.filmstrip_auto_video = !t.settings.filmstrip_auto_video, cx,
                    ))
                    .child(self.option_toggle(
                        "filmstrip-auto-playback", "Create automatically on playback",
                        "Sample the current video when playback starts, unless a filmstrip is already cached.",
                        s.filmstrip_auto_playback, video, true,
                        |t, _| t.settings.filmstrip_auto_playback = !t.settings.filmstrip_auto_playback, cx,
                    ))
                    .child(self.option_toggle(
                        "chapter-thumbnails", "Show in the chapter list", "Show a picture from each chapter next to its title, once the video has video thumbnails.",
                        s.chapter_thumbnails, video, true, |t, _| t.settings.chapter_thumbnails = !t.settings.chapter_thumbnails, cx,
                    ))
                    .child(self.section("Audio waveforms"))
                    .child(self.option_toggle(
                        "waveform-enabled", "Audio waveforms",
                        "Analyze how loud the audio is over time, for the waveform visualizations and the waveforms in the playlist.",
                        audio, true, false, |t, _| t.settings.timeline_waveform = !t.settings.timeline_waveform, cx,
                    ))
                    .child(self.option_toggle(
                        "waveform-auto-audio", "Analyze audio files automatically",
                        "Analyze music and other files without video when they play or are added to the playlist. Their timeline shows the waveform in place of a video visualization.",
                        s.waveform_auto_audio, audio, true, |t, _| t.settings.waveform_auto_audio = !t.settings.waveform_auto_audio, cx,
                    ))
                    .child(self.option_toggle(
                        "waveform-auto-video", "Analyze videos automatically",
                        "Analyze the soundtrack of videos when they play or are added to the playlist. A waveform visualization analyzes the video playing either way.",
                        s.waveform_auto_video, audio, true, |t, _| t.settings.waveform_auto_video = !t.settings.waveform_auto_video, cx,
                    ))
                    .child(self.section("Timeline"))
                    .child(self.option_toggle(
                        "timeline-hover-playhead", "Mark the previewed time",
                        "While hovering over the timeline, draw a line on the visualization where the previewed frame is.",
                        s.timeline_hover_playhead, shown, false, |t, _| t.settings.timeline_hover_playhead = !t.settings.timeline_hover_playhead, cx,
                    ))
                    .child(self.section("Timeline in a window"))
                    .child(self.option_toggle(
                        "thumb-hover", "Preview on hover", "Show the nearest video thumbnail above the timeline while hovering over it.",
                        s.timeline_hover_enabled(false), video, false,
                        |t, _| t.settings.set_timeline_hover(false, !t.settings.timeline_hover_enabled(false)), cx,
                    ))
                    .child(self.timeline_visualization_row(false, cx))
                    .child(self.visualization_number_row(false, false, cx))
                    .child(self.thumbnail_height_row(false, cx))
                    .child(self.option_toggle(
                        "thumb-adaptive", "Adapt height to window width", "Make the visualization lower in narrow windows.",
                        s.timeline_adaptive, s.visualization_enabled(false), false,
                        |t, _| t.settings.timeline_adaptive = !t.settings.timeline_adaptive, cx,
                    ))
                    .child(self.thumbnail_height_row(true, cx))
                    .child(self.section("Timeline in fullscreen"))
                    .child(self.option_choice("thumb-fullscreen-hover", "Preview on hover", "Show the nearest video thumbnail above the timeline while hovering over it.",
                        vec![("Same as in a window".into(), None), ("On".into(), Some(true)), ("Off".into(), Some(false))],
                        s.timeline_fullscreen_hover, video, |t, v, _| t.settings.timeline_fullscreen_hover = v, cx))
                    .child(self.timeline_visualization_row(true, cx))
                    .child(self.visualization_number_row(true, false, cx))
                    .child(self.visualization_number_row(true, true, cx))
                    .child(self.section("Thumbnail and waveform cache"))
                    .child(self.row(
                        "Cache directory",
                        Some(&s.thumbnail_cache_dir().display().to_string()),
                        div().flex().gap_2()
                            .child(self.text_button("thumbdir-reset", "Restore default", false, |t, _, _, cx| {
                                t.settings.thumbnail_cache_dir = None;
                                t.apply_settings(cx);
                            }, cx))
                            .child(self.text_button("thumbdir", "Choose…", false, |_, _, w, cx| {
                                let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                                    files: false,
                                    directories: true,
                                    multiple: false,
                                    prompt: Some("Choose".into()),
                                });
                                cx.spawn_in(w, async move |this, cx| {
                                    if let Ok(Ok(Some(p))) = rx.await
                                        && let Some(p) = p.into_iter().next()
                                    {
                                        let _ = this.update(cx, |v, cx| {
                                            v.settings.thumbnail_cache_dir = Some(p);
                                            v.apply_settings(cx);
                                        });
                                    }
                                }).detach();
                            }, cx))
                            .into_any_element(),
                    ))
                    .child(self.row(
                        "Keep unused thumbnails for", None,
                        self.choice("thumb-days", [("7 days", 7), ("14 days", 14), ("30 days", 30), ("90 days", 90),
                            ("180 days", 180), ("1 year", 365), ("Forever", 0)].into_iter()
                            .map(|(label, value)| (label.into(), value)).collect(), s.thumbnail_cache_days,
                            |t, v, _| t.settings.thumbnail_cache_days = v, cx),
                    ))
                    .child(self.row(
                        "Maximum cache size", None,
                        self.choice("thumb-mb", [("256 MB", 256), ("512 MB", 512), ("1 GB", 1024), ("2 GB", 2048),
                            ("5 GB", 5120), ("10 GB", 10240), ("Unlimited", 0)].into_iter()
                            .map(|(label, value)| (label.into(), value)).collect(), s.thumbnail_cache_mb,
                            |t, v, _| t.settings.thumbnail_cache_mb = v, cx),
                    ))
                    .child(self.row(
                        "Current cache size", Some(&self.thumbnails.cache_label()),
                        self.text_button("thumb-clear", "Clear cache", false, |t, _, _, cx| {
                            t.playlist_previews.clear();
                            t.thumbnails.clear_cache(&t.settings);
                            cx.notify();
                        }, cx).into_any_element(),
                    ));
            }
            2 => {
                use crate::settings::PlaylistThumbnails;
                let video = s.timeline_thumbnails;
                let filmstrips_auto = video && s.filmstrip_auto_video;
                let inline = s.playlist_thumbnails.inline();
                let playlist_desc = match s.playlist_thumbnails {
                    PlaylistThumbnails::None => "Show only an icon for each item.",
                    PlaylistThumbnails::StaticInline => "Show one picture from each video next to its title.",
                    PlaylistThumbnails::Inline => {
                        "Show a picture next to each title. Moving the pointer across a row scrubs through its video thumbnails."
                    }
                    PlaylistThumbnails::InlineSide => {
                        "Show a picture next to each title, and a larger preview beside the playlist while scrubbing across a row."
                    }
                    PlaylistThumbnails::Hover => {
                        "Show a large preview beside the playlist while scrubbing across a row."
                    }
                };
                let playlist_desc = if s.playlist_thumbnails.scrubbing() && !video {
                    format!("{playlist_desc} Scrubbing needs video thumbnails, which are turned off.")
                } else if s.playlist_thumbnails.scrubbing() {
                    format!("{playlist_desc} Scrubbing works on videos that have video thumbnails.")
                } else {
                    playlist_desc.to_string()
                };
                let representative_desc = if !inline {
                    "Only used when the playlist shows a picture next to each title."
                } else if filmstrips_auto {
                    "Already covered: video thumbnails are created automatically and include this picture."
                } else {
                    "Create one picture for each video added to the playlist, unless one is already cached."
                };
                c = c
                    .child(self.section("Windowed"))
                    .child(self.toggle_row(
                        "playlist-window-visible", "Show playlist", None, self.show_playlist,
                        |t, _| {
                            t.show_playlist = !t.show_playlist;
                            t.settings.set_playlist_visibility(false, t.show_playlist);
                            t.playlist_autohide_open = false;
                        }, cx,
                    ))
                    .child(self.toggle_row(
                        "playlist-window-autohide", "Autohide playlist", Some("Reveal the enabled playlist at the right edge."),
                        s.playlist_autohide, |t, _| {
                            t.settings.set_playlist_autohide_for(false, !t.settings.playlist_autohide);
                            t.playlist_autohide_open = false;
                        }, cx,
                    ))
                    .child(self.section("Fullscreen"))
                    .child(self.toggle_row(
                        "playlist-fs-visible", "Show playlist", None,
                        s.fullscreen_playlist_initially_visible(),
                        |t, _| {
                            let visible = !t.settings.fullscreen_playlist_initially_visible();
                            t.settings.set_playlist_visibility(true, visible);
                            t.fs_playlist = visible;
                            t.playlist_autohide_open = false;
                        }, cx,
                    ))
                    .child(self.toggle_row(
                        "playlist-fs-autohide", "Autohide playlist", Some("Reveal the enabled playlist at the right edge in fullscreen."),
                        s.playlist_autohide_for(true), |t, _| {
                            let enabled = t.settings.playlist_autohide_for(true);
                            t.settings.set_playlist_autohide_for(true, !enabled);
                            t.playlist_autohide_open = false;
                        }, cx,
                    ))
                    .child(self.section("Playlist labels"))
                    .child(self.option_toggle(
                        "playlist-compact-title", "Prefer media title without inline thumbnails",
                        "Use the custom or embedded title instead of the file name when available.",
                        s.playlist_compact_prefer_title, true, false,
                        |t, _| t.settings.playlist_compact_prefer_title = !t.settings.playlist_compact_prefer_title, cx,
                    ))
                    .child(self.option_choice(
                        "playlist-inline-labels", "Labels with inline thumbnails", "Show the title, file name or both.",
                        vec![
                            ("Title".into(), crate::settings::PlaylistLabels::Title),
                            ("File name".into(), crate::settings::PlaylistLabels::FileName),
                            ("Title and file name".into(), crate::settings::PlaylistLabels::Both),
                        ],
                        s.playlist_inline_labels, true, |t, v, _| t.settings.playlist_inline_labels = v, cx,
                    ))
                    .child(self.section("Thumbnail display"))
                    .child(self.playlist_mode_cards(PlaylistThumbnails::ALL.len(), cx))
                    .child(div().text_size(px(12.)).text_color(self.theme.text_dim).child(playlist_desc))
                    .child(self.option_toggle(
                        "playlist-thumb-auto-video", "Create pictures automatically", representative_desc,
                        s.playlist_thumbnail_auto_video || (inline && filmstrips_auto), inline && !filmstrips_auto, true,
                        |t, _| t.settings.playlist_thumbnail_auto_video = !t.settings.playlist_thumbnail_auto_video, cx,
                    ));
            }
            3 => {
                c = c
                    .child(self.section("Playback"))
                    .child(self.toggle_row(
                        "resume",
                        "Resume where you left off",
                        Some("Remembers the position of files you didn't finish"),
                        s.resume_playback,
                        |t, _| t.settings.resume_playback = !t.settings.resume_playback,
                        cx,
                    ))
                    .child(self.toggle_row(
                        "startpaused",
                        "Start files paused",
                        Some("Opened files wait for Play instead of starting right away"),
                        s.start_paused,
                        |t, _| t.settings.start_paused = !t.settings.start_paused,
                        cx,
                    ))
                    .child(self.toggle_row(
                        "autonext",
                        "Play next item automatically",
                        None,
                        s.auto_play_next,
                        |t, _| t.settings.auto_play_next = !t.settings.auto_play_next,
                        cx,
                    ))
                    .child(self.row(
                        "Opening files",
                        None,
                        self.choice(
                            "openmode",
                            vec![
                                ("Replaces the playlist".into(), OpenMode::Replace),
                                ("Adds to the playlist".into(), OpenMode::Append),
                            ],
                            s.open_mode,
                            |t, v, _| t.settings.open_mode = v,
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "When MavPlayer is already open",
                        Some("Opening files from a file manager or the command line"),
                        self.choice(
                            "whenrunning",
                            vec![
                                ("Open a new window".into(), RunningMode::NewWindow),
                                ("Add to playlist and play".into(), RunningMode::PlayNow),
                                ("Add to playlist".into(), RunningMode::Enqueue),
                                ("Replace playlist and play".into(), RunningMode::Replace),
                            ],
                            s.when_running,
                            |t, v, _| t.settings.when_running = v,
                            cx,
                        ),
                    ))
                    .child(self.toggle_row(
                        "tray",
                        "Keep running in the system tray",
                        Some("Closing the window leaves MavPlayer in the tray, so files open almost instantly. Quit from the tray icon's menu"),
                        s.keep_in_tray,
                        |t, cx| {
                            t.settings.keep_in_tray = !t.settings.keep_in_tray;
                            super::shell::set_tray(&t.settings, t.settings.keep_in_tray, cx);
                        },
                        cx,
                    ))
                    .child(self.toggle_row(
                        "trayplay",
                        "Play in the background",
                        Some("Closing the window while a file is open hides it in the system tray, and playback goes on. The tray icon's menu then has Play/Pause, Previous and Next"),
                        s.tray_playback,
                        |t, cx| {
                            t.settings.tray_playback = !t.settings.tray_playback;
                            super::shell::set_tray(&t.settings, t.settings.tray_playback, cx);
                        },
                        cx,
                    ))
                    .child(self.row(
                        "While dragging the progress bar",
                        None,
                        self.choice(
                            "seekdrag",
                            vec![
                                ("Keep playing".into(), SeekDragMode::KeepPlaying),
                                ("Pause until released".into(), SeekDragMode::Pause),
                            ],
                            s.seek_drag,
                            |t, v, _| t.settings.seek_drag = v,
                            cx,
                        ),
                    ))
                    .child(self.toggle_row(
                        "exact",
                        "Exact seeking",
                        Some("Land on the exact frame instead of the nearest keyframe"),
                        s.exact_seeks,
                        |t, _| t.settings.exact_seeks = !t.settings.exact_seeks,
                        cx,
                    ))
                    .child(self.section("Steps"))
                    .child(self.row(
                        "Short seek",
                        Some("Left / Right"),
                        self.stepper(
                            "seek",
                            format!("{} s", s.seek_step),
                            |t, d, _| t.settings.seek_step = (t.settings.seek_step + d as f64).clamp(1.0, 60.0),
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Long seek",
                        Some("Ctrl + Left / Right"),
                        self.stepper(
                            "seekl",
                            format!("{} s", s.seek_step_large),
                            |t, d, _| {
                                t.settings.seek_step_large =
                                    (t.settings.seek_step_large + d as f64 * 10.0).clamp(10.0, 600.0)
                            },
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Volume step",
                        None,
                        self.stepper(
                            "vstep",
                            format!("{} %", s.volume_step),
                            |t, d, _| t.settings.volume_step = (t.settings.volume_step + d as f64).clamp(1.0, 20.0),
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Maximum volume",
                        Some("Above 100 % amplifies quiet media"),
                        self.stepper(
                            "vmax",
                            format!("{} %", s.max_volume),
                            |t, d, _| {
                                t.settings.max_volume = (t.settings.max_volume as i32 + d * 10).clamp(100, 150) as u32
                            },
                            cx,
                        ),
                    ))
                    .child(self.section("Screenshots"))
                    .child(
                        self.row(
                            "Save screenshots to",
                            Some(&s.screenshot_dir().display().to_string()),
                            self.text_button(
                                "shotdir",
                                "Choose…",
                                false,
                                |_this, _, w, cx| {
                                    let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                                        files: false,
                                        directories: true,
                                        multiple: false,
                                        prompt: Some("Choose".into()),
                                    });
                                    cx.spawn_in(w, async move |this, cx| {
                                        if let Ok(Ok(Some(p))) = rx.await
                                            && let Some(p) = p.into_iter().next()
                                        {
                                            let _ = this.update(cx, |v, cx| {
                                                v.settings.screenshot_dir = Some(p);
                                                v.apply_settings(cx);
                                            });
                                        }
                                    })
                                    .detach();
                                },
                                cx,
                            )
                            .into_any_element(),
                        ),
                    );
            }
            4 => {
                c = c
                    .child(self.section("Decoding"))
                    .child(self.row("Hardware decoding", Some("Uses the GPU's video decoder (VA-API, NVDEC, D3D11, VideoToolbox); falls back to software automatically"), self.choice("hwdec", vec![("Automatic (above 1080p)".into(), HwDecode::Auto), ("Always".into(), HwDecode::Always), ("Off".into(), HwDecode::Off)], s.hwdec, |t, v, cx| {
                        t.settings.hwdec = v;
                        t.apply_settings(cx);
                        t.reload_current(cx);
                    }, cx)))
                    .child(self.row("Deinterlacing", Some("Automatic only processes interlaced material (DVDs, TV)"), self.choice("deint", vec![("Automatic".into(), Deinterlace::Auto), ("Always".into(), Deinterlace::On), ("Off".into(), Deinterlace::Off)], s.deinterlace, |t, v, _| t.settings.deinterlace = v, cx)))
                    .child(self.toggle_row("tonemap", "HDR tone mapping", Some("Converts HDR10/HLG to standard range so it doesn't look washed out"), s.tone_map_hdr, |t, _| t.settings.tone_map_hdr = !t.settings.tone_map_hdr, cx))
                    .child(self.section("Scaling"))
                    .child(self.row("Downscaling", Some("For pictures larger than the window"), self.choice("scale", vec![("Nearest neighbor".into(), ScaleQuality::Nearest), ("Linear".into(), ScaleQuality::Linear), ("Good (bicubic)".into(), ScaleQuality::Good), ("Best (Lanczos)".into(), ScaleQuality::Best)], s.scale_quality, |t, v, _| t.settings.scale_quality = v, cx)))
                    .children(self.upscaling_rows(cx));
            }
            5 => {
                let mut devs: Vec<(String, Option<String>)> = vec![("System default".into(), None)];
                devs.extend(self.audio_devices.iter().map(|d| (d.clone(), Some(d.clone()))));
                c = c
                    .child(self.section("Output"))
                    .child(self.row(
                        "Audio device",
                        None,
                        self.choice(
                            "adev",
                            devs,
                            s.audio_device.clone(),
                            |t, v, cx| {
                                t.settings.audio_device = v;
                                t.apply_settings(cx);
                                t.reload_current(cx);
                            },
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Channels",
                        Some("Auto sends surround sound to surround devices and downmixes otherwise"),
                        self.choice(
                            "chan",
                            vec![
                                ("Auto".into(), ChannelMode::Auto),
                                ("Stereo".into(), ChannelMode::Stereo),
                                ("Mono".into(), ChannelMode::Mono),
                                ("5.1".into(), ChannelMode::Surround51),
                                ("7.1".into(), ChannelMode::Surround71),
                            ],
                            s.channels,
                            |t, v, cx| {
                                t.settings.channels = v;
                                t.apply_settings(cx);
                                t.reload_current(cx);
                            },
                            cx,
                        ),
                    ))
                    .child(self.section("Tracks"))
                    .child(self.row(
                        "Preferred audio languages",
                        Some("Comma separated, most preferred first"),
                        self.input_box("audio_langs"),
                    ))
                    .child(self.section("Processing"))
                    .child(self.toggle_row(
                        "norm",
                        "Normalize volume",
                        Some("Evens out loud and quiet passages"),
                        s.audio_fx.normalize,
                        |t, _| {
                            t.settings.audio_fx.normalize = !t.settings.audio_fx.normalize;
                            t.player.set_audio_fx(t.settings.audio_fx.clone());
                        },
                        cx,
                    ));
            }
            6 => {
                c = c
                    .child(self.section("Tracks"))
                    .child(self.row("Preferred subtitle languages", Some(&sub_langs_hint), self.input_box("sub_langs")))
                    .child(self.toggle_row(
                        "subdef",
                        "Show subtitles",
                        Some("Remembered for later videos and launches. The subtitle button and menus change it too"),
                        s.show_subtitles,
                        |t, _| t.set_show_subtitles(!t.settings.show_subtitles),
                        cx,
                    ))
                    .child(self.toggle_row(
                        "subauto",
                        "Load matching subtitle files",
                        Some("movie.srt, movie.en.srt, Subs/movie.srt …"),
                        s.auto_load_subs,
                        |t, _| t.settings.auto_load_subs = !t.settings.auto_load_subs,
                        cx,
                    ))
                    .child(self.row(
                        "Text encoding",
                        Some("For old subtitle files that aren't UTF-8"),
                        self.input_box("sub_charset"),
                    ))
                    .child(self.section("Appearance"))
                    .child(self.row(
                        "Style",
                        None,
                        self.choice(
                            "substyle",
                            vec![
                                ("Outlined text".into(), SubStyle::Outline),
                                ("Text on dark box".into(), SubStyle::Box),
                            ],
                            s.sub_style,
                            |t, v, _| t.settings.sub_style = v,
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Font family",
                        Some("Leave empty for the default font; unknown names are not saved"),
                        {
                            let entered =
                                self.inputs.get("sub_font_family").map(|i| i.read(cx).text()).unwrap_or_default();
                            let candidate = entered.trim();
                            let status = if candidate.is_empty() {
                                let source = super::render::default_subtitle_font_source(cx);
                                Some((
                                    match source {
                                        super::render::DefaultFontSource::System => {
                                            format!("Default: {DEFAULT_SUB_FONT_FAMILY}")
                                        }
                                        super::render::DefaultFontSource::Bundled => {
                                            format!("Default: {DEFAULT_SUB_FONT_FAMILY} (using bundled {BUNDLED_SUB_FONT_FAMILY})")
                                        }
                                        super::render::DefaultFontSource::Unavailable => {
                                            format!("Default: {DEFAULT_SUB_FONT_FAMILY} (font unavailable)")
                                        }
                                    },
                                    source != super::render::DefaultFontSource::System,
                                ))
                            } else if super::render::subtitle_available_weights(candidate).is_empty() {
                                let replacement = if super::render::subtitle_available_weights("").is_empty() {
                                    format!("bundled {BUNDLED_SUB_FONT_FAMILY}")
                                } else {
                                    DEFAULT_SUB_FONT_FAMILY.to_owned()
                                };
                                Some((format!("Font not available (using {replacement})"), true))
                            } else {
                                None
                            };
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(self.font_family_picker(cx))
                                .when_some(status, |d, (message, warning)| {
                                    d.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(if warning { self.theme.danger } else { self.theme.text_dim })
                                            .child(message),
                                    )
                                })
                                .into_any_element()
                        },
                    ))
                    .child(
                        self.row(
                            "Font weight",
                            Some("Only the weights supported by the selected font are offered"),
                            self.choice(
                                "subweight",
                                {
                                    let weights = super::render::subtitle_available_weights(&s.sub_font_family);
                                    // A variable font may expose hundreds of valid numeric
                                    // weights. Offer standard steps and the current
                                    // value so the menu remains manageable.
                                    if weights.len() > 40 {
                                        weights
                                            .into_iter()
                                            .filter(|w| w % 100 == 0 || *w == s.sub_font_weight)
                                            .collect::<Vec<_>>()
                                    } else {
                                        weights
                                    }
                                }
                                .into_iter()
                                .map(|weight| {
                                    let name = match weight {
                                        100 => "Thin",
                                        200 => "Extra Light",
                                        300 => "Light",
                                        400 => "Regular",
                                        500 => "Medium",
                                        600 => "Semibold",
                                        700 => "Bold",
                                        800 => "Extra Bold",
                                        900 => "Black",
                                        _ => "Weight",
                                    };
                                    (format!("{name} ({weight})"), weight)
                                })
                                .collect(),
                                s.sub_font_weight,
                                |this, weight, _| this.settings.sub_font_weight = weight,
                                cx,
                            ),
                        ),
                    )
                    .child(self.row(
                        "Size",
                        None,
                        self.stepper(
                            "subsize",
                            format!("{:.0} %", s.sub_scale * 100.0),
                            |t, d, _| t.settings.sub_scale = (t.settings.sub_scale + d as f32 * 0.1).clamp(0.3, 3.0),
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Subtitle position",
                        Some("Automatic uses the black bar when possible"),
                        self.choice(
                            "subposition",
                            vec![
                                ("Automatic".into(), SubPosition::Automatic),
                                ("Video picture".into(), SubPosition::VideoPicture),
                                ("Player window".into(), SubPosition::PlayerWindow),
                            ],
                            s.sub_position,
                            |t, v, _| t.settings.sub_position = v,
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Distance from bottom",
                        Some("Percentage of video picture height"),
                        self.stepper(
                            "submargin",
                            format!("{:.0} %", s.sub_margin * 100.0),
                            |t, d, _| t.settings.sub_margin = (t.settings.sub_margin + d as f32 * 0.01).clamp(0.0, 0.4),
                            cx,
                        ),
                    ));
            }
            8 => {
                c = c
                    .child(self.section("Network"))
                    .child(self.row(
                        "Read-ahead cache",
                        Some("How much of a stream to buffer"),
                        self.stepper(
                            "cache",
                            format!("{:.0} s", s.network_cache_secs),
                            |t, d, _| {
                                t.settings.network_cache_secs =
                                    (t.settings.network_cache_secs + d as f64 * 5.0).clamp(2.0, 300.0)
                            },
                            cx,
                        ),
                    ))
                    .child(self.section("DVD"))
                    .child(
                        self.row(
                            "Player region",
                            Some("Only needed for region-locked discs"),
                            self.choice(
                                "region",
                                (0..=8)
                                    .map(|r| (if r == 0 { "Automatic".to_string() } else { format!("Region {r}") }, r))
                                    .collect(),
                                s.dvd_region,
                                |t, v, _| t.settings.dvd_region = v,
                                cx,
                            ),
                        ),
                    );
            }
            _ => {
                c = c
                    .child(self.section("Performance"))
                    .child(self.row(
                        "Decoder threads",
                        Some("0 = automatic"),
                        self.stepper(
                            "threads",
                            if s.decoder_threads == 0 { "auto".into() } else { s.decoder_threads.to_string() },
                            |t, d, _| {
                                t.settings.decoder_threads = (t.settings.decoder_threads as i32 + d).clamp(0, 64) as u32
                            },
                            cx,
                        ),
                    ))
                    .child(self.section("Data"))
                    .child(
                        self.row(
                            "Configuration folder",
                            Some(&crate::settings::config_dir().display().to_string()),
                            self.text_button(
                                "opencfg",
                                "Open",
                                false,
                                |_, _, _, cx| {
                                    let d = crate::settings::config_dir();
                                    let _ = std::fs::create_dir_all(&d);
                                    cx.open_with_system(&d);
                                },
                                cx,
                            )
                            .into_any_element(),
                        ),
                    )
                    .child(
                        self.row(
                            "Forget resume positions",
                            None,
                            self.text_button(
                                "clrresume",
                                "Clear",
                                false,
                                |this, _, _, cx| {
                                    this.resume.clear();
                                    this.resume.save();
                                    this.osd("Resume positions cleared", cx);
                                },
                                cx,
                            )
                            .into_any_element(),
                        ),
                    )
                    .child(
                        self.row(
                            "Reset all preferences",
                            None,
                            self.text_button(
                                "reset",
                                "Reset",
                                false,
                                |this, _, w, cx| {
                                    let recent = std::mem::take(&mut this.settings.recent);
                                    this.settings = crate::settings::Settings { recent, ..Default::default() };
                                    this.show_playlist = this.settings.show_playlist;
                                    this.fs_playlist = this.settings.fullscreen_playlist_initially_visible();
                                    this.playlist_autohide_open = false;
                                    this.theme = super::theme::Theme::resolve(this.settings.theme, w);
                                    this.apply_settings(cx);
                                    this.osd("Preferences reset", cx);
                                },
                                cx,
                            )
                            .into_any_element(),
                        ),
                    );
            }
        }
        self.tabbed_modal("Preferences", 820., 520., (SETTINGS_TABS, tab, Dialog::Settings), c, cx)
    }

    /// Preferences → Interface → "Control bar": which controls the bottom
    /// bar shows, and in what order.
    fn bar_controls_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let enabled = &self.settings.bar_controls;
        let mut col =
            div().flex().flex_col().child(self.section("Control bar")).child(
                div().pb_1().text_size(px(11.)).text_color(t.text_dim).child(
                    "Play/Pause is always shown. A window too narrow for all of them hides some until it is wider.",
                ),
            );
        let small_btn = |id: String, i: Icon, active: bool| {
            div()
                .id(SharedString::from(id))
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .when(active, |d| d.cursor_pointer().hover(|s| s.bg(t.hover)))
                .child(icon(i, px(14.), if active { t.text } else { t.text_disabled }))
        };
        for left in [true, false] {
            col = col.child(div().pt_2().text_size(px(12.)).text_color(t.text_dim).child(if left {
                "Left of the title"
            } else {
                "Right of the title"
            }));
            // Shown ones in their order, then the hidden ones.
            let on: Vec<BarControl> = enabled.iter().copied().filter(|c| c.on_left() == left).collect();
            let off = BarControl::ALL.into_iter().filter(|c| c.on_left() == left && !on.contains(c));
            let n = on.len();
            let items: Vec<(BarControl, bool)> = on.iter().map(|&c| (c, true)).chain(off.map(|c| (c, false))).collect();
            for (i, (c, shown)) in items.into_iter().enumerate() {
                let (up, down) = (shown && i > 0, shown && i + 1 < n);
                let id = format!("{c:?}");
                let control = div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(small_btn(format!("bar-up-{id}"), Icon::Up, up).when(up, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            this.move_bar_control(c, -1);
                            this.apply_settings(cx);
                        }))
                    }))
                    .child(small_btn(format!("bar-down-{id}"), Icon::Down, down).when(down, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            this.move_bar_control(c, 1);
                            this.apply_settings(cx);
                        }))
                    }))
                    .child(div().w(px(8.)))
                    .child(self.toggle(format!("tg-bar-{id}"), shown, move |this, _| this.toggle_bar_control(c), cx))
                    .into_any_element();
                col = col.child(self.row(c.label(), None, control));
            }
        }
        col.into_any_element()
    }

    /// Shows or hides `c` in the bottom bar. A control that comes back goes
    /// to its usual place among the others.
    fn toggle_bar_control(&mut self, c: BarControl) {
        let list = &mut self.settings.bar_controls;
        if let Some(i) = list.iter().position(|&x| x == c) {
            list.remove(i);
        } else {
            let rank = |x: BarControl| BarControl::ALL.iter().position(|&a| a == x).unwrap_or(usize::MAX);
            let at = list.iter().position(|&x| rank(x) > rank(c)).unwrap_or(list.len());
            list.insert(at, c);
        }
    }

    /// Moves `c` one place earlier (`-1`) or later (`1`) among the shown
    /// controls on its side of the title.
    fn move_bar_control(&mut self, c: BarControl, dir: i32) {
        let list = &mut self.settings.bar_controls;
        let Some(i) = list.iter().position(|&x| x == c) else { return };
        let same_side = |x: &BarControl| x.on_left() == c.on_left();
        let j = if dir < 0 {
            list[..i].iter().rposition(same_side)
        } else {
            list[i + 1..].iter().position(same_side).map(|k| i + 1 + k)
        };
        if let Some(j) = j {
            list.swap(i, j);
        }
    }

    fn effects_dialog(&mut self, tab: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let fx = self.settings.audio_fx.clone();
        let mut c = div().flex_1().min_w_0().px_5().pb_4().flex().flex_col();
        let set_fx = |this: &mut Self, f: &dyn Fn(&mut AudioFx), cx: &mut Context<Self>| {
            f(&mut this.settings.audio_fx);
            this.player.set_audio_fx(this.settings.audio_fx.clone());
            cx.notify();
        };
        let _ = set_fx;
        match tab {
            0 => {
                let current_preset =
                    EQ_PRESETS.iter().position(|(_, g)| *g == fx.eq_db).map(|i| i as i32).unwrap_or(-1);
                let mut presets: Vec<(String, i32)> =
                    EQ_PRESETS.iter().enumerate().map(|(i, (n, _))| (n.to_string(), i as i32)).collect();
                if current_preset < 0 {
                    presets.insert(0, ("Custom".into(), -1));
                }
                let mut bands = div().flex().gap_1().h(px(220.)).mt_3();
                for (i, f) in EQ_BANDS.iter().enumerate() {
                    let g = fx.eq_db[i];
                    let label = if *f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
                    bands = bands.child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_1()
                            .child(div().text_size(px(10.)).text_color(t.text_dim).child(format!("{g:+.0}")))
                            .child(
                                div().flex_1().w_full().flex().flex_col().items_center().child(
                                    self.slider(
                                        SharedString::from(format!("eq-{i}")),
                                        (g + 12.0) / 24.0,
                                        true,
                                        t,
                                        move |this, v, _, cx| {
                                            this.settings.audio_fx.eq_db[i] = ((v * 24.0 - 12.0) * 2.0).round() / 2.0;
                                            this.settings.audio_fx.eq_enabled = true;
                                            this.player.set_audio_fx(this.settings.audio_fx.clone());
                                            cx.notify();
                                        },
                                        cx,
                                    )
                                    .h_auto()
                                    .flex_1(),
                                ),
                            )
                            .child(div().text_size(px(10.)).text_color(t.text_dim).child(label)),
                    );
                }
                c = c
                    .child(self.toggle_row(
                        "eqon",
                        "Enable equalizer",
                        None,
                        fx.eq_enabled,
                        |this, _| {
                            this.settings.audio_fx.eq_enabled = !this.settings.audio_fx.eq_enabled;
                            this.player.set_audio_fx(this.settings.audio_fx.clone());
                        },
                        cx,
                    ))
                    .child(self.row(
                        "Preset",
                        None,
                        self.choice(
                            "eqpreset",
                            presets,
                            current_preset,
                            |this, v, _| {
                                if v >= 0 {
                                    this.settings.audio_fx.eq_db = EQ_PRESETS[v as usize].1;
                                    this.settings.audio_fx.eq_enabled = true;
                                    this.player.set_audio_fx(this.settings.audio_fx.clone());
                                }
                            },
                            cx,
                        ),
                    ))
                    .child(
                        self.row(
                            &format!("Preamp  {:+.1} dB", fx.preamp_db),
                            None,
                            div()
                                .w(px(220.))
                                .child(self.slider(
                                    "preamp",
                                    (fx.preamp_db + 12.0) / 24.0,
                                    false,
                                    t,
                                    |this, v, _, cx| {
                                        this.settings.audio_fx.preamp_db = ((v * 24.0 - 12.0) * 2.0).round() / 2.0;
                                        this.player.set_audio_fx(this.settings.audio_fx.clone());
                                        cx.notify();
                                    },
                                    cx,
                                ))
                                .into_any_element(),
                        ),
                    )
                    .child(bands);
            }
            1 => {
                c = c
                    .child(self.section("Dynamics"))
                    .child(self.toggle_row(
                        "fxnorm",
                        "Normalize volume",
                        Some("Smooths out loudness differences"),
                        fx.normalize,
                        |this, _| {
                            this.settings.audio_fx.normalize = !this.settings.audio_fx.normalize;
                            this.player.set_audio_fx(this.settings.audio_fx.clone());
                        },
                        cx,
                    ))
                    .child(self.toggle_row(
                        "fxnight",
                        "Night mode",
                        Some("Compresses loud effects so dialogue stays audible"),
                        fx.night_mode,
                        |this, _| {
                            this.settings.audio_fx.night_mode = !this.settings.audio_fx.night_mode;
                            this.player.set_audio_fx(this.settings.audio_fx.clone());
                        },
                        cx,
                    ))
                    .child(self.section("Stereo"))
                    .child(self.toggle_row(
                        "fxwide",
                        "Widen stereo",
                        None,
                        fx.stereo_widen,
                        |this, _| {
                            this.settings.audio_fx.stereo_widen = !this.settings.audio_fx.stereo_widen;
                            this.player.set_audio_fx(this.settings.audio_fx.clone());
                        },
                        cx,
                    ))
                    .child(self.toggle_row(
                        "fxswap",
                        "Swap left and right",
                        None,
                        fx.swap_channels,
                        |this, _| {
                            this.settings.audio_fx.swap_channels = !this.settings.audio_fx.swap_channels;
                            this.player.set_audio_fx(this.settings.audio_fx.clone());
                        },
                        cx,
                    ))
                    .child(self.toggle_row(
                        "fxkaraoke",
                        "Karaoke (reduce centred vocals)",
                        None,
                        fx.karaoke,
                        |this, _| {
                            this.settings.audio_fx.karaoke = !this.settings.audio_fx.karaoke;
                            this.player.set_audio_fx(this.settings.audio_fx.clone());
                        },
                        cx,
                    ));
            }
            2 => {
                let a = self.video_adjust.clone();
                let adj_slider = |this: &Self,
                                  id: &'static str,
                                  label: &str,
                                  value: f32,
                                  lo: f32,
                                  hi: f32,
                                  set: fn(&mut VideoAdjust, f32),
                                  cx: &mut Context<Self>| {
                    this.row(
                        &format!("{label}  {value:.2}"),
                        None,
                        div()
                            .w(px(260.))
                            .child(this.slider(
                                id,
                                (value - lo) / (hi - lo),
                                false,
                                this.theme,
                                move |this, v, _, cx| {
                                    let mut adj = this.video_adjust.clone();
                                    set(&mut adj, ((lo + v * (hi - lo)) * 100.0).round() / 100.0);
                                    this.set_video_adjust(adj, cx);
                                },
                                cx,
                            ))
                            .into_any_element(),
                    )
                };
                c = c
                    .child(self.section("Colour"))
                    .child(adj_slider(
                        self,
                        "brightness",
                        "Brightness",
                        a.brightness,
                        -1.0,
                        1.0,
                        |a, v| a.brightness = v,
                        cx,
                    ))
                    .child(adj_slider(self, "contrast", "Contrast", a.contrast, 0.0, 2.0, |a, v| a.contrast = v, cx))
                    .child(adj_slider(
                        self,
                        "saturation",
                        "Saturation",
                        a.saturation,
                        0.0,
                        3.0,
                        |a, v| a.saturation = v,
                        cx,
                    ))
                    .child(adj_slider(self, "gamma", "Gamma", a.gamma, 0.1, 3.0, |a, v| a.gamma = v, cx))
                    .child(adj_slider(self, "hue", "Hue (°)", a.hue, -180.0, 180.0, |a, v| a.hue = v, cx))
                    .child(self.section("Enhancement"))
                    .child(adj_slider(self, "sharpen", "Sharpen", a.sharpen, 0.0, 2.0, |a, v| a.sharpen = v, cx))
                    .child(self.toggle_row(
                        "denoise",
                        "Reduce noise",
                        None,
                        a.denoise,
                        |this, cx| {
                            let mut adj = this.video_adjust.clone();
                            adj.denoise = !adj.denoise;
                            this.set_video_adjust(adj, cx)
                        },
                        cx,
                    ))
                    .child(self.toggle_row(
                        "gray",
                        "Grayscale",
                        None,
                        a.grayscale,
                        |this, cx| {
                            let mut adj = this.video_adjust.clone();
                            adj.grayscale = !adj.grayscale;
                            this.set_video_adjust(adj, cx)
                        },
                        cx,
                    ))
                    .child(self.toggle_row(
                        "negate",
                        "Invert colours",
                        None,
                        a.negate,
                        |this, cx| {
                            let mut adj = this.video_adjust.clone();
                            adj.negate = !adj.negate;
                            this.set_video_adjust(adj, cx)
                        },
                        cx,
                    ))
                    .child(div().pt_3().child(self.text_button(
                        "adj-reset",
                        "Reset picture",
                        false,
                        |this, _, _, cx| {
                            let keep = this.video_adjust.clone();
                            this.set_video_adjust(
                                VideoAdjust {
                                    rotation: keep.rotation,
                                    hflip: keep.hflip,
                                    vflip: keep.vflip,
                                    crop: keep.crop,
                                    ..Default::default()
                                },
                                cx,
                            );
                        },
                        cx,
                    )));
            }
            _ => {
                let ad = self.player.audio_delay();
                let sd = self.sub_delay;
                let sp = self.player.speed();
                c = c
                    .child(self.section("Synchronization"))
                    .child(self.row(
                        "Audio delay",
                        Some("Positive = audio later"),
                        self.stepper(
                            "adelay",
                            format!("{:+.0} ms", ad * 1000.0),
                            |this, d, _| {
                                let v = this.player.audio_delay() + d as f64 * 0.05;
                                this.player.set_audio_delay(v);
                            },
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Subtitle delay",
                        Some("Positive = subtitles later"),
                        self.stepper(
                            "sdelay",
                            format!("{:+.0} ms", sd * 1000.0),
                            |this, d, _| this.sub_delay += d as f64 * 0.05,
                            cx,
                        ),
                    ))
                    .child(self.row(
                        "Playback speed",
                        Some("Pitch is preserved"),
                        self.stepper(
                            "speed",
                            format!("{sp:.2}×"),
                            |this, d, _| {
                                let v = ((this.player.speed() + d as f64 * 0.05) * 100.0).round() / 100.0;
                                this.player.set_speed(v);
                            },
                            cx,
                        ),
                    ));
            }
        }
        self.tabbed_modal("Effects & Filters", 760., 480., (EFFECT_TABS, tab, Dialog::Effects), c, cx)
    }

    fn media_info_dialog(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let st = self.player.state();
        let kv = |k: &str, v: String| {
            div()
                .flex()
                .py(px(3.))
                .child(div().w(px(160.)).flex_none().text_size(px(12.)).text_color(t.text_dim).child(k.to_string()))
                .child(div().flex_1().min_w_0().text_size(px(13.)).child(v))
                .into_any_element()
        };
        let mut c = div().p_4().flex().flex_col();
        let Some(m) = st.media.clone() else {
            return self.modal(
                "Media Information",
                640.,
                div().p_4().child("Nothing is playing.").into_any_element(),
                None,
                cx,
            );
        };
        c = c.child(self.section("General"));
        if let Some(src) = &st.source {
            c = c.child(kv(
                "Source",
                match src {
                    engine::MediaSource::File(p) => p.display().to_string(),
                    engine::MediaSource::Url(u) => u.clone(),
                    engine::MediaSource::Dvd { path, title: 0, .. } => format!("DVD {path} (with menus)"),
                    engine::MediaSource::Dvd { path, title, .. } => format!("DVD {path}, title {title}"),
                    engine::MediaSource::Cd { device, track } => format!("Audio CD {device}, track {track}"),
                },
            ));
        }
        c = c.child(kv("Container", format!("{} ({})", m.format_long_name, m.format_name)));
        if m.duration > 0.0 {
            c = c.child(kv("Duration", format_time(m.duration)));
        }
        if let Some(sz) = m.file_size {
            c = c.child(kv("Size", format_size(sz)));
        }
        if m.bitrate > 0 {
            c = c.child(kv("Overall bitrate", format!("{:.0} kb/s", m.bitrate as f64 / 1000.0)));
        }
        if !m.chapters.is_empty() {
            c = c.child(kv("Chapters", m.chapters.len().to_string()));
        }
        for kind in [TrackKind::Video, TrackKind::Audio, TrackKind::Subtitle] {
            let tracks: Vec<_> = m.tracks_of(kind).collect();
            if tracks.is_empty() {
                continue;
            }
            c = c.child(self.section(match kind {
                TrackKind::Video => "Video",
                TrackKind::Audio => "Audio",
                TrackKind::Subtitle => "Subtitles",
            }));
            for (n, tr) in tracks.iter().enumerate() {
                let mut desc = vec![tr.codec.clone()];
                if tr.width > 0 {
                    desc.push(format!("{}×{}", tr.width, tr.height));
                }
                if tr.fps > 0.0 && !tr.is_cover_art {
                    desc.push(format!("{:.3} fps", tr.fps));
                }
                if tr.channels > 0 {
                    desc.push(tr.channel_layout.clone());
                }
                if tr.sample_rate > 0 {
                    desc.push(format!("{} Hz", tr.sample_rate));
                }
                if tr.bitrate > 0 {
                    desc.push(format!("{:.0} kb/s", tr.bitrate as f64 / 1000.0));
                }
                if let Some(l) = &tr.language {
                    desc.push(engine::language_name(l));
                }
                if tr.is_cover_art {
                    desc.push("cover art".into());
                }
                let label = tr.title.clone().unwrap_or_else(|| format!("Track {}", n + 1));
                c = c.child(kv(&label, desc.join(" · ")));
            }
        }
        if let Some(camera) = &m.camera {
            c = c.child(self.section("Camera metadata"));
            for (label, value) in [
                ("Manufacturer", &camera.manufacturer),
                ("Camera model", &camera.model),
                ("Capture gamma", &camera.capture_gamma),
                ("Capture gamut", &camera.capture_gamut),
                ("Coding equations", &camera.coding_equations),
                ("Capture FPS", &camera.capture_fps),
                ("Recording codec", &camera.codec),
                ("Lens model", &camera.lens_model),
            ] {
                if let Some(value) = value {
                    c = c.child(kv(label, value.clone()));
                }
            }
            let source = if camera.embedded {
                format!("Embedded MP4 XML: {}", camera.source.display())
            } else {
                camera.source.display().to_string()
            };
            c = c.child(kv("Metadata source", source));
        }
        c = c.child(self.section("Playback"));
        if let Some(v) = &st.video {
            c = c.child(kv(
                "Video decoder",
                format!(
                    "{}{}",
                    v.decoder,
                    v.hwdec.as_ref().map(|h| format!(" (hardware: {h})")).unwrap_or_else(|| " (software)".into())
                ),
            ));
            if !v.pix_fmt.is_empty() {
                let mut pf = vec![v.pix_fmt.clone()];
                if !v.color.is_empty() {
                    pf.push(v.color.clone());
                }
                if v.hdr {
                    pf.push("HDR".into());
                }
                c = c.child(kv("Pixel format", pf.join(" · ")));
            }
            c = c.child(kv(
                "Displayed size",
                format!("{}×{} (aspect {:.3})", self.frame_size.0, self.frame_size.1, v.display_aspect),
            ));
        }
        if let Some(a) = &st.audio {
            c = c.child(kv(
                "Audio decoder",
                format!("{} · {} Hz {} {}", a.decoder, a.in_rate, a.in_layout, a.in_format),
            ));
            c = c.child(kv("Audio output", format!("{} · {} Hz · {} ch", a.device, a.out_rate, a.out_channels)));
        }
        c = c
            .child(kv("Frames", format!("{} decoded, {} dropped", st.decoded_frames, st.dropped_frames)))
            .child(kv("Buffered", format!("{:.1} s", st.cache_secs)));
        if !m.metadata.is_empty() {
            c = c.child(self.section("Tags"));
            for (k, v) in m.metadata.iter().take(40) {
                c = c.child(kv(k, v.clone()));
            }
        }
        self.modal("Media Information", 680., c.into_any_element(), None, cx)
    }

    fn disc_dialog(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let drives = disc::optical_drives();
        let selected = self.disc_scan.as_ref().map(|(p, _)| p.clone());
        let mut c = div().p_4().flex().flex_col().gap_2();
        c = c.child(self.section("Source"));
        if drives.is_empty() {
            c = c.child(div().text_size(px(13.)).text_color(t.text_dim).child(
                "No disc drives were found. You can open a DVD folder (VIDEO_TS), a DVD ISO image or an audio CD image (.cue) instead.",
            ));
        }
        for d in drives {
            let p = d.path.clone();
            let sel = selected.as_deref() == Some(d.path.as_str());
            c = c.child(
                div()
                    .id(SharedString::from(format!("drive-{}", d.path)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(36.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if sel { t.accent } else { t.border })
                    .when(sel, |d| d.bg(t.accent_soft))
                    .hover(|s| s.bg(t.hover))
                    .child(icon(Icon::Disc, px(18.), t.text))
                    .child(div().flex_1().text_size(px(13.)).child(d.label.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| this.scan_disc(p.clone(), cx))),
            );
        }
        c = c.child(
            div()
                .flex()
                .gap_2()
                .child(self.text_button(
                    "disc-folder",
                    "Open DVD Folder…",
                    false,
                    |this, _, w, cx| this.pick_disc(true, w, cx),
                    cx,
                ))
                .child(self.text_button(
                    "disc-iso",
                    "Open Disc Image…",
                    false,
                    |this, _, w, cx| this.pick_disc(false, w, cx),
                    cx,
                )),
        );
        let note = |text: String, color| div().text_size(px(13.)).text_color(color).child(text);
        let row = |id: String| {
            div()
                .id(SharedString::from(id))
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .h(px(34.))
                .flex_none()
                .rounded(px(7.))
                .cursor_pointer()
                .hover(|s| s.bg(t.hover))
        };
        let mut footer = div().flex().gap_2();
        match self.disc_scan.clone() {
            None => {}
            Some((path, None)) => {
                c = c.child(self.section(&format!("On {path}"))).child(note("Reading disc…".into(), t.text_dim));
            }
            Some((path, Some(disc::DiscContent::Unreadable))) => {
                c = c.child(self.section(&format!("On {path}"))).child(note(
                    "No DVD or audio CD could be read. Is there a disc in the drive? Encrypted DVDs need libdvdcss."
                        .into(),
                    t.danger,
                ));
            }
            Some((path, Some(disc::DiscContent::Dvd(titles)))) => {
                c = c.child(self.section(&format!("DVD titles on {path}")));
                let main = disc::main_title(&titles);
                for tt in titles {
                    let (p, n) = (path.clone(), tt.number);
                    c = c.child(
                        row(format!("title-{n}"))
                            .child(div().w(px(70.)).text_size(px(13.)).child(format!("Title {n}")))
                            .child(div().w(px(70.)).text_size(px(13.)).child(format_time(tt.duration)))
                            .child(
                                div()
                                    .w(px(90.))
                                    .text_size(px(12.))
                                    .text_color(t.text_dim)
                                    .child(format!("{} chapters", tt.chapters)),
                            )
                            .child(div().flex_1().truncate().text_size(px(12.)).text_color(t.text_dim).child({
                                let mut d = tt.audio.join(", ");
                                if !tt.subtitles.is_empty() {
                                    d.push_str(&format!(" · subtitles: {}", tt.subtitles.join(", ")));
                                }
                                d
                            }))
                            .when(main == Some(n), |d| {
                                d.child(
                                    div()
                                        .px_2()
                                        .rounded(px(4.))
                                        .bg(t.accent_soft)
                                        .text_color(t.accent)
                                        .text_size(px(11.))
                                        .child("Main feature"),
                                )
                            })
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.close_dialog(w, cx);
                                this.playlist.clear();
                                this.play_dvd_title(p.clone(), n, w, cx);
                            })),
                    );
                }
                let menus = engine::dvd_menus_available();
                if menus {
                    let p = path.clone();
                    footer = footer.child(self.text_button(
                        "disc-menus",
                        "Play from Disc Menu",
                        true,
                        move |this, _, w, cx| this.play_disc_dvd(p.clone(), true, w, cx),
                        cx,
                    ));
                }
                footer = footer.child(self.text_button(
                    "disc-play",
                    "Play Main Feature",
                    !menus,
                    move |this, _, w, cx| this.play_disc_dvd(path.clone(), false, w, cx),
                    cx,
                ));
            }
            Some((path, Some(disc::DiscContent::AudioCd(toc)))) => {
                let mut head = "Audio CD".to_string();
                match (&toc.artist, &toc.album) {
                    (Some(a), Some(t)) => head = format!("{a} – {t}"),
                    (None, Some(t)) => head = t.clone(),
                    _ => {}
                }
                c = c.child(self.section(&head));
                let total: f64 = toc.tracks.iter().map(|t| t.duration()).sum();
                c = c.child(note(format!("{} tracks, {}", toc.tracks.len(), format_time(total)), t.text_dim));
                for (k, tr) in toc.tracks.iter().enumerate() {
                    let (p, toc2) = (path.clone(), toc.clone());
                    let name = tr.title.clone().unwrap_or_else(|| format!("Track {}", tr.number));
                    let artist = tr.artist.clone().filter(|a| Some(a) != toc.artist.as_ref());
                    c = c.child(
                        row(format!("track-{}", tr.number))
                            .child(
                                div()
                                    .w(px(28.))
                                    .text_size(px(12.))
                                    .text_color(t.text_dim)
                                    .child(format!("{}", tr.number)),
                            )
                            .child(div().flex_1().truncate().text_size(px(13.)).child(match artist {
                                Some(a) => format!("{name} – {a}"),
                                None => name,
                            }))
                            .child(div().text_size(px(12.)).text_color(t.text_dim).child(format_time(tr.duration())))
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.close_dialog(w, cx);
                                this.play_cd_tracks(p.clone(), &toc2, None, k, false, w, cx);
                            })),
                    );
                }
                footer = footer.child(self.text_button(
                    "disc-cd",
                    "Play CD",
                    true,
                    move |this, _, w, cx| {
                        this.close_dialog(w, cx);
                        this.play_cd_tracks(path.clone(), &toc, None, 0, false, w, cx);
                    },
                    cx,
                ));
            }
            Some((path, Some(disc::DiscContent::MountedCd(files)))) => {
                c = c.child(self.section(&format!("Audio CD ({path})")));
                for f in &files {
                    c = c.child(note(
                        f.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        t.text,
                    ));
                }
                footer = footer.child(self.text_button(
                    "disc-cd",
                    "Play CD",
                    true,
                    move |this, _, w, cx| {
                        this.close_dialog(w, cx);
                        this.save_current_position();
                        this.playlist.clear();
                        let sources = files.iter().map(|f| engine::MediaSource::File(f.clone())).collect();
                        this.open_sources(sources, w, cx);
                    },
                    cx,
                ));
            }
        }
        self.modal("Play Disc", 640., c.into_any_element(), Some(footer.into_any_element()), cx)
    }

    fn pick_disc(&mut self, folder: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: !folder,
            directories: folder,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(p))) = rx.await
                && let Some(p) = p.into_iter().next()
            {
                let _ = this.update(cx, |v, cx| {
                    if engine::cdda::is_cue(&p) {
                        v.scan_disc(p.display().to_string(), cx);
                        return;
                    }
                    match disc::dvd_root(&p) {
                        Some(root) => v.scan_disc(root, cx),
                        None => v.osd("That doesn't look like a DVD (no VIDEO_TS found) or an audio CD image", cx),
                    }
                });
            }
        })
        .detach();
    }
}

fn ffmpeg_version() -> String {
    unsafe { crate::engine::av::from_cstr(crate::engine::av::ff::av_version_info()).unwrap_or_else(|| "?".into()) }
}
