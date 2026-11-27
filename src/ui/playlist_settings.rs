//! Compact playlist control panels, using the timeline settings visual style.

use super::menu::{MenuItem, OpenMenu};
use super::player::PlayerView;
use super::theme::Theme;
use super::widgets::{icon, scrollbar, tooltip};
use crate::icons::Icon;
use crate::settings::PlaylistThumbnails as P;
use std::cell::Cell;
use std::rc::Rc;
use gpui::{
    AnyElement, Bounds, Context, Div, MouseButton, SharedString, Window, anchored, canvas, div, fill, point, prelude::*,
    px, rgb, size,
};

// Draw schematic previews, not media images. No thumbnail cache access is needed.
fn swatch(mode: P, t: Theme) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            if w < 8. || h < 12. {
                return;
            }
            let mut rect = |x: f32, y: f32, width: f32, height: f32, color: gpui::Hsla| {
                window.paint_quad(fill(
                    Bounds::new(point(bounds.left() + px(x), bounds.top() + px(y)), size(px(width), px(height))),
                    color,
                ));
            };
            let list_w = if mode.side() { w * 0.59 } else { w };
            let list_x = if mode.side() { w - list_w } else { 0. };
            let row_h = h / 3.;
            let thumb_w = (row_h * 1.5).min(list_w * 0.36);
            let colors = [0x526c78, 0x87916b, 0xa57c58];
            for (i, color) in colors.into_iter().enumerate() {
                let y = row_h * i as f32;
                rect(list_x, y + 1., list_w, row_h - 2., if i == 1 { t.accent_soft } else { t.track });
                let x = if mode.inline() {
                    rect(list_x + 2., y + 2., thumb_w, row_h - 4., rgb(color).into());
                    if mode == P::Inline && i == 1 {
                        rect(list_x + 2. + thumb_w * 0.50, y + 2., thumb_w * 0.50, row_h - 4., rgb(0x526c78).into());
                    }
                    thumb_w + 5.
                } else {
                    4.
                };
                let text_y = if mode.inline() { 0.32 } else { 0.44 };
                rect(list_x + x, y + row_h * text_y, (list_w - x - 5.).max(0.) * 0.76, 2., t.text_dim);
                if mode.inline() {
                    rect(list_x + x, y + row_h * 0.63, (list_w - x - 5.).max(0.) * 0.48, 1., t.text_disabled);
                }
                if mode.scrubbing() && i == 1 {
                    rect(list_x + list_w * 0.72, y + 1., 1.5, row_h - 2., t.accent);
                }
            }
            if mode.side() {
                let x = 0.;
                let width = (list_x - 3.).max(0.);
                rect(x, h * 0.20, width, h * 0.60, rgb(0x526c78).into());
                rect(x, h * 0.57, width, h * 0.23, rgb(0x354c40).into());
                rect(x + width * 0.60, h * 0.30, width * 0.20, h * 0.18, rgb(0xa6a879).into());
                if mode == P::Hover {
                    rect(x + width * 0.12, h * 0.40, width * 0.30, h * 0.17, rgb(0x87916b).into());
                }
            }
        },
    )
    .w_full()
    .h(px(30.))
    .into_any_element()
}

fn filmstrip_icon(color: gpui::Hsla) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let mut rect = |x: f32, y: f32, w: f32, h: f32| {
                window.paint_quad(fill(Bounds::new(point(bounds.left() + px(x), bounds.top() + px(y)), size(px(w), px(h))), color));
            };
            rect(0., 1., 16., 1.);
            rect(0., 12., 16., 1.);
            for x in [0., 5., 10., 15.] {
                rect(x, 2., 1., 10.);
            }
            for x in [1., 6., 11.] {
                rect(x, 4., 4., 1.);
                rect(x, 9., 4., 1.);
            }
        },
    )
    .w(px(16.))
    .h(px(14.))
    .into_any_element()
}

