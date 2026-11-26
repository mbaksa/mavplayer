//! Persistent, in-window timeline settings popover. It shares the menu's
//! backdrop and lifetime, but its controls do not dismiss it when clicked.

use super::menu::OpenMenu;
use super::player::PlayerView;
use super::theme::Theme;
use super::widgets::{icon, scrollbar, tooltip};
use crate::icons::Icon;
use crate::settings::TimelineVisualization as V;
use gpui::{AnyElement, Bounds, ClickEvent, Context, Div, ElementId, MouseButton, SharedString, Stateful, Window,
    anchored, canvas, div, fill, point, prelude::*, px, rgb, size};

pub(super) const MAIN: [V; 4] = [V::None, V::Filmstrip, V::Waveform, V::FilmstripWaveform];
pub(super) const BARCODES: [V; 8] = [V::BarcodeAverage, V::BarcodeCompressed, V::DominantColors, V::Luma,
    V::LumaRange, V::Saturation, V::VisualComplexity, V::FrameDifference];

fn label(mode: V) -> &'static str {
    match mode {
        V::None => "None",
        V::BarcodeAverage => "Average color",
        V::BarcodeCompressed => "Compressed frames",
        V::DominantColors => "Dominant colors",
        _ => mode.label(),
    }
}

fn description(mode: V) -> &'static str {
    match mode {
        V::None => "Show only the classic seekbar.",
        V::Filmstrip => "Show sampled video frames along the timeline.",
        V::Waveform => "Show the minimum and maximum audio amplitudes over time.",
        V::FilmstripWaveform => "Combine sampled video frames with the audio waveform.",
        V::BarcodeAverage => "Show the average RGB color of each sampled frame.",
        V::BarcodeCompressed => "Compress each sampled frame horizontally while preserving its vertical composition.",
        V::DominantColors => "Show the most prominent groups of colors in each sampled frame.",
        V::Luma => "Show the average image brightness of each sampled frame.",
        V::LumaRange => "Show the 5th to 95th percentile of image brightness, with the median marked.",
        V::Saturation => "Show the average HSV color saturation of each sampled frame.",
        V::VisualComplexity => "Highlight local brightness changes, detail and texture within sampled frames.",
        V::FrameDifference => "Show visual changes between sampled frames, not a measurement of motion.",
    }
}

// Illustrative swatches, not another analysis of the current media. They use
// no decoded images, persistent textures or background work.
fn swatch(mode: V, t: Theme) -> AnyElement {
    canvas(|_, _, _| (), move |bounds, _, window, _| {
        let w = f32::from(bounds.size.width);
        let h = f32::from(bounds.size.height);
        if w <= 0.0 || h <= 0.0 { return; }
        let mut rect = |x: f32, y: f32, width: f32, height: f32, color: gpui::Hsla| {
            window.paint_quad(fill(Bounds::new(point(bounds.left() + px(x), bounds.top() + px(y)), size(px(width), px(height))), color));
        };
        let colors = [0x526c78, 0x87916b, 0xa57c58, 0x3c505d, 0x766f85, 0xb5a180];
        match mode {
            V::None => {
                rect(0., h * 0.45, w, 2., t.track);
                rect(0., h * 0.45, w * 0.38, 2., t.accent);
            }
            V::Filmstrip | V::FilmstripWaveform => {
                let height = if mode == V::FilmstripWaveform { h * 0.62 } else { h };
                for (i, c) in colors.into_iter().enumerate() {
                    let x = w * i as f32 / 6.;
                    rect(x, 0., (w / 6. - 1.).max(0.), height, rgb(c).into());
                    let fw = (w / 6. - 1.).max(0.);
                    let dark = rgb((c >> 1) & 0x7f7f7f).into();
                    rect(x, height * 0.58, fw, height * 0.42, dark);
                    // Tiny illustrative landscapes and buildings with distinct silhouettes.
                    rect(x + fw * 0.12, height * 0.12, fw * 0.18, height * 0.18, rgb(0xe4d5a4).into());
                    for j in 0..9 {
                        let ridge = (0.28 + ((j * 3 + i * 5) % 7) as f32 * 0.055) * height;
                        rect(x + fw * j as f32 / 9., ridge, fw / 9., height * 0.76 - ridge, dark);
                    }
                    rect(x + fw * 0.54, height * 0.48, fw * 0.23, height * 0.38, rgb(0x433c32).into());
                    rect(x + fw * 0.61, height * 0.56, fw * 0.08, height * 0.13, rgb(0xe4d5a4).into());
                }
            }
            V::Waveform => {}
            _ => {
                for i in 0..36 {
                    let x = w * i as f32 / 36.;
                    let width = w * (i + 1) as f32 / 36. - x;
                    let v = ((i * 17 + i / 5 * 31) % 180 + 45) as u32;
                    let base = colors[(i / 6) % colors.len()];
                    match mode {
                        V::BarcodeCompressed => {
                            for j in 0..4 {
                                let c = colors[(i / 6 + j * 2) % colors.len()];
                                rect(x, h * j as f32 / 4., width, h / 4., rgb(c).into());
                            }
                        }
                        V::DominantColors => {
                            let split = 0.35 + v as f32 / 900.;
                            rect(x, 0., width, h * split, rgb(base).into());
                            rect(x, h * split, width, h * (0.85 - split), rgb(colors[(i / 6 + 2) % colors.len()]).into());
                            rect(x, h * 0.85, width, h * 0.15, rgb(colors[(i / 6 + 4) % colors.len()]).into());
                        }
                        V::LumaRange => {
                            let top = h * (0.08 + v as f32 / 1200.);
                            let bottom = h * (0.68 + v as f32 / 800.);
                            rect(x, 0., width, h, rgb(0x141414).into());
                            rect(x, top, width, bottom - top, rgb(0x919191).into());
                            rect(x, (top + bottom) * 0.5, width, 1., rgb(0xffffff).into());
                        }
                        V::Luma => rect(x, 0., width, h, rgb(((i / 4 * 39 + 30) % 256) as u32 * 0x010101).into()),
                        V::Saturation => rect(x, 0., width, h, rgb(((i / 3 * 61 + 17) % 256) as u32 * 0x010101).into()),
                        V::VisualComplexity => rect(x, 0., width, h, rgb(v * 0x010101).into()),
                        V::FrameDifference => rect(x, 0., width, h, rgb((if i % 7 == 0 { v } else { v / 5 }) * 0x010101).into()),
                        _ => rect(x, 0., width, h, rgb(base).into()),
                    }
                }
            }
        }
        if matches!(mode, V::Waveform | V::FilmstripWaveform) {
            let (top, span) = if mode == V::Waveform { (0., h) } else { (h * 0.64, h * 0.36) };
            for i in 0..48 {
                let a = span * (0.15 + 0.8 * (((i * 13 + i / 5 * 7) % 19) as f32 / 18.));
                rect(w * i as f32 / 48., top + (span - a) * 0.5, (w / 48. - 0.5).max(0.5), a, t.accent);
            }
        }
    }).w_full().h(px(22.)).into_any_element()
}

