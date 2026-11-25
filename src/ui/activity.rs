//! Reusable indicator for cancellable background work, with a live tooltip.

use super::theme::Theme;
use gpui::{AnyElement, Bounds, Context, Hsla, Pixels, Window, canvas, div, point, prelude::*, px, size};
use std::rc::Rc;
use std::time::Duration;

/// A caller may supply another painter; the default is a rotating dot ring.
pub(super) type ActivityAnimation = fn(Bounds<Pixels>, &mut Window, Hsla);

pub(super) fn indicator(
    id: &'static str,
    theme: Theme,
    label: impl Fn() -> String + 'static,
    animation: Option<ActivityAnimation>,
) -> AnyElement {
    let label: Rc<dyn Fn() -> String> = Rc::new(label);
    let paint = animation.unwrap_or(dot_ring);
    div().id(id).size(px(18.)).flex_none().flex().items_center().justify_center()
        .tooltip(move |_, cx| {
            let label = label.clone();
            cx.new(|cx: &mut Context<ActivityTooltip>| {
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(Duration::from_millis(100)).await;
                        if this.update(cx, |_, cx| cx.notify()).is_err() { break; }
                    }
                }).detach();
                ActivityTooltip { label, theme }
            }).into()
        })
        .child(canvas(|_, _, _| (), move |bounds, _, window, _| {
            paint(bounds, window, theme.text_dim);
            window.request_animation_frame();
        }).size(px(16.)))
        .into_any_element()
}

struct ActivityTooltip {
    label: Rc<dyn Fn() -> String>,
    theme: Theme,
}

impl Render for ActivityTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().px_2().py_1().rounded(px(5.))
            .bg(self.theme.popup).border_1().border_color(self.theme.border)
            .text_color(self.theme.text).text_size(px(12.)).shadow_md()
            .flex().flex_col()
            .children((self.label)().lines().map(|line| div().child(line.to_owned())))
    }
}

fn dot_ring(bounds: Bounds<Pixels>, window: &mut Window, color: Hsla) {
    let phase = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() / 100 % 8) as usize;
    for i in 0..8 {
        let angle = i as f32 * std::f32::consts::TAU / 8.0;
        let p = point(bounds.origin.x + px(7. + angle.cos() * 5.), bounds.origin.y + px(7. + angle.sin() * 5.));
        let mut color = color;
        color.a *= 0.2 + 0.8 * ((i + 8 - phase) % 8) as f32 / 7.0;
        window.paint_quad(gpui::fill(Bounds::new(p, size(px(2.), px(2.))), color));
    }
}
