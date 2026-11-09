#![allow(unused_imports)]
//! Core service layer: application state + all Modbus operations.
//!
//! This is a direct port of the old `src-tauri` command layer, with Tauri
//! removed. Instead of `#[tauri::command]` + `emit`, operations are plain
//! methods on [`Service`] and backend→UI pushes are delivered through a
//! [`tokio::sync::broadcast`] channel of [`Event`] values. UI views subscribe
//! and translate events into `cx.notify()`.

pub mod reg;
pub mod service;

pub use reg::*;
pub use service::*;
