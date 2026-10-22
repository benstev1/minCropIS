// No console window behind the app in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Desktop app: the minIS web app in its own window. The server runs inside the
//! app on a private local port; the window shows its page.

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

const TITLE: &str = "Bens Super Cool Image Stabilizer";

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // Use the bundled ffmpeg when present; otherwise (e.g. in development)
            // ffmpeg is looked up on PATH.
            if let Ok(resources) = app.path().resource_dir() {
                let dir = resources.join("ffmpeg");
                let exe = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
                if dir.join(exe).exists() {
                    minis::video::set_tool_dir(dir);
                }
            }

            // Jobs only live as long as the app, so files left by earlier runs are
            // unreachable; clear them out. Results the user saved live elsewhere.
            let data_dir = app.path().app_cache_dir()?.join("jobs");
            let _ = std::fs::remove_dir_all(&data_dir);

            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            listener.set_nonblocking(true)?;
            tauri::async_runtime::spawn(async move {
                let result = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => minis::web::serve(listener, data_dir).await,
                    Err(e) => Err(e.into()),
                };
                if let Err(e) = result {
                    eprintln!("server stopped: {e:#}");
                }
            });

            let url = format!("http://127.0.0.1:{port}/").parse()?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title(TITLE)
                .inner_size(1280.0, 860.0)
                .min_inner_size(480.0, 600.0)
                // Let the page handle dropped videos itself, as it does in a browser.
                .disable_drag_drop_handler()
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the app");
}
