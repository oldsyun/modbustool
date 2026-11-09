//! Shared UI scaffolding: theme constants, small widget helpers, and the
//! event bridge that pumps `Service` broadcast events into gpui entities.

use std::sync::Arc;

use gpui_kit::gpui::{
    Context, FocusHandle, IntoElement, ParentElement, Styled, Task,
};
use gpui_kit::component::StyledExt;

use crate::core::service::{Event, Service};

// ── theme ────────────────────────────────────────────────────────
pub mod theme {
    use gpui_kit::gpui::Rgba;

    pub fn bg() -> Rgba { rgb(0x1e1e1e) }
    pub fn panel() -> Rgba { rgb(0x252526) }
    pub fn surface() -> Rgba { rgb(0x2d2d30) }
    pub fn border() -> Rgba { rgb(0x3e3e42) }
    pub fn text() -> Rgba { rgb(0xe6e6e6) }
    pub fn text_dim() -> Rgba { rgb(0x9a9a9a) }
    pub fn accent() -> Rgba { rgb(0x0071e3) }
    pub fn green() -> Rgba { rgb(0x34c759) }
    pub fn red() -> Rgba { rgb(0xff3b30) }
    pub fn amber() -> Rgba { rgb(0xff9f0a) }

    pub fn rgb(hex: u32) -> Rgba {
        gpui_kit::gpui::rgb(hex)
    }
}

/// A tiny labelled field wrapper used across forms.
pub fn field(label: &str, control: impl IntoElement) -> impl IntoElement {
    use gpui_kit::gpui::div;
    div()
        .v_flex()
        .gap_1()
        .child(
            div()
                .text_sm()
                .text_color(theme::text_dim())
                .child(label.to_string()),
        )
        .child(control)
}

/// Pump `Service` broadcast events into a UI entity.
///
/// Spawns a tokio task that receives events and calls `handler` for each on
/// the UI thread via `cx.spawn`. The returned task must be kept alive by the
/// view (store it in a field) — dropping it stops the pump.
pub fn bridge_events<V, F>(
    service: Arc<Service>,
    cx: &mut Context<V>,
    handler: F,
) -> Task<()>
where
    V: 'static,
    F: FnMut(&mut V, Event, &mut Context<V>) + 'static,
{
    let mut rx = service.subscribe();
    let mut handler = handler;
    cx.spawn(async move |this, cx| {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let ok = this
                        .update(cx, |view, cx| {
                            handler(view, ev, cx);
                            cx.notify();
                        })
                        .is_ok();
                    if !ok {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    })
}

/// Convenience: a focus handle initializer.
pub fn init_focus(cx: &mut Context<impl gpui_kit::gpui::Render>) -> FocusHandle {
    cx.focus_handle()
}
