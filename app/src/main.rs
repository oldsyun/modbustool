mod app;
mod ui;

// Re-use the library crate's core module so both the binary and the
// integration tests share the exact same service layer.
pub use modbus_tool_gpui::core;

fn main() {
    app::run();
}
