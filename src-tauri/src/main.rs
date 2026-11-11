// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use animepahe_gui::{client::HttpClient, runner::Runner, server};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use tauri::Manager;

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            let (port_tx, port_rx) = mpsc::channel::<u16>();

            // Run the shared Axum backend on a dedicated Tokio runtime. The
            // webview then loads the normal GUI from that local server, so the
            // injected app token, static assets, and API endpoints all work the
            // same way as in the browser build.
            std::thread::spawn(move || {
                let runtime = match tokio::runtime::Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        eprintln!("Cannot create Tokio runtime: {err}");
                        return;
                    }
                };

                runtime.block_on(async move {
                    let mut bound = None;
                    for port in 8765..=8785 {
                        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                            Ok(listener) => {
                                bound = Some((port, listener));
                                break;
                            }
                            Err(_) => continue,
                        }
                    }

                    let Some((port, listener)) = bound else {
                        eprintln!("No free loopback port in 8765-8785 for the AnimePahe GUI server.");
                        return;
                    };

                    if port_tx.send(port).is_err() {
                        return;
                    }

                    let runner = Arc::new(Runner::new());
                    let client = HttpClient::new();
                    if let Err(err) = server::serve_on_listener(listener, runner, client).await {
                        eprintln!("AnimePahe GUI server error: {err}");
                    }
                });
            });

            let port = port_rx
                .recv_timeout(Duration::from_secs(10))
                .map_err(|_| "AnimePahe GUI backend did not start in time")?;

            if let Some(window) = handle.get_webview_window("main") {
                window.eval(&format!(
                    "window.location.replace('http://127.0.0.1:{port}/')"
                ))?;
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running AnimePahe GUI desktop application");
}
