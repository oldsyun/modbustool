//! Shared UI scaffolding: theme constants, small widget helpers, and the
//! event bridge that pumps `Service` broadcast events into gpui entities.

use std::sync::Arc;

use gpui_kit::gpui::{
    Context, FocusHandle, IntoElement, ParentElement, Styled, Task,
};
use gpui_kit::component::StyledExt;

use crate::core::service::{Event, Service};

// ── theme ────────────────────────────────────────────────────────
// Light theme, matching the original Tauri/web UI palette (src/style.css):
//   --bg #f5f5f7  --surface #fff  --border #d2d2d7 / #e8e8ed
//   --text #1d1d1f  --text-secondary #6e6e73  --accent #0071e3
pub mod theme {
    use gpui_kit::gpui::Rgba;

    /// Page background (main gradient base).
    pub fn bg() -> Rgba { rgb(0xf5f5f7) }

    /// Toolbar / panel / card background.
    pub fn panel() -> Rgba { rgb(0xffffff) }

    /// Slightly recessed surface (table headers, inputs background).
    pub fn surface() -> Rgba { rgb(0xfafafa) }

    /// Strong border (dividers, table lines).
    pub fn border() -> Rgba { rgb(0xd2d2d7) }

    /// Light border (subtle separators).
    pub fn border_light() -> Rgba { rgb(0xe8e8ed) }

    /// Primary text.
    pub fn text() -> Rgba { rgb(0x1d1d1f) }

    /// Secondary / dim text.
    pub fn text_dim() -> Rgba { rgb(0x6e6e73) }

    /// Tertiary text (placeholders, hints).
    pub fn text_tertiary() -> Rgba { rgb(0xaeaeb2) }

    /// Accent (links, focus, active tab).
    pub fn accent() -> Rgba { rgb(0x0071e3) }

    pub fn green() -> Rgba { rgb(0x34c759) }
    pub fn red() -> Rgba { rgb(0xff3b30) }
    pub fn amber() -> Rgba { rgb(0xff9f0a) }

    /// Zebra striping for table rows (very light gray).
    pub fn row_alt() -> Rgba { rgb(0xf5f5f7) }

    /// Header background (slightly tinted).
    pub fn header_bg() -> Rgba { rgb(0xf5f5f7) }

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