fn heading(text: &'static str, t: Theme) -> Div {
    div().pt_1().text_size(px(11.)).text_color(t.text_dim).child(text)
}

impl PlayerView {
    #[allow(clippy::too_many_arguments)]
    fn timeline_panel_button(
        &self, id: impl Into<ElementId>, text: impl Into<SharedString>, selected: bool, enabled: bool,
        tip: &'static str, run: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = self.theme;
        div().id(id).px_2().py_1().min_h(px(26.)).rounded(px(5.)).border_1()
            .border_color(if selected { t.accent } else { t.border })
            .bg(if selected { t.accent_soft } else { t.surface_alt })
            .text_color(if enabled { t.text } else { t.text_disabled })
            .text_size(px(12.)).child(text.into()).tooltip(tooltip(tip, t))
            .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(t.hover)).on_click(cx.listener(run)))
    }

    #[allow(clippy::too_many_arguments)]
    fn timeline_panel_toggle(
        &self, id: impl Into<ElementId>, text: &'static str, on: bool, enabled: bool,
        tip: &'static str, run: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = self.theme;
        div().id(id).flex().items_center().justify_between().gap_2().px_2().py_1()
            .text_size(px(12.)).text_color(if enabled { t.text } else { t.text_disabled })
            .child(div().flex_1().min_w_0().child(text))
            .child(div().flex_none().w(px(40.)).h(px(22.)).rounded_full().p(px(3.)).flex()
                .bg(if on { t.accent } else { t.track })
                .when(on, |d| d.justify_end())
                .child(div().size(px(16.)).rounded_full().bg(gpui::white()).shadow_sm())
                .when(!enabled, |d| d.opacity(0.45)))
            .tooltip(tooltip(tip, t))
            .when(enabled, |d| d.cursor_pointer().on_click(cx.listener(run)))
    }

    pub(super) fn timeline_cards(&self, choices: &[V], fullscreen: bool, cx: &mut Context<Self>) -> Div {
        let s = &self.settings;
        let t = self.theme;
        let current = s.timeline_visualization(fullscreen);
        let explicit = !fullscreen || s.timeline_fullscreen_visualization.is_some();
        let mut list = div().flex().flex_col().gap_1();
        for (row, pair) in choices.chunks(2).enumerate() {
            let mut line = div().flex().gap_1();
            for (col, &mode) in pair.iter().enumerate() {
                let selected = explicit && current == mode;
                let enabled = mode == V::None || mode.available(s.timeline_thumbnails, s.timeline_waveform) != V::None;
                line = line.child(div().id(SharedString::from(format!("timeline-card-{fullscreen}-{mode:?}-{row}-{col}")))
                    .flex_1().min_w_0().flex().flex_col().gap_1().p_1().rounded(px(5.)).border_1()
                    .border_color(if selected { t.accent } else { t.border })
                    .bg(if selected { t.accent_soft } else { t.surface_alt })
                    .child(swatch(mode, t))
                    .child(div().text_size(px(11.)).truncate().child(label(mode)))
                    .tooltip(tooltip(description(mode), t))
                    .when(!enabled, |d| d.opacity(0.45))
                    .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(t.hover)).on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.set_timeline_visualization(fullscreen, mode);
                        this.apply_settings(cx);
                    }))));
            }
            list = list.child(line);
        }
        list
    }

    pub(super) fn timeline_settings_panel(&self, menu: &OpenMenu, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let s = &self.settings;
        let fs = window.is_fullscreen();
        let visible = s.visualization_enabled(fs);
        let height = s.visualization_height(fs);
        let opacity = s.visualization_opacity(fs);
        let min_height = s.timeline_filmstrip_min_height.clamp(12, height);
        let hover = s.timeline_hover_enabled(fs);
        let cursor = s.timeline_hover_playhead;
        let adaptive = s.timeline_adaptive;
        let viewport = window.viewport_size();
        let bottom = menu.pos.y;
        let width = px(370.).min((viewport.width - px(8.)).max(px(100.)));
        let available = (bottom - px(8.)).max(px(100.));
        let panel_height = px(600.).min(available).min((viewport.height - px(8.)).max(px(100.)));
        let origin = point(menu.pos.x.min((viewport.width - width - px(4.)).max(px(4.))).max(px(4.)), (bottom - panel_height - px(4.)).max(px(4.)));
        let scroll = menu.scroll(0);
        let mut body = div().flex().flex_col().gap_2().p_2()
            .child(heading("VISUALIZATION", t));
        if fs {
            body = body.child(self.timeline_panel_toggle("timeline-mode-inherit", "Same visualization as windowed mode",
                s.timeline_fullscreen_visualization.is_none(), true, "Use the visualization selected for windowed mode.", |this, _, _, cx| {
                    this.settings.timeline_fullscreen_visualization = if this.settings.timeline_fullscreen_visualization.is_none() { Some(this.settings.timeline_visualization(false)) } else { None };
                    this.apply_settings(cx);
                }, cx));
        }
        body = body.child(self.timeline_cards(&MAIN, fs, cx))
            .child(heading("VIDEO BARCODES", t))
            .child(self.timeline_cards(&BARCODES, fs, cx))
            .child(heading("APPEARANCE", t));
        let slider_label = |text: &'static str, value: String| div().flex().justify_between().text_size(px(12.)).child(text).child(value);
        let mut appearance = div().flex().flex_col().gap_1();
        if fs {
            appearance = appearance.child(self.timeline_panel_toggle("timeline-opacity-inherit", "Use windowed opacity", s.timeline_fullscreen_opacity.is_none(), visible,
                "Use the opacity selected for windowed mode.", |this, _, _, cx| { this.settings.timeline_fullscreen_opacity = if this.settings.timeline_fullscreen_opacity.is_none() { Some(this.settings.visualization_opacity(false)) } else { None }; this.apply_settings(cx); }, cx));
        }
        appearance = appearance.child(slider_label("Opacity", format!("{opacity}%")))
            .when(visible, |d| d.child(self.slider("timeline-opacity-slider", (opacity - 50) as f32 / 50., false, t, move |this, f, _, _| {
                let n = 50 + (f * 50.).round() as u32;
                if fs { this.settings.timeline_fullscreen_opacity = Some(n); } else { this.settings.timeline_opacity = n; }
            }, cx)));
        if fs {
            appearance = appearance.child(self.timeline_panel_toggle("timeline-height-inherit", "Use windowed height", s.timeline_fullscreen_height.is_none(), visible,
                "Use the height selected for windowed mode.", |this, _, _, cx| { this.settings.timeline_fullscreen_height = if this.settings.timeline_fullscreen_height.is_none() { Some(this.settings.visualization_height(false)) } else { None }; this.apply_settings(cx); }, cx));
        }
        appearance = appearance.child(slider_label("Height", format!("{height} px")))
            .when(visible, |d| d.child(self.slider("timeline-height-slider", (height - 12) as f32 / 84., false, t, move |this, f, _, _| {
                let n = 12 + (f * 84.).round() as u32;
                if fs { this.settings.timeline_fullscreen_height = Some(n); } else {
                    this.settings.timeline_filmstrip_height = n;
                    this.settings.timeline_filmstrip_min_height = this.settings.timeline_filmstrip_min_height.min(n);
                }
            }, cx)));
        if !fs {
            appearance = appearance.child(self.timeline_panel_toggle("timeline-adaptive", "Adapt height to window width", adaptive, visible,
                "Reduce the visualization height in smaller windows.", |this, _, _, cx| { this.settings.timeline_adaptive = !this.settings.timeline_adaptive; this.apply_settings(cx); }, cx));
            if adaptive && visible {
                appearance = appearance.child(slider_label("Minimum height", format!("{min_height} px")))
                    .when(height > 12, |d| d.child(self.slider("timeline-min-height-slider", (min_height - 12) as f32 / (height - 12) as f32, false, t, move |this, f, _, _| {
                        this.settings.timeline_filmstrip_min_height = (12 + (f * (height - 12) as f32).round() as u32).min(height);
                    }, cx)));
            }
        }
        body = body.child(appearance).child(heading("PREVIEW", t));
        if fs {
            body = body.child(self.timeline_panel_toggle("timeline-preview-inherit", "Use windowed preview setting", s.timeline_fullscreen_hover.is_none(), s.timeline_thumbnails,
                "Use the thumbnail preview setting selected for windowed mode.", |this, _, _, cx| { this.settings.timeline_fullscreen_hover = if this.settings.timeline_fullscreen_hover.is_none() { Some(this.settings.timeline_hover_enabled(false)) } else { None }; this.apply_settings(cx); }, cx));
        }
        body = body.child(self.timeline_panel_toggle("timeline-preview", "Thumbnail preview on hover", hover, s.timeline_thumbnails,
            "Display a thumbnail of the nearest sampled video frame while hovering over the timeline.", move |this, _, _, cx| {
                this.settings.set_timeline_hover(fs, !hover); this.apply_settings(cx);
            }, cx))
            .child(self.timeline_panel_toggle("timeline-cursor", "Show timeline preview cursor", cursor, true,
                "Mark the timestamp of the preview frame with a temporary vertical line.", |this, _, _, cx| {
                    this.settings.timeline_hover_playhead = !this.settings.timeline_hover_playhead; this.apply_settings(cx);
                }, cx))
            .child(heading("GENERATION", t));
        let video = self.player.with_state(|st| self.thumbnails.can_regenerate(st, s));
        let audio = self.player.with_state(|st| self.thumbnails.waveform.can_regenerate(st, s));
        body = body.child(self.timeline_panel_button("timeline-regenerate-video", "Regenerate filmstrip thumbnails", false, video,
            "Rebuild the sampled video frames and invalidate derived visualizations.", |this, _, _, cx| this.regenerate_thumbnails(cx), cx))
            .child(self.timeline_panel_button("timeline-regenerate-audio", "Generate / regenerate audio waveform", false, audio,
                "Rebuild the waveform for the selected audio stream.", |this, _, _, cx| { this.thumbnails.waveform.regenerate(&this.player, &this.settings); cx.notify(); }, cx))
            .child(self.timeline_panel_button("timeline-regenerate-all", "Regenerate all visualizations", false, video && audio,
                "Rebuild the filmstrip thumbnails and the audio waveform.", |this, _, _, cx| {
                    this.regenerate_thumbnails(cx); this.thumbnails.waveform.regenerate(&this.player, &this.settings); cx.notify();
                }, cx));
        let title = div().flex().flex_none().items_center().justify_between().gap_2().px_2().py_1().border_b_1().border_color(t.border)
            .child(div().flex().flex_col().child("Timeline settings").child(div().text_size(px(11.)).text_color(t.text_dim).child(if fs { "Fullscreen" } else { "Windowed" })))
            .child(div().id("timeline-settings-close").cursor_pointer().p_1().rounded(px(4.)).hover(|s| s.bg(t.hover))
                .child(icon(Icon::WindowClose, px(16.), t.text)).tooltip(tooltip("Close timeline settings", t))
                .on_click(cx.listener(|this, _, _, cx| this.close_menu(cx))));
        let panel = div().id("timeline-settings-panel").w(width).h(panel_height).flex().flex_col().rounded(px(8.))
            .bg(t.popup).border_1().border_color(t.border).shadow_lg().text_color(t.text).text_size(px(13.)).occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, window, cx| {
                if this.handle_drag_move(ev, window, cx) { cx.stop_propagation(); }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, window, cx| {
                if this.drag.is_some() { this.end_drag(window, cx); cx.stop_propagation(); }
            }))
            .child(title)
            .child(div().relative().flex_1().min_h_0().flex()
                .child(div().id("timeline-settings-scroll").flex_1().min_w_0().overflow_y_scroll().track_scroll(&scroll).child(body))
                .child(scrollbar("timeline-settings-scrollbar", &scroll, t)));
        anchored().position(origin).snap_to_window_with_margin(px(4.)).child(panel).into_any_element()
    }
}
