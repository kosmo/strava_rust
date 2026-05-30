// Suppress the console window on Windows release builds.
// On macOS the terminal is suppressed by using `cargo tauri build` (.app bundle).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod database;
mod map_server;
mod strava;
mod tiles;

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // Start the axum map server in Tauri's async runtime
            tauri::async_runtime::spawn(async {
                if let Err(e) = map_server::serve_map_server().await {
                    eprintln!("Map server error: {}", e);
                }
            });

            // Open the main window pointing to the local server.
            // The axum server starts nearly instantly, so the WebView
            // request arrives after the server is ready.
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External("http://127.0.0.1:8080".parse().unwrap()),
            )
            .title("Strava Map")
            .inner_size(1200.0, 900.0)
            .resizable(true)
            .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
