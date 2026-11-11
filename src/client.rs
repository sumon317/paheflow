use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
#[cfg(not(target_os = "android"))]
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::{resource_dir, AppConfig};

pub const MIN_INTERVAL_MS: u64 = 400;
pub const TIMEOUT_SECS: u64 = 30;
pub const RATE_LIMIT_MARKER: &str = "error code: 1015";

#[derive(Clone)]
pub struct HttpClient {
    last_request: Arc<AtomicU64>,
    lock: Arc<Mutex<()>>,
    http: reqwest::Client,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .dns_resolver(Arc::new(crate::dns::SmartDnsResolver::new()))
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        Self {
            last_request: Arc::new(AtomicU64::new(0)),
            lock: Arc::new(Mutex::new(())),
            http,
        }
    }

    /// Locate a Cloudflare-capable HTTP client. Native curl-impersonate is
    /// preferred; the bundled `animepahe-curl` helper (curl_cffi) is a
    /// supported fallback for installations created by `requirements.sh`.
    pub fn find_cf_client() -> Option<PathBuf> {
        let project = resource_dir();

        let mut candidates = Vec::new();
        for name in ["curl-impersonate", "animepahe-curl"] {
            if let Ok(path) = which::which(name) {
                candidates.push(path);
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            candidates.push(home.join(".local/bin/curl-impersonate"));
            candidates.push(home.join(".local/bin/animepahe-curl"));
        }
        candidates.push(project.join(".uv/bin/curl-impersonate"));
        candidates.push(project.join(".uv/bin/animepahe-curl"));

        candidates.into_iter().find(|path| path.is_file())
    }

    /// Full client lookup, including a plain `curl` fallback for systems that
    /// do not have an impersonating client installed.
    pub fn find_client_binary() -> PathBuf {
        Self::find_cf_client()
            .or_else(|| which::which("curl").ok())
            .unwrap_or_else(|| PathBuf::from("curl"))
    }

    #[cfg(not(target_os = "android"))]
    fn find_impersonate_binary() -> PathBuf {
        Self::find_client_binary()
    }

    pub async fn throttle(&self) {
        let _guard = self.lock.lock().await;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last = self.last_request.load(Ordering::Relaxed);
        if last > 0 && now_ms < last + MIN_INTERVAL_MS {
            let wait = last + MIN_INTERVAL_MS - now_ms;
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
        let stored = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_request.store(stored, Ordering::Relaxed);
    }

    pub async fn fetch(
        &self,
        url: &str,
        referer: Option<&str>,
        cfg: &AppConfig,
    ) -> Result<String, String> {
        #[cfg(target_os = "android")]
        {
            return self.fetch_reqwest(url, referer, cfg).await;
        }

        #[cfg(not(target_os = "android"))]
        {
            let bin = Self::find_impersonate_binary();
            let bin_str = bin.to_string_lossy().to_string();
            let is_animepahe = url.contains("animepahe.");

            let mut last_error = String::from("Request to animepahe failed.");

            for attempt in 1..=4 {
                self.throttle().await;

                let mut cmd = Command::new(&bin);
                cmd.arg("-sS")
                    .arg("-L")
                    .arg("--compressed")
                    .arg("--max-time")
                    .arg(TIMEOUT_SECS.to_string())
                    .arg("--connect-timeout")
                    .arg("10");

                if bin_str.contains("impersonate") {
                    cmd.arg("--impersonate").arg("firefox147");
                }

                // Only attach AnimePahe's cf_clearance to animepahe domains.
                // Do NOT send it to kwik.cx or CDNs, which triggers a Cloudflare block!
                if is_animepahe && !cfg.cf.trim().is_empty() {
                    cmd.arg("-b").arg(format!("cf_clearance={}", cfg.cf.trim()));
                }

                if !cfg.ua.trim().is_empty() {
                    cmd.arg("-A").arg(cfg.ua.trim());
                }

                if let Some(ref_url) = referer {
                    cmd.arg("-H").arg(format!("Referer: {}", ref_url));
                }

                cmd.arg(url);

                let output_res = cmd.output().await;
                match output_res {
                    Ok(output) => {
                        let stdout_str = String::from_utf8_lossy(&output.stdout).to_string();
                        if stdout_str.contains(RATE_LIMIT_MARKER) {
                            last_error = String::from(
                                "animepahe.pw rate-limited the request (Cloudflare 1015). Retrying...",
                            );
                            tokio::time::sleep(Duration::from_millis(1000 * attempt)).await;
                            continue;
                        }

                        let lower = stdout_str.to_lowercase();
                        if lower.contains("just a moment...")
                            || lower.contains("cf-browser-verification")
                        {
                            return Err(String::from(
                                "Cloudflare is challenging this request (HTTP 403 challenge). Please update cf_clearance in Settings."
                            ));
                        }
                        if lower.contains("sorry, you have been blocked")
                            || lower.contains("cf-error-details")
                        {
                            return Err(String::from(
                                "Cloudflare refused this request. Your IP or cookie is blocked — copy a fresh cf_clearance into Settings."
                            ));
                        }

                        if output.status.success() {
                            return Ok(stdout_str);
                        } else {
                            let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();
                            last_error = if !stderr_str.trim().is_empty() {
                                stderr_str
                            } else {
                                format!(
                                    "HTTP request failed with status code {:?}",
                                    output.status.code()
                                )
                            };
                        }
                    }
                    Err(_) => {
                        // If CLI execution fails (e.g. command not found or SELinux error), fall back to reqwest
                        return self.fetch_reqwest(url, referer, cfg).await;
                    }
                }

                tokio::time::sleep(Duration::from_millis(600 * attempt)).await;
            }

            Err(last_error)
        }
    }

    pub async fn fetch_reqwest(
        &self,
        url: &str,
        referer: Option<&str>,
        cfg: &AppConfig,
    ) -> Result<String, String> {
        let is_animepahe = url.contains("animepahe.");
        let mut last_error = String::from("Request to animepahe failed.");

        for attempt in 1..=4 {
            self.throttle().await;

            let mut req = self.http.get(url);

            let ua = if !cfg.ua.trim().is_empty() {
                cfg.ua.trim()
            } else {
                "Mozilla/5.0 (Linux; Android 14; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Mobile Safari/537.36"
            };
            req = req.header(reqwest::header::USER_AGENT, ua);
            if url.contains("/api") {
                req = req.header(
                    reqwest::header::ACCEPT,
                    "application/json, text/javascript, */*; q=0.01",
                );
                req = req.header("X-Requested-With", "XMLHttpRequest");
            } else {
                req = req.header(
                    reqwest::header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
                );
            }
            req = req.header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9");

            if is_animepahe && !cfg.cf.trim().is_empty() {
                req = req.header(
                    reqwest::header::COOKIE,
                    format!("cf_clearance={}", cfg.cf.trim()),
                );
            }

            let effective_ref = referer.or(if is_animepahe {
                Some("https://animepahe.pw/")
            } else {
                None
            });
            if let Some(ref_url) = effective_ref {
                req = req.header(reqwest::header::REFERER, ref_url);
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = match resp.text().await {
                        Ok(t) => t,
                        Err(e) => {
                            last_error = format!("Failed to read response body: {}", e);
                            tokio::time::sleep(Duration::from_millis(600 * attempt)).await;
                            continue;
                        }
                    };

                    if text.contains(RATE_LIMIT_MARKER) {
                        last_error = String::from(
                            "animepahe.pw rate-limited the request (Cloudflare 1015). Retrying...",
                        );
                        tokio::time::sleep(Duration::from_millis(1000 * attempt)).await;
                        continue;
                    }

                    let lower = text.to_lowercase();
                    if lower.contains("just a moment...")
                        || lower.contains("cf-browser-verification")
                    {
                        return Err(String::from(
                            "Cloudflare is challenging this request (HTTP 403 challenge). Please update cf_clearance in Settings.",
                        ));
                    }
                    if lower.contains("sorry, you have been blocked")
                        || lower.contains("cf-error-details")
                    {
                        return Err(String::from(
                            "Cloudflare refused this request. Your IP or cookie is blocked — copy a fresh cf_clearance into Settings.",
                        ));
                    }

                    if status.is_success() {
                        return Ok(text);
                    } else if status == reqwest::StatusCode::NOT_FOUND {
                        return Err(format!("HTTP request failed with status code {}", status));
                    } else {
                        last_error = format!("HTTP request failed with status code {}", status);
                    }
                }
                Err(err) => {
                    last_error = format!("HTTP client request failed: {}", err);
                }
            }

            tokio::time::sleep(Duration::from_millis(600 * attempt)).await;
        }

        Err(last_error)
    }

    pub async fn fetch_json(
        &self,
        url: &str,
        referer: Option<&str>,
        cfg: &AppConfig,
    ) -> Result<serde_json::Value, String> {
        let body = self.fetch(url, referer, cfg).await?;
        serde_json::from_str(&body).map_err(|e| {
            if body.contains("<html") {
                "animepahe.pw returned HTML instead of JSON — slug may have rotated or cookie expired.".to_string()
            } else {
                format!("Failed to parse JSON response: {}", e)
            }
        })
    }

    pub async fn fetch_bytes(
        &self,
        url: &str,
        referer: Option<&str>,
        cfg: &AppConfig,
    ) -> Result<(Vec<u8>, String), String> {
        let is_animepahe = url.contains("animepahe.");
        let mut last_error = String::from("Failed to download image.");

        for attempt in 1..=3 {
            self.throttle().await;

            let mut req = self.http.get(url);

            let ua = if !cfg.ua.trim().is_empty() {
                cfg.ua.trim()
            } else {
                "Mozilla/5.0 (Linux; Android 14; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Mobile Safari/537.36"
            };
            req = req.header(reqwest::header::USER_AGENT, ua);
            req = req.header(
                reqwest::header::ACCEPT,
                "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
            );
            req = req.header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9");

            if is_animepahe && !cfg.cf.trim().is_empty() {
                req = req.header(
                    reqwest::header::COOKIE,
                    format!("cf_clearance={}", cfg.cf.trim()),
                );
            }

            let effective_ref = referer.or(if is_animepahe {
                Some("https://animepahe.pw/")
            } else {
                None
            });
            if let Some(ref_url) = effective_ref {
                req = req.header(reqwest::header::REFERER, ref_url);
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let content_type = resp
                            .headers()
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("image/jpeg")
                            .to_string();
                        match resp.bytes().await {
                            Ok(bytes) => return Ok((bytes.to_vec(), content_type)),
                            Err(e) => {
                                last_error = format!("Failed to read image bytes: {}", e);
                            }
                        }
                    } else {
                        last_error = format!("HTTP request failed with status code {}", status);
                    }
                }
                Err(err) => {
                    last_error = format!("HTTP client request failed: {}", err);
                }
            }

            tokio::time::sleep(Duration::from_millis(400 * attempt)).await;
        }

        Err(last_error)
    }
}