fn mode_tooltip(mode: P) -> SharedString {
    let description = match mode {
        P::None => "show only media icons",
        P::StaticInline => "show a representative picture next to each title",
        P::Inline => "scrub through thumbnails inside each row",
        P::InlineSide => "show a picture in the row and a larger preview to the left",
        P::Hover => "show a larger preview to the left without inline pictures",
    };
    format!("{}\n({description})", mode.label()).into()
}

fn action_tooltip(label: &str) -> SharedString {
    let description = match label {
        "Save current playlist state" => "Save the current contents and state of the playlist for later use.",
        "Import playlist from file…" => "Import entries from an M3U, M3U8 or PLS file into the current playlist.",
        "Reset thumbnail" => "Restore the default representative thumbnail.",
        "Add files" => "Add individual media files to the current playlist.",
        "Add folder" => "Add media files from a folder to the current playlist.",
        "Export playlist (M3U)" => "Export the playlist as a standard M3U file, without saving its internal state.",
        "Clear playlist" => "Remove all entries from the playlist without deleting media files.",
        _ if label.starts_with("Generate missing playlist thumbnails") => "Create missing representative pictures for the playlist.",
        _ if label.starts_with("Generate playlist thumbnail") => "Create a representative picture for the selected media.",
        _ if label.starts_with("Generate missing filmstrip") => "Sample video frames for scrubbing, timeline previews and visualizations; keep existing filmstrips.",
        _ if label.starts_with("Regenerate filmstrip") => "Replace existing filmstrips by sampling the video again.",
        _ if label.starts_with("Generate missing waveforms") => "Analyze audio amplitude over time where a waveform is not cached.",
        _ if label.starts_with("Regenerate") && label.contains("waveform") => "Analyze the audio again and replace the existing waveform cache.",
        _ => label,
    };
    SharedString::from(description.to_owned())
}

fn heading(text: impl Into<SharedString>, t: Theme) -> Div {
    div().pt_1().text_size(px(11.)).text_color(t.text_dim).child(text.into())
}

