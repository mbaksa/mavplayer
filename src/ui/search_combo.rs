//! Searchable popup list shared by playlist and future collection selectors.

use super::player::PlayerView;
use super::text_input::TextInput;
use super::theme::Theme;
use super::widgets::icon;
use crate::fuzzy::{SearchField, SearchQuery};
use crate::icons::Icon;
use gpui::{
    AnyElement, Context, Entity, MouseButton, Pixels, Point, ScrollHandle, SharedString, Window, anchored, div, point,
    prelude::*, px,
};

struct Entry {
    id: usize,
    label: String,
    field: SearchField,
}

pub(super) struct SearchCombo {
    entries: Vec<Entry>,
    visible: Vec<usize>,
    selected: Option<usize>,
    active: Option<usize>,
}

impl SearchCombo {
    pub(super) fn new(labels: impl Iterator<Item = (usize, String)>, active: Option<usize>) -> Self {
        let entries: Vec<_> = labels.map(|(id, label)| Entry { id, field: SearchField::new(&label), label }).collect();
        let visible = (0..entries.len()).collect();
        let selected = entries
            .iter()
            .position(|entry| Some(entry.id) == active)
            .or_else(|| (!entries.is_empty()).then_some(0));
        Self { entries, visible, selected, active }
    }

    pub(super) fn filter(&mut self, text: &str) {
        let query = SearchQuery::new(text);
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                (query.is_empty() || query.score(std::slice::from_ref(&entry.field)).is_some()).then_some(i)
            })
            .collect();
        self.selected = self.visible.first().copied();
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected.filter(|i| self.visible.contains(i)).or_else(|| self.visible.first().copied())
    }

    pub(super) fn selected_id(&self) -> Option<usize> {
        self.selected_index().map(|i| self.entries[i].id)
    }

    pub(super) fn move_by(&mut self, delta: isize, scroll: &ScrollHandle) {
        if self.visible.is_empty() {
            self.selected = None;
            return;
        }
        let pos = self.selected_index().and_then(|i| self.visible.iter().position(|&entry| entry == i)).unwrap_or(0);
        let target = match delta {
            isize::MIN => 0,
            isize::MAX => self.visible.len() - 1,
            _ => pos.saturating_add_signed(delta).min(self.visible.len() - 1),
        };
        self.selected = Some(self.visible[target]);
        let offset = scroll.offset();
        let top = px(4. + target as f32 * 42.);
        let height = scroll.bounds().size.height;
        let y = if top < -offset.y {
            -top
        } else if top + px(42.) > -offset.y + height {
            -(top + px(42.) - height)
        } else {
            offset.y
        };
        scroll.set_offset(point(offset.x, y.clamp(-scroll.max_offset().height, px(0.))));
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &self,
        pos: Point<Pixels>,
        t: Theme,
        scroll: ScrollHandle,
        input: Option<Entity<TextInput>>,
        item_icon: Icon,
        on_select: impl Fn(&mut PlayerView, usize, &mut Window, &mut Context<PlayerView>) + Copy + 'static,
        window: &Window,
        cx: &mut Context<PlayerView>,
    ) -> AnyElement {
        let viewport = window.viewport_size();
        let width = px(400.).min((viewport.width - px(8.)).max(px(100.)));
        let count = self.visible.len();
        let selected = self.selected_index();
        let height = px(48. + count.max(1) as f32 * 42.).min(px(480.)).min((viewport.height - px(8.)).max(px(100.)));
        let mut rows = div().flex().flex_col().p_1();
        if self.visible.is_empty() {
            rows = rows.child(div().px_2().py_2().text_color(t.text_dim).child("No matching items"));
        }
        for &index in &self.visible {
            let entry = &self.entries[index];
            let id = entry.id;
            let active = Some(id) == self.active;
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("search-combo-{index}")))
                    .h(px(42.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .when(selected == Some(index), |d| d.bg(t.hover))
                    .when(active, |d| d.text_color(t.accent))
                    .hover(|s| s.bg(t.hover))
                    .child(icon(item_icon, px(18.), t.text_dim))
                    .child(div().flex_1().min_w_0().truncate().child(entry.label.clone()))
                    .when(active, |d| d.child(icon(Icon::Check, px(16.), t.accent)))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.close_menu(cx);
                        on_select(this, id, w, cx);
                        cx.notify();
                    })),
            );
        }
        let panel = div()
            .id("search-combo-panel")
            .w(width)
            .h(height)
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
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(t.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(32.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .bg(t.surface_alt)
                            .border_1()
                            .border_color(t.border)
                            .children(input),
                    )
                    .child(
                        div()
                            .id("search-combo-close")
                            .cursor_pointer()
                            .p_1()
                            .rounded(px(4.))
                            .hover(|s| s.bg(t.hover))
                            .child(icon(Icon::WindowClose, px(16.), t.text))
                            .on_click(cx.listener(|this, _, _, cx| this.close_menu(cx))),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id("search-combo-scroll")
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .track_scroll(&scroll)
                            .child(rows),
                    )
                    .child(super::widgets::scrollbar("search-combo-scrollbar", &scroll, t)),
            );
        anchored().position(pos).snap_to_window_with_margin(px(4.)).child(panel).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtering_preserves_identity_and_order() {
        let mut combo = SearchCombo::new(
            [(4, "Science Fiction"), (9, "Music"), (2, "Silent Films")].into_iter().map(|(id, title)| (id, title.to_owned())),
            Some(9),
        );
        assert_eq!(combo.selected_id(), Some(9));
        combo.filter("sci fic");
        assert_eq!(combo.selected_id(), Some(4));
        combo.filter("zzzz");
        assert_eq!(combo.selected_id(), None);
        combo.filter("");
        assert_eq!(combo.visible, vec![0, 1, 2]);
        assert_eq!(combo.selected_id(), Some(4));
    }
}
