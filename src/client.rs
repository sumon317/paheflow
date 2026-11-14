use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
#[cfg(not(target_os = "android"))]
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::{resource_dir, AppConfig};

pub const MIN_INTERVAL_MS: u64 = 350;
pub const TIMEOUT_SECS: u64 = 30;
pub const RATE_LIMIT_MARKER: &str = "error code: 1015";

const ANIMEPAHE_DOMAINS: &[&str] = &[
    "animepahe.pw",
    "animepahe.com",
    "animepahe.org",
    "animepahe.ru",
    "animepahe.si",
];

pub fn is_animepahe_url(raw_url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw_url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let lower = host.to_ascii_lowercase();
    ANIMEPAHE_DOMAINS
        .iter()
        .any(|d| lower == *d || lower.ends_with(&format!(".{}", d)))
}

#[cfg(any(not(target_os = "android"), test))]
fn is_rate_limit_text(lower_text: &str) -> bool {
    lower_text.contains(RATE_LIMIT_MARKER)
        || lower_text.contains("too many requests")
        || lower_text.contains("rate limit")
        || lower_text.contains("error 1015")
        || lower_text.contains("returned error: 429")
        || lower_text.contains("http/1.1 429")
        || lower_text.contains("http/2 429")
        || lower_text.contains("http/3 429")
        || lower_text.contains("status code 429")
}

fn is_cloudflare_challenge_body(is_success_status: Option<bool>, lower_body: &str) -> bool {
    let explicit_marker = lower_body.contains("<title>just a moment")
        || lower_body.contains("cf-browser-verification")
        || lower_body.contains("_cf_chl_opt")
        || lower_body.contains("/cdn-cgi/challenge-platform");
    if explicit_marker {
        return true;
    }
    is_success_status == Some(false) && lower_body.contains("just a moment...")
}