impl PlayerView {
    pub(super) fn playlist_mode_cards(&self, columns: usize, cx: &mut Context<Self>) -> Div {
        let t = self.theme;
        let mut cards = div().flex().flex_col().gap_1();
        for pair in P::ALL.chunks(columns) {
            let mut row = div().flex().gap_1();
            for &mode in pair {
                let selected = self.settings.playlist_thumbnails == mode;
                row = row.child(
                    div()
                        .id(SharedString::from(format!("playlist-mode-{mode:?}")))
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_1()
                        .rounded(px(5.))
                        .border_1()
                        .border_color(if selected { t.accent } else { t.border })
                        .bg(if selected { t.accent_soft } else { t.surface_alt })
                        .child(swatch(mode, t))
                        .tooltip(tooltip(mode_tooltip(mode), t))
                        .cursor_pointer()
                        .hover(|s| s.bg(t.hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings.playlist_thumbnails = mode;
                            this.apply_settings(cx);
                        })),
                );
            }
            // Keep the last card the same width as the preceding cards.
            for _ in pair.len()..columns {
                row = row.child(div().flex_1().min_w_0());
            }
            cards = cards.child(row);
        }
        cards
    }

    fn playlist_action_button(&self, index: usize, item: &MenuItem, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let (label, ic, enabled, submenu) = match item {
            MenuItem::Action { label, icon, enabled, .. } => (label, *icon, *enabled, false),
            MenuItem::Submenu { label, icon, enabled, .. } => (label, *icon, *enabled, true),
            _ => return div().into_any_element(),
        };
        let keep_open = label.as_ref() == "Reset thumbnail";
        let run = match item {
            MenuItem::Action { run, .. } => Some(run.clone()),
            _ => None,
        };
        let waveform = label.contains("waveform") && (label.starts_with("Generate") || label.starts_with("Regenerate"));
        let picture = label.starts_with("Generate") && label.contains("playlist thumbnail");
        let filmstrip = (label.starts_with("Generate") || label.starts_with("Regenerate")) && label.contains("filmstrip");
        let display = if let Some(text) = label.strip_prefix("Generate ") {
            let mut chars = text.chars();
            SharedString::from(chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default())
        } else {
            label.clone()
        };
        let color = if enabled { t.text } else { t.text_disabled };
        div()
            .id(SharedString::from(format!("playlist-panel-action-{index}")))
            .flex_1()
            .min_w_0()
            .min_h(px(30.))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(px(5.))
            .border_1()
            .border_color(t.border)
            .bg(t.surface_alt)
            .text_size(px(12.))
            .text_color(color)
            .when(waveform, |d| d.child(super::render::waveform_badge(color)))
            .when(filmstrip, |d| d.child(filmstrip_icon(color)))
            .when_some(if picture { Some(Icon::Screenshot) } else { ic }, |d, ic| d.child(icon(ic, px(14.), color)))
            .child(div().flex_1().min_w_0().child(display))
            .tooltip(tooltip(action_tooltip(label.as_ref()), t))
            .when(submenu, |d| d.child(icon(Icon::ChevronRight, px(12.), color)))
            .when(enabled, |d| {
                d.cursor_pointer().hover(|s| s.bg(t.hover)).on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                    if let Some(run) = &run {
                        this.playlist_previews.pointer = Some(ev.position());
                        if !keep_open {
                            this.close_menu(cx);
                        }
                        run(this, window, cx);
                        if !keep_open {
                            this.note_activity(cx);
                        }
                    } else if let Some(menu) = this.menu.as_mut() {
                        menu.open_path = vec![index];
                    }
                    cx.notify();
                }))
            })
            .into_any_element()
    }

    pub(super) fn open_playlist_settings(&mut self, window: &Window, cx: &mut Context<Self>) {
        let bounds = self.playlist_bounds.get();
        let width = px(420.).min((window.viewport_size().width - px(8.)).max(px(100.)));
        let pos = point((bounds.left() - width - px(4.)).max(px(4.)), bounds.top().max(px(4.)));
        let items = self.playlist_panel_menu();
        self.open_menu(items, pos, None, cx);
        if let Some(menu) = self.menu.as_mut() {
            menu.playlist_context = true;
            menu.playlist_settings = true;
            menu.min_width = px(420.);
        }
    }

    pub(super) fn open_collection_settings(&mut self, window: &Window, cx: &mut Context<Self>) {
        let bounds = self.playlist_bounds.get();
        let width = px(400.).min((window.viewport_size().width - px(8.)).max(px(100.)));
        let pos = point((bounds.left() - width - px(4.)).max(px(4.)), bounds.top().max(px(4.)));
        let items = self.collection_panel_menu();
        self.open_menu(items, pos, None, cx);
        if let Some(menu) = self.menu.as_mut() {
            menu.playlist_context = true;
            menu.collection_settings = true;
            menu.min_width = px(400.);
        }
    }

    pub(super) fn playlist_context_panel(&self, menu: &OpenMenu, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let mut items = menu.items.clone();
        if let Some(id) = menu.playlist_item
            && let Some(reset) = self.playlist_panel_reset_action(id)
            && let Some(index) = items.iter().position(|item| matches!(item, MenuItem::Action { label, .. } if label.as_ref() == "Edit title"))
        {
            items.insert(index + 1, reset);
        }
        let viewport = window.viewport_size();
        let mut width = menu.min_width.min((viewport.width - px(8.)).max(px(100.)));
        if !menu.playlist_settings && !menu.collection_settings {
            let left_space = self.playlist_bounds.get().left() - px(8.);
            if left_space >= px(180.) {
                width = width.min(left_space);
            }
        }
        let columns = if width >= px(300.) { 2 } else { 1 };
        let mut pos = menu.pos;
        let mut max_h = (viewport.height - px(8.)).clamp(px(100.), px(680.));
        if !menu.playlist_settings && !menu.collection_settings {
            // Keep the item panel above the controls, including fullscreen visualizations.
            let playlist = self.playlist_bounds.get();
            let controls = self.control_bounds.get();
            let bottom = if controls.size.height > px(0.) && controls.top() > playlist.top() {
                playlist.bottom().min(controls.top())
            } else {
                playlist.bottom()
            };
            // Reserve room for fullscreen controls even when the item panel hides them.
            let bottom = if window.is_fullscreen() {
                let reserve = super::render::FULLSCREEN_CONTROL_BAR_H
                    + if self.player.with_state(|st| self.thumbnails.has_previews(st, &self.settings, true)) {
                        self.settings.visualization_height(true) as f32 + 2.
                    } else {
                        0.
                    };
                bottom.min(viewport.height - px(reserve))
            } else {
                bottom.min(viewport.height)
            } - px(4.);
            pos.x = (playlist.left() - width - px(4.)).max(px(4.));
            let top = playlist.top().max(px(4.));
            let available = (bottom - top).max(px(1.));
            // Allow extra height for wrapped button labels, with scrolling on short windows.
            let mut rows = 0;
            let mut buttons = 0usize;
            let mut headings = 0;
            for item in &items {
                if matches!(item, MenuItem::Header(_) | MenuItem::Separator) {
                    rows += buttons.div_ceil(columns);
                    buttons = 0;
                    headings += 1;
                } else {
                    buttons += 1;
                }
            }
            rows += buttons.div_ceil(columns);
            max_h = px(130. + rows as f32 * 46. + headings as f32 * 28.).min(available).min(px(680.));
            pos.y = pos.y.clamp(top, (bottom - max_h).max(top));
        }
        let scroll = menu.scroll(0);
        let mut body = div().flex().flex_col().gap_1().p_2();
        if menu.playlist_settings {
            body = body
                .child(heading("THUMBNAIL DISPLAY", t))
                .child(self.playlist_mode_cards(P::ALL.len(), cx))
                .child(self.timeline_panel_toggle(
                    "playlist-panel-autohide",
                    if self.in_fullscreen { "Autohide playlist in fullscreen" } else { "Autohide playlist in window" },
                    self.playlist_autohide(),
                    true,
                    "Automatically hide the playlist panel.",
                    |this, _, _, cx| {
                        this.set_playlist_autohide(!this.playlist_autohide());
                        this.apply_settings(cx);
                    },
                    cx,
                ));
        }
        if let Some(item) = menu.playlist_item.and_then(|id| self.playlist.items.iter().find(|it| it.id == id)) {
            let scrub = self.playlist_panel_scrub_preview(item.id).map(|(_, preview)| preview);
            let preview = super::playlist_thumbnails::Key::new(item, &self.settings).and_then(|key| {
                let duration = item.duration.unwrap_or_default();
                let target = key.representative_time(&self.settings, duration);
                self.thumbnails.for_playlist(&key.path, key.stream, key.count, target).or_else(|| {
                    self.playlist_previews.cached_representative(&key, target, duration * crate::engine::thumbnails::representative_fraction(&key.path))
                })
            });
            let (title, filename) = item.labels(crate::settings::PlaylistLabels::Both);
            body = body.child(div().flex().items_center().gap_2().p_1()
                .when_some(scrub.as_ref().or(preview), |d, p| d.child(p.element(104., 58.)))
                .child(div().flex_1().min_w_0().flex().flex_col().gap_1().child(title)
                    .when_some(filename, |d, name| d.child(div().text_size(px(11.)).text_color(t.text_dim).child(name)))));
        }
        let mut i = 0;
        let mut section = "";
        while i < items.len() {
            match &items[i] {
                MenuItem::Header(label) => {
                    section = label.as_ref();
                    body = body.child(div().mt_1().h(px(1.)).bg(t.border)).child(heading(label.clone(), t));
                    if menu.playlist_settings && label.as_ref() == "SAVE" {
                        body = body.child(self.timeline_panel_toggle(
                            "playlist-panel-autosave",
                            "Auto save playlist",
                            self.playlist_library.autosave(),
                            self.playlist_library.writable,
                            "Automatically save changes to this playlist.",
                            |this, _, _, cx| this.toggle_playlist_autosave(cx),
                            cx,
                        ));
                    }
                    i += 1;
                }
                MenuItem::Separator => {
                    body = body.child(div().my_1().h(px(1.)).bg(t.border));
                    i += 1;
                }
                _ => {
                    if section == "GENERATE" && columns == 2 {
                        let start = i;
                        while i < items.len() && !matches!(&items[i], MenuItem::Header(_) | MenuItem::Separator) {
                            i += 1;
                        }
                        let mut pictures = div().flex_1().min_w_0().flex().flex_col().gap_1();
                        let mut audio = div().flex_1().min_w_0().flex().flex_col().gap_1();
                        for index in start..i {
                            let item = &items[index];
                            let is_audio = matches!(item, MenuItem::Action { label, .. } if label.contains("waveform"));
                            let button = self.playlist_action_button(index, item, cx);
                            if is_audio { audio = audio.child(button); } else { pictures = pictures.child(button); }
                        }
                        body = body.child(div().flex().items_start().gap_1().child(pictures).child(audio));
                        continue;
                    }
                    let row_columns = if (section == "ORGANIZE" || (!menu.playlist_settings && section.is_empty())) && width >= px(300.) { 3 } else { columns };
                    let mut row = div().flex().gap_1();
                    let mut n = 0;
                    while n < row_columns && i < items.len()
                        && !matches!(&items[i], MenuItem::Header(_) | MenuItem::Separator)
                    {
                        row = row.child(self.playlist_action_button(i, &items[i], cx));
                        i += 1;
                        n += 1;
                    }
                    body = body.child(row);
                }
            }
        }
        let panel_bounds = Rc::new(Cell::new(Bounds::default()));
        let drag_bounds = panel_bounds.clone();
        let title = div()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(t.border)
            .when(menu.playlist_settings || menu.collection_settings, |d| d.cursor_pointer().on_mouse_down(MouseButton::Left, cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                if let Some(menu) = this.menu.as_mut() {
                    let bounds = drag_bounds.get();
                    menu.panel_drag = Some((point(ev.position.x - bounds.left(), ev.position.y - bounds.top()), bounds.size.height));
                }
                cx.stop_propagation();
            })))
            .child(if menu.collection_settings { "Collection settings" } else if menu.playlist_settings { "Playlist settings" } else { "Playlist item actions" })
            .child(
                div()
                    .id("playlist-panel-close")
                    .cursor_pointer()
                    .p_1()
                    .rounded(px(4.))
                    .hover(|s| s.bg(t.hover))
                    .child(icon(Icon::WindowClose, px(16.), t.text))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, _, cx| this.close_menu(cx))),
            );
        let panel = div()
            .id("playlist-settings-panel")
            .w(width)
            .relative()
            .child(canvas(move |bounds, _, _| panel_bounds.set(bounds), |_, _, _, _| {}).absolute().size_full())
            .max_h(max_h)
            .flex()
            .flex_col()
            .rounded(px(8.))
            .bg(t.popup)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .text_color(t.text)
            .text_size(px(13.))
            .occlude()
            .when(menu.playlist_item.is_some(), |d| {
                d.on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, _, cx| {
                    this.clear_playlist_panel_scrub(cx);
                }))
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(title)
            .child(
                div()
                    .relative()
                    .flex_shrink()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id("playlist-settings-scroll")
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .track_scroll(&scroll)
                            .child(body),
                    )
                    .child(scrollbar("playlist-settings-scrollbar", &scroll, t)),
            );
        anchored().position(pos).snap_to_window_with_margin(px(4.)).child(panel).into_any_element()
    }
}
