use animepahe_gui::{client, runner, server};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::var("ANIMEPAHE_GUI_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .filter(|p| *p > 0)
        .unwrap_or(8765);

    let client = client::HttpClient::new();
    let runner = Arc::new(runner::Runner::new());

    // Bind before launching the browser so a busy port is reported instead of
    // opening a dead page, and so port 0 resolves to a real port.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            let url = format!("http://127.0.0.1:{}", port);
            if is_instance_running(port).await {
                println!("PaheFlow is already running at {}.", url);
                let no_browser = std::env::var("ANIMEPAHE_GUI_NO_BROWSER")
                    .map(|v| v == "1")
                    .unwrap_or(false);
                if !no_browser {
                    open_app_window(&url);
                }
                return Ok(());
            }
            return Err(format!("Failed to bind http://{}: {}", addr, e).into());
        }
    };
    let actual_port = listener
        .local_addr()
        .map(|bound| bound.port())
        .unwrap_or(port);
    let url = format!("http://127.0.0.1:{}", actual_port);

    // Auto-detect fresh Cloudflare cookies from local browser profiles off the critical path
    tokio::task::spawn_blocking(|| {
        let _ = animepahe_gui::autocookie::try_auto_import();
    });

    let no_browser = std::env::var("ANIMEPAHE_GUI_NO_BROWSER")
        .map(|v| v == "1")
        .unwrap_or(false);
    if !no_browser {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            open_app_window(&url);
        });
    }

    println!("==================================================");
    println!("  PaheFlow — Native Anime Downloader");
    println!("  Running on: http://127.0.0.1:{}", actual_port);
    println!("  Press Ctrl+C to stop.");
    println!("==================================================");

    let runner_for_stop = runner.clone();
    tokio::select! {
        res = server::serve_on_listener(listener, runner, client) => {
            if let Err(e) = res {
                eprintln!("Server error: {}", e);
                std::process::exit(1);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down PaheFlow...");
            runner_for_stop.stop().await;
        }
    }

    Ok(())
}

fn open_app_window(url: &str) {
    // Strictly use Firefox and Firefox-based browsers (never Chromium-based)
    let firefox_runners = [
        ("firefox", vec!["--new-window", url]),
        ("firefox-esr", vec!["--new-window", url]),
        ("librewolf", vec!["--new-window", url]),
        ("waterfox", vec!["--new-window", url]),
        ("floorp", vec!["--new-window", url]),
        ("zen-browser", vec!["--new-window", url]),
        ("zen", vec!["--new-window", url]),
        ("mullvad-browser", vec!["--new-window", url]),
    ];

    for (bin, args) in firefox_runners {
        if which::which(bin).is_ok() && std::process::Command::new(bin).args(&args).spawn().is_ok()
        {
            println!("  Launched in Firefox window via {}", bin);
            return;
        }
    }

    // Fallback to system default browser
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

async fn is_instance_running(port: u16) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let url = format!("http://127.0.0.1:{}/", port);
    client
        .get(&url)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}
