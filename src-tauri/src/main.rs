// Modbus Workbench — Tauri 2.x application entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod state;
mod simreg;

use state::AppState;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                if window.label() == "main" {
                    std::process::exit(0);
                }
            }
        })
        .setup(|app| {
            // Native frosted-glass vibrancy (requires macOSPrivateApi: true).
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;
                use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
                if let Some(window) = app.get_webview_window("main") {
                    let _ = apply_vibrancy(
                        &window,
                        NSVisualEffectMaterial::HudWindow,
                        None,
                        Some(18.0),
                    );
                }
            }

            // 常驻自动变化循环：按各寄存器的 vary 配置驱动（无需手动开关）。
            commands::spawn_vary_loop(app.handle());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::connect_tcp,
            commands::connect_rtu,
            commands::connect_rtu_over_tcp,
            commands::disconnect,
            commands::conn_info,
            commands::read_registers,
            commands::write_register,
            commands::write_point,
            commands::read_points,
            commands::send_raw,
            commands::send_raw_frame,
            commands::start_poll,
            commands::stop_poll,
            commands::stop_all_polls,
            commands::sim_slave_start,
            commands::sim_slave_stop,
            commands::sim_slave_stop_all,
            commands::sim_slave_status,
            commands::sim_set_register,
            commands::sim_set_input,
            commands::sim_set_coil,
            commands::sim_set_discrete,
            commands::sim_reset,
            commands::sim_snapshot,
            commands::load_config,
            commands::save_config,
            commands::list_serial_ports,
            commands::sim_reg_list,
            commands::sim_reg_add,
            commands::sim_reg_update,
            commands::sim_reg_delete,
            commands::sim_reg_seed,
            commands::unit_list,
            commands::unit_add,
            commands::unit_remove,
            commands::sim_reg_add_batch,
            commands::sim_reg_export_xlsx,
            commands::sim_reg_import_xlsx,
            commands::export_log_txt,
            commands::save_project_file,
            commands::import_project_file,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Modbus Tool");
}
