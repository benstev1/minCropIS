// No console window behind the app in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Desktop app: the minIS web app in its own window. The server runs inside the
//! app on a private local port; the window shows its page. Nothing is kept on
//! disk once the app closes.

use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

const TITLE: &str = "Bens Super Cool Image Stabilizer";

/// Longest the app waits on exit for a running job to stop and its files to be deleted.
const EXIT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Where uploads and results live while the app runs.
fn jobs_dir(app: &AppHandle) -> tauri::Result<PathBuf> {
    Ok(app.path().app_cache_dir()?.join("jobs"))
}

fn main() {
    // Tells the server to stop and clean up, and reports back when it has.
    let (stop_server, server_stopped) = tokio::sync::oneshot::channel::<()>();
    let (cleaned_up, wait_for_cleanup) = std::sync::mpsc::channel::<()>();

    let app = tauri::Builder::default()
        .setup(move |app| {
            // Use the bundled ffmpeg when present; otherwise (e.g. in development)
            // ffmpeg is looked up on PATH.
            if let Ok(resources) = app.path().resource_dir() {
                let dir = resources.join("ffmpeg");
                let exe = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
                if dir.join(exe).exists() {
                    minis::video::set_tool_dir(dir);
                }
            }

            // Anything left from a run that didn't exit cleanly (e.g. a crash).
            let data_dir = jobs_dir(app.handle())?;
            let _ = std::fs::remove_dir_all(&data_dir);

            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            listener.set_nonblocking(true)?;
            tauri::async_runtime::spawn(async move {
                let shutdown = async {
                    let _ = server_stopped.await;
                };
                let result = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => minis::web::serve(listener, data_dir, shutdown).await,
                    Err(e) => Err(e.into()),
                };
                if let Err(e) = result {
                    eprintln!("server stopped: {e:#}");
                }
                let _ = cleaned_up.send(());
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
        .build(tauri::generate_context!())
        .expect("error while building the app");

    let mut stop_server = Some(stop_server);
    app.run(move |app, event| {
        if let RunEvent::Exit = event {
            // Stop any running job and delete every upload and result.
            if let Some(stop) = stop_server.take() {
                let _ = stop.send(());
                let _ = wait_for_cleanup.recv_timeout(EXIT_CLEANUP_TIMEOUT);
            }
            if let Ok(dir) = jobs_dir(app) {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    });
}