fn extract_chrome_major(ua: &str) -> Option<&str> {
    let idx = ua.find("Chrome/").or_else(|| ua.find("Chromium/"))?;
    let after = &ua[idx..];
    let slash = after.find('/')? + 1;
    let ver = &after[slash..];
    let major = ver.split(['.', ' ']).next()?;
    if !major.is_empty() && major.chars().all(|c| c.is_ascii_digit()) {
        Some(major)
    } else {
        None
    }
}

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
            .timeout(Duration::from_secs(60))
            .read_timeout(Duration::from_secs(25))
            .connect_timeout(Duration::from_secs(12))
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

    pub fn enforce_backoff(&self, backoff_ms: u64) {
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
            + backoff_ms;
        self.last_request.store(future_ms, Ordering::Relaxed);
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
            let is_animepahe = is_animepahe_url(url);

            let mut last_error = String::from("Request to animepahe failed.");

            for attempt in 1..=5 {
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
                        let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();
                        let combined = format!("{}\n{}", stdout_str, stderr_str);
                        let lower_combined = combined.to_lowercase();

                        let is_rate_limit = is_rate_limit_text(&lower_combined);

                        if is_rate_limit {
                            let backoff_ms = (1200 * (1 << attempt)).min(16000) + (attempt * 150);
                            eprintln!(
                                "[HttpClient] Rate limit reached on animepahe (curl 429/1015). Backing off for {}ms (attempt {}/5)...",
                                backoff_ms, attempt
                            );
                            self.enforce_backoff(backoff_ms);
                            last_error = format!(
                                "animepahe.pw rate-limited requests (HTTP 429 Too Many Requests). Backing off {}s...",
                                backoff_ms / 1000
                            );
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            continue;
                        }

                        if is_cloudflare_challenge_body(
                            Some(output.status.success()),
                            &lower_combined,
                        ) {
                            return Err(String::from(
                                "Cloudflare is challenging this request (HTTP 403 challenge). Tap 'Solve Cloudflare' or update cf_clearance in Settings.",
                            ));
                        }
                        if lower_combined.contains("sorry, you have been blocked")
                            || lower_combined.contains("cf-error-details")
                        {
                            return Err(String::from(
                                "Cloudflare refused this request (IP or cookie blocked). Tap 'Solve Cloudflare' or copy a fresh cf_clearance into Settings.",
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
        let is_animepahe = is_animepahe_url(url);
        let mut last_error = String::from("Request to animepahe failed.");

        for attempt in 1..=5 {
            self.throttle().await;

            let mut req = self.http.get(url);

            let ua = if !cfg.ua.trim().is_empty() {
                cfg.ua.trim()
            } else {
                "Mozilla/5.0 (Linux; Android 14; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Mobile Safari/537.36"
            };
            req = req.header(reqwest::header::USER_AGENT, ua);
            if let Some(major) = extract_chrome_major(ua) {
                let sec_ch_ua = format!(
                    r#""Not(A:Brand";v="99", "Google Chrome";v="{0}", "Chromium";v="{0}""#,
                    major
                );
                let is_mobile = if ua.contains("Mobile") { "?1" } else { "?0" };
                let platform = if ua.contains("Android") {
                    r#""Android""#
                } else if ua.contains("Windows") {
                    r#""Windows""#
                } else if ua.contains("Mac OS") {
                    r#""macOS""#
                } else {
                    r#""Linux""#
                };
                req = req.header("sec-ch-ua", sec_ch_ua);
                req = req.header("sec-ch-ua-mobile", is_mobile);
                req = req.header("sec-ch-ua-platform", platform);
            }
            let fetch_site = if is_animepahe {
                "same-origin"
            } else {
                "cross-site"
            };
            if url.contains("/api") {
                req = req.header(
                    reqwest::header::ACCEPT,
                    "application/json, text/javascript, */*; q=0.01",
                );
                req = req.header("X-Requested-With", "XMLHttpRequest");
                req = req.header("sec-fetch-dest", "empty");
                req = req.header("sec-fetch-mode", "cors");
                req = req.header("sec-fetch-site", fetch_site);
            } else {
                req = req.header(
                    reqwest::header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
                );
                req = req.header("sec-fetch-dest", "document");
                req = req.header("sec-fetch-mode", "navigate");
                req = req.header("sec-fetch-site", fetch_site);
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
                    let is_429 =
                        status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 429;

                    let retry_after_secs = if is_429 {
                        resp.headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                    } else {
                        None
                    };

                    let text = match resp.text().await {
                        Ok(t) => t,
                        Err(e) => {
                            last_error = format!("Failed to read response body: {}", e);
                            tokio::time::sleep(Duration::from_millis(600 * attempt)).await;
                            continue;
                        }
                    };

                    let lower = text.to_lowercase();
                    let is_rate_limit = is_429
                        || text.contains(RATE_LIMIT_MARKER)
                        || lower.contains("too many requests")
                        || lower.contains("rate limit")
                        || lower.contains("error 1015");

                    if is_rate_limit {
                        let backoff_ms = if let Some(secs) = retry_after_secs {
                            (secs * 1000).max(2000)
                        } else {
                            (1200 * (1 << attempt)).min(16000) + (attempt * 150)
                        };
                        eprintln!(
                            "[HttpClient] Rate limit reached on animepahe (HTTP {} / 1015). Backing off for {}ms (attempt {}/5)...",
                            status, backoff_ms, attempt
                        );
                        self.enforce_backoff(backoff_ms);
                        last_error = format!(
                            "animepahe.pw rate-limited requests (HTTP 429 Too Many Requests). Backing off {}s...",
                            backoff_ms / 1000
                        );
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    }

                    if is_cloudflare_challenge_body(Some(status.is_success()), &lower) {
                        return Err(String::from(
                            "Cloudflare is challenging this request (HTTP 403 challenge). Tap 'Solve Cloudflare' or update cf_clearance in Settings.",
                        ));
                    }
                    if lower.contains("sorry, you have been blocked")
                        || lower.contains("cf-error-details")
                    {
                        return Err(String::from(
                            "Cloudflare refused this request (IP or cookie blocked). Tap 'Solve Cloudflare' or copy a fresh cf_clearance into Settings.",
                        ));
                    }

                    if status.is_success() {
                        return Ok(text);
                    } else if status == reqwest::StatusCode::NOT_FOUND
                        || status == reqwest::StatusCode::GONE
                    {
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
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return Err(
                "animepahe.pw returned an empty response — slug may have rotated.".to_string(),
            );
        }
        serde_json::from_str(trimmed).map_err(|e| {
            let lower = trimmed.to_ascii_lowercase();
            if lower.contains("<html") || lower.starts_with("<!doctype") || trimmed.starts_with('<') {
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
        let is_animepahe = is_animepahe_url(url);
        let mut last_error = String::from("Failed to download image.");

        for attempt in 1..=3 {
            // Images / posters are hosted on high-bandwidth CDNs; do not throttle them.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animepahe_url_checks_hostname_only() {
        assert!(is_animepahe_url("https://animepahe.pw/api?m=search&q=test"));
        assert!(is_animepahe_url("https://i.animepahe.com/posters/1.jpg"));
        assert!(is_animepahe_url("https://animepahe.si/anime/abc"));
        assert!(!is_animepahe_url("https://kwik.cx/e/123?ref=animepahe.pw"));
        assert!(!is_animepahe_url("https://evil-animepahe.pw/api"));
        assert!(!is_animepahe_url("not-a-url"));
    }

    #[test]
    fn rate_limit_detection_ignores_bare_429_numbers() {
        assert!(!is_rate_limit_text("episode 429 released (429 mb)"));
        assert!(is_rate_limit_text("http/2 429 \n too many requests"));
        assert!(is_rate_limit_text(
            "curl: (22) the requested url returned error: 429"
        ));
        assert!(is_rate_limit_text("cloudflare error code: 1015"));
    }

    #[test]
    fn cloudflare_challenge_avoids_false_positives_on_200_ok() {
        assert!(!is_cloudflare_challenge_body(
            Some(true),
            r#"{"title":"just a moment... in time"}"#
        ));
        assert!(is_cloudflare_challenge_body(
            Some(false),
            "just a moment... please wait"
        ));
        assert!(is_cloudflare_challenge_body(
            Some(true),
            "<html><head><title>Just a moment...</title></head><body>_cf_chl_opt</body></html>"
        ));
    }

    #[test]
    fn chrome_major_version_extracted_only_for_chromium_uas() {
        assert_eq!(
            extract_chrome_major("Mozilla/5.0 Chrome/134.0.0.0 Mobile Safari/537.36"),
            Some("134")
        );
        assert_eq!(
            extract_chrome_major(
                "Mozilla/5.0 (X11; Linux x86_64; rv:147.0) Gecko/20100101 Firefox/147.0"
            ),
            None
        );
    }
}
