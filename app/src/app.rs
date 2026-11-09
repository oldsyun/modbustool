//! Application bootstrap: create a Tokio runtime (the Modbus service is built
//! on tokio primitives), enter it for the lifetime of the UI, then open the
//! three top-level windows.

use std::sync::Arc;

use gpui_kit::gpui::{px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};

use crate::core::service::Service;
use crate::ui::main_window::MainWindow;
use crate::ui::scanner_window::ScannerWindow;
use crate::ui::simulator_window::SimulatorWindow;

pub fn run() {
    // Own a multi-threaded Tokio runtime and keep it alive for the app's
    // lifetime. Entering it makes tokio primitives (Mutex locks, broadcast,
    // spawn, sleep) usable from gpui's main thread and its async tasks.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _guard = runtime.enter();

    let service = Service::new();
    // Background loops run on this runtime.
    runtime.block_on(async {
        // spawn_vary_loop uses tokio::spawn, so must run inside runtime.
        service.spawn_vary_loop();
    });
    // Keep the runtime handle in a thread-local/global so background tasks
    // scheduled later (polls, slave servers) also use it. Handles obtained
    // within `enter()` use the current runtime already; tasks spawned from UI
    // callbacks run in gpui's executor but their tokio primitives resolve to
    // this runtime because the guard is held on the main thread.
    std::mem::forget(runtime); // keep runtime alive for process lifetime

    let app_service = service.clone();
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        // Force the component library's built-in theme to Light so buttons,
        // inputs and other widgets match the app's light palette (and don't
        // follow a dark system appearance).
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Light, None, cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let svc = app_service.clone();
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui_kit::gpui::TitlebarOptions {
                    title: Some("Modbus Tool".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            cx,
            move |window, cx| cx.new(|cx| MainWindow::new(svc.clone(), window, cx)),
        )
        .expect("open main window");

        cx.activate(true);
    });
}

pub fn open_simulator(service: Arc<Service>, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1095.), px(800.)), cx);
    let svc = service.clone();
    gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui_kit::gpui::TitlebarOptions {
                title: Some("从站模拟器".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        move |window, cx| cx.new(|cx| SimulatorWindow::new(svc.clone(), window, cx)),
    )
    .ok();
}

pub fn open_scanner(service: Arc<Service>, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(720.), px(760.)), cx);
    let svc = service.clone();
    gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui_kit::gpui::TitlebarOptions {
                title: Some("Unit ID 扫描".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        move |window, cx| cx.new(|cx| ScannerWindow::new(svc.clone(), window, cx)),
    )
    .ok();
}
