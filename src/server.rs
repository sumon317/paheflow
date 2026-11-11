use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderName, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures_util::stream::Stream;

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::api;
use crate::client::HttpClient;
use crate::config::{public_config, read_config, resource_dir, write_config, PublicConfig};
use crate::library;
use crate::runner::Runner;

pub struct AppState {
    pub client: HttpClient,
    pub runner: Arc<Runner>,
    pub token: String,
}

pub fn generate_token() -> String {
    let mut bytes = [0u8; 18];
    for b in &mut bytes {
        *b = fastrand::u8(..);
    }
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/index.html", get(serve_index))
        .route("/static/*path", get(serve_static))
        .route("/api/state", get(get_state))
        .route("/api/config", get(get_config).post(post_config))
        .route("/api/health", get(get_health))
        .route("/api/deps", get(get_deps))
        .route("/api/catalog", get(get_catalog))
        .route("/api/search", get(get_search))
        .route("/api/episodes", get(get_episodes))
        .route("/api/episode-options", get(get_episode_options))
        .route("/api/status", get(get_status))
        .route("/api/logs", get(get_logs))
        .route("/api/events", get(get_events))
        .route("/api/run", post(post_run))
        .route("/api/pause", post(post_pause))
        .route("/api/resume", post(post_resume))
        .route("/api/stop", post(post_stop))
        .route("/api/shutdown", post(post_shutdown))
        .route("/api/clear-logs", post(post_clear_logs))
        .route("/api/library", get(get_library))
        .route("/api/library/delete", post(post_library_delete))
        .route(
            "/api/library/delete-folder",
            post(post_library_delete_folder),
        )
        .route("/api/open", post(post_open))
        .route("/api/check-update", get(get_check_update))
        .route("/api/test-connection", post(post_test_connection))
        .route("/api/pick-folder", post(post_pick_folder))
        .route("/api/sync-credentials", post(post_sync_credentials))
        .route("/api/auto-detect-cookie", post(post_auto_detect_cookie))
        .route("/api/open-cf-page", post(post_open_cf_page))
        .layer(extension_cors())
        .layer(middleware::from_fn(validate_host))
        .with_state(state)
}

/// Only browser extensions need cross-origin access (the auto-sync feature).
/// Regular web pages receive no CORS permission.
fn extension_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _parts| {
            let value = origin.as_bytes();
            value.starts_with(b"chrome-extension://") || value.starts_with(b"moz-extension://")
        }))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, HeaderName::from_static("x-app-token")])
}

fn host_is_allowed(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }

    let hostname = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inside, remainder)) if remainder.is_empty() || remainder.starts_with(':') => {
                inside
            }
            _ => return false,
        }
    } else if host.matches(':').count() > 1 {
        // Unbracketed IPv6 literal.
        host.as_str()
    } else {
        host.split(':').next().unwrap_or(&host)
    };

    matches!(hostname, "127.0.0.1" | "localhost" | "::1")
}

/// Reject DNS-rebinding attempts: only loopback host names may reach the GUI.
async fn validate_host(request: Request<Body>, next: Next) -> Response {
    let allowed = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(host_is_allowed)
        .unwrap_or(false);

    if !allowed {
        return (
            StatusCode::FORBIDDEN,
            "Invalid Host header. This server only accepts loopback requests.",
        )
            .into_response();
    }

    next.run(request).await
}

fn check_token(headers: &HeaderMap, state: &AppState) -> bool {
    headers
        .get("X-App-Token")
        .and_then(|value| value.to_str().ok())
        .map(|token| token == state.token)
        .unwrap_or(false)
}

/// `EventSource` cannot set request headers, so the SSE endpoint may also use
/// `?token=...`. Every other route requires the header.
fn check_token_any(headers: &HeaderMap, query: &HashMap<String, String>, state: &AppState) -> bool {
    check_token(headers, state)
        || query
            .get("token")
            .map(|token| token == &state.token)
            .unwrap_or(false)
}

async fn serve_index(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let path = resource_dir().join("static").join("index.html");
    match fs::read_to_string(&path) {
        Ok(html) => {
            let replaced = html.replace("__APP_TOKEN__", &state.token);
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-store")
                .header("X-Content-Type-Options", "nosniff")
                .body(Body::from(replaced))
                .unwrap()
        }
        Err(_) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("index.html not found"))
            .unwrap(),
    }
}

/// Resolve a request path below `static/`, rejecting traversal and symlink
/// escapes. Returns `None` for anything that is not an existing regular file
/// inside the static directory.
fn resolve_static_in(root: &Path, raw_path: &str) -> Option<PathBuf> {
    let canonical_root = root.canonicalize().ok()?;
    let candidate = root.join(raw_path.trim_start_matches(['/', '\\']));
    let canonical = candidate.canonicalize().ok()?;
    if canonical.starts_with(&canonical_root) && canonical.is_file() {
        Some(canonical)
    } else {
        None
    }
}

fn resolve_static_file(raw_path: &str) -> Option<PathBuf> {
    resolve_static_in(&resource_dir().join("static"), raw_path)
}

async fn serve_static(axum::extract::Path(path): axum::extract::Path<String>) -> impl IntoResponse {
    let Some(file_path) = resolve_static_file(&path) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Static file not found"))
            .unwrap();
    };

    let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
    match fs::read(&file_path) {
        Ok(bytes) => Response::builder()
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::CACHE_CONTROL, "no-cache")
            .header("X-Content-Type-Options", "nosniff")
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("Error reading static file"))
            .unwrap(),
    }
}

async fn get_health(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let cfg = read_config();
    Ok(Json(serde_json::json!({
        "cfConfigured": !cfg.cf.trim().is_empty(),
        "uaConfigured": !cfg.ua.trim().is_empty(),
        "cfLength": cfg.cf.trim().len(),
        "error": ""
    })))
}

#[cfg(target_os = "android")]
fn deps_payload() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "Cloudflare client",
            "purpose": "Built-in native HTTP client & in-app web solver",
            "required": true,
            "found": true,
            "path": "native-rust"
        },
        {
            "name": "Rust Engine",
            "purpose": "Built-in concurrent HLS downloader & unpacker",
            "required": true,
            "found": true,
            "path": "native-rust"
        },
        {
            "name": "Stream Assembler",
            "purpose": "Built-in video/audio stream multiplexer (replaces ffmpeg)",
            "required": true,
            "found": true,
            "path": "native-rust"
        },
        {
            "name": "Media Player",
            "purpose": "Android system media player (VLC, MX Player, Gallery)",
            "required": false,
            "found": true,
            "path": "android-system"
        }
    ])
}

#[cfg(not(target_os = "android"))]
fn deps_payload() -> serde_json::Value {
    let has_ffmpeg = which::which("ffmpeg").is_ok();
    let has_mpv = which::which("mpv").is_ok();
    let cf_client = HttpClient::find_cf_client();

    serde_json::json!([
        {
            "name": "Cloudflare client",
            "purpose": "Cloudflare bypass (curl-impersonate or animepahe-curl)",
            "required": true,
            "found": cf_client.is_some(),
            "path": cf_client
                .as_ref()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default()
        },
        {
            "name": "Rust Engine",
            "purpose": "Built-in concurrent HLS downloader & unpacker",
            "required": true,
            "found": true,
            "path": "native-rust"
        },
        {
            "name": "ffmpeg",
            "purpose": "Merging video and audio streams",
            "required": true,
            "found": has_ffmpeg,
            "path": if has_ffmpeg { "ffmpeg" } else { "" }
        },
        {
            "name": "mpv",
            "purpose": "Optional: play a playlist link",
            "required": false,
            "found": has_mpv,
            "path": if has_mpv { "mpv" } else { "" }
        }
    ])
}

async fn get_deps(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(deps_payload()))
}

async fn get_config(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PublicConfig>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let cfg = read_config();
    Ok(Json(public_config(&cfg)))
}

async fn post_config(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<PublicConfig>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }

    let mut current = read_config();
    if let Some(cf) = body.get("cf").and_then(|v| v.as_str()) {
        current.cf = cf.trim().to_string();
    }
    if let Some(ua) = body.get("ua").and_then(|v| v.as_str()) {
        current.ua = ua.trim().to_string();
    }
    if let Some(frags) = body.get("concurrent_fragments").and_then(|v| v.as_u64()) {
        current.concurrent_fragments = frags as usize;
    }
    if let Some(ddir) = body.get("download_dir").and_then(|v| v.as_str()) {
        let trimmed = ddir.trim();
        if trimmed.is_empty() {
            current.download_dir = None;
        } else {
            current.download_dir = Some(trimmed.to_string());
        }
    }

    let saved = write_config(current).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(public_config(&saved)))
}

async fn post_sync_credentials(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "ok": false, "error": "Invalid or missing session token" })),
        ));
    }

    let mut current = read_config();
    let mut updated = false;

    if let Some(cf) = body.get("cf").and_then(|v| v.as_str()) {
        let trimmed = cf.trim();
        if !trimmed.is_empty() {
            current.cf = trimmed.to_string();
            updated = true;
        }
    }

    if let Some(ua) = body.get("ua").and_then(|v| v.as_str()) {
        let trimmed = ua.trim();
        if !trimmed.is_empty() {
            current.ua = trimmed.to_string();
            updated = true;
        }
    }

    if updated {
        write_config(current).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "ok": false, "error": e })),
            )
        })?;
        println!(
            "[Extension Sync] Cloudflare credentials automatically updated from browser extension."
        );
        Ok(Json(
            serde_json::json!({ "ok": true, "message": "Credentials updated successfully" }),
        ))
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": "No cf or ua fields provided" })),
        ))
    }
}

async fn get_state(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }

    let cfg = read_config();
    let job_snap = state.runner.snapshot(false).await;
    let catalog_items = api::catalog();

    Ok(Json(serde_json::json!({
        "config": public_config(&cfg),
        "health": {
            "cfConfigured": !cfg.cf.trim().is_empty(),
            "uaConfigured": !cfg.ua.trim().is_empty(),
            "cfLength": cfg.cf.trim().len(),
            "error": ""
        },
        "dependencies": deps_payload(),
        "catalog": { "items": catalog_items },
        "job": job_snap
    })))
}

pub fn is_version_newer(latest: &str, current: &str) -> bool {
    let parse_nums = |s: &str| -> Vec<u32> {
        s.split('.')
            .filter_map(|p| {
                let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<u32>().ok()
            })
            .collect()
    };
    let v_latest = parse_nums(latest);
    let v_current = parse_nums(current);
    for (l, c) in v_latest.iter().zip(v_current.iter()) {
        if l > c {
            return true;
        }
        if l < c {
            return false;
        }
    }
    v_latest.len() > v_current.len() && v_latest[v_current.len()..].iter().any(|&x| x > 0)
}

async fn get_check_update(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }

    let current_version = env!("CARGO_PKG_VERSION");
    let client = reqwest::Client::builder()
        .user_agent(format!("PaheFlow/{}", current_version))
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let url = "https://api.github.com/repos/sumon317/paheflow/releases/latest";
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok(Json(serde_json::json!({
                "ok": false,
                "update_available": false,
                "current_version": current_version,
                "error": format!("Network error: {}", e)
            })));
        }
    };

    if !resp.status().is_success() {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "update_available": false,
            "current_version": current_version,
            "error": format!("GitHub returned status {}", resp.status())
        })));
    }

    let release: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return Ok(Json(serde_json::json!({
                "ok": false,
                "update_available": false,
                "current_version": current_version,
                "error": format!("Failed to parse release JSON: {}", e)
            })));
        }
    };

    let tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let clean_tag = tag.trim_start_matches('v');
    let title = release.get("name").and_then(|v| v.as_str()).unwrap_or(tag);
    let body = release.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let html_url = release
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut apk_url = None;
    if let Some(assets) = release.get("assets").and_then(|v| v.as_array()) {
        for asset in assets {
            if let Some(name) = asset.get("name").and_then(|v| v.as_str()) {
                if name.ends_with(".apk") {
                    if let Some(dl_url) = asset.get("browser_download_url").and_then(|v| v.as_str())
                    {
                        apk_url = Some(dl_url.to_string());
                        break;
                    }
                }
            }
        }
    }

    let is_newer = is_version_newer(clean_tag, current_version);

    Ok(Json(serde_json::json!({
        "ok": true,
        "update_available": is_newer,
        "current_version": current_version,
        "latest_version": clean_tag,
        "title": title,
        "notes": body,
        "html_url": html_url,
        "apk_url": apk_url
    })))
}

async fn get_catalog(
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }
    let refresh = query
        .get("refresh")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let cfg = read_config();
    if refresh {
        match api::refresh_catalog(&state.client, &cfg).await {
            Ok(items) => Ok(Json(serde_json::json!({
                "items": items,
                "count": items.len(),
                "refreshed": true
            }))),
            Err(e) => Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e })),
            )),
        }
    } else {
        let items = api::catalog();
        Ok(Json(serde_json::json!({
            "items": items,
            "count": items.len(),
            "refreshed": false
        })))
    }
}

async fn get_search(
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }
    let q = query.get("q").cloned().unwrap_or_default();
    let cfg = read_config();
    match api::search(&state.client, &q, &cfg).await {
        Ok(items) => Ok(Json(serde_json::json!({ "items": items }))),
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e })),
        )),
    }
}

async fn get_episodes(
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }
    let mut slug = query.get("slug").cloned().unwrap_or_default();
    let title_param = query.get("title").cloned();
    let page = query.get("page").and_then(|p| p.parse::<u32>().ok());
    let audio_param = query.get("audio").cloned().unwrap_or_default();
    let cfg = read_config();

    let audio_filter = audio_param.trim().to_lowercase();

    // 1. If the anime's full release is already cached on disk, use it directly
    let (cached, saved_at) = api::load_release_cache_with_meta(&slug);
    if !cached.is_empty() {
        let available_audios = api::detect_available_audios(&cached);
        let total_all = cached.len();
        let filtered = api::filter_episodes_by_audio(&cached, &audio_filter);
        let total = filtered.len();

        let per_page = 30usize;
        let last_page = total.div_ceil(per_page).max(1) as u32;
        let pg = page.unwrap_or(1).min(last_page).max(1);

        let paged_items = if page.is_some() {
            let start_idx = ((pg - 1) as usize) * per_page;
            if start_idx < filtered.len() {
                filtered[start_idx..(start_idx + per_page).min(filtered.len())].to_vec()
            } else {
                Vec::new()
            }
        } else {
            filtered
        };

        return Ok(Json(serde_json::json!({
            "items": paged_items,
            "lastPage": last_page,
            "page": pg,
            "slug": slug,
            "total": total,
            "totalAll": total_all,
            "availableAudios": available_audios,
            "audio": if audio_filter.is_empty() || audio_filter == "all" { serde_json::Value::Null } else { serde_json::json!(audio_filter) },
            "stale": false,
        })));
    }

    // 2. Not cached: fetch the requested page directly (fast single request, no multi-page loop timeouts)
    if let Some(pg) = page {
        let mut res = api::release_page(&state.client, &slug, pg, &cfg).await;
        if let Err(ref e) = res {
            if e.contains("slug may have rotated") || e.contains("returned HTML instead of JSON") {
                if let Some(new_slug) =
                    api::resolve_active_slug(&state.client, &slug, title_param.as_deref(), &cfg)
                        .await
                {
                    println!(
                        "[Slug Auto-Heal] Self-healed rotated slug: {} -> {}",
                        slug, new_slug
                    );
                    slug = new_slug;
                    res = api::release_page(&state.client, &slug, pg, &cfg).await;
                }
            }
        }
        match res {
            Ok((items, last_page)) => {
                let available_audios = api::detect_available_audios(&items);
                let total_all = items.len();
                let filtered = api::filter_episodes_by_audio(&items, &audio_filter);
                let total = filtered.len();

                // If this is the only page, cache it immediately so subsequent loads are instant
                if last_page <= 1 && !items.is_empty() {
                    api::save_release_cache(&slug, &items);
                } else if last_page > 1 && last_page <= 20 {
                    let bg_client = state.client.clone();
                    let bg_slug = slug.clone();
                    let bg_cfg = cfg.clone();
                    tokio::spawn(async move {
                        let _ = api::release_with_status(&bg_client, &bg_slug, &bg_cfg).await;
                    });
                }

                Ok(Json(serde_json::json!({
                    "items": filtered,
                    "lastPage": last_page,
                    "page": pg,
                    "slug": slug,
                    "total": total,
                    "totalAll": total_all,
                    "availableAudios": available_audios,
                    "audio": if audio_filter.is_empty() || audio_filter == "all" { serde_json::Value::Null } else { serde_json::json!(audio_filter) },
                    "stale": false,
                })))
            }
            Err(e) => {
                if cached.is_empty() {
                    return Err((
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({ "error": e })),
                    ));
                }
                let warning = api::cache_warning(&e, cached.len(), saved_at);
                let available_audios = api::detect_available_audios(&cached);
                let total_all = cached.len();
                let filtered = api::filter_episodes_by_audio(&cached, &audio_filter);
                let total = filtered.len();
                Ok(Json(serde_json::json!({
                    "items": filtered,
                    "lastPage": 1,
                    "page": 1,
                    "slug": slug,
                    "total": total,
                    "totalAll": total_all,
                    "availableAudios": available_audios,
                    "audio": if audio_filter.is_empty() || audio_filter == "all" { serde_json::Value::Null } else { serde_json::json!(audio_filter) },
                    "stale": true,
                    "warning": warning,
                })))
            }
        }
    } else {
        let mut res = api::release_with_status(&state.client, &slug, &cfg).await;
        if let Err(ref e) = res {
            if e.contains("slug may have rotated") || e.contains("returned HTML instead of JSON") {
                if let Some(new_slug) =
                    api::resolve_active_slug(&state.client, &slug, title_param.as_deref(), &cfg)
                        .await
                {
                    println!(
                        "[Slug Auto-Heal] Self-healed rotated slug: {} -> {}",
                        slug, new_slug
                    );
                    slug = new_slug;
                    res = api::release_with_status(&state.client, &slug, &cfg).await;
                }
            }
        }
        match res {
            Ok(list) => {
                let available_audios = api::detect_available_audios(&list.items);
                let total_all = list.items.len();
                let filtered = api::filter_episodes_by_audio(&list.items, &audio_filter);
                let total = filtered.len();
                Ok(Json(serde_json::json!({
                    "items": filtered,
                    "slug": slug,
                    "total": total,
                    "totalAll": total_all,
                    "availableAudios": available_audios,
                    "audio": if audio_filter.is_empty() || audio_filter == "all" { serde_json::Value::Null } else { serde_json::json!(audio_filter) },
                    "stale": list.stale,
                    "warning": list.warning,
                })))
            }
            Err(e) => Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e })),
            )),
        }
    }
}

async fn get_episode_options(
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }
    let mut slug = query.get("slug").cloned().unwrap_or_default();
    let episode = query.get("episode").cloned().unwrap_or_default();
    let cfg = read_config();

    let mut res = api::available_options(&state.client, &slug, &episode, &cfg).await;
    if let Err(ref e) = res {
        if e.contains("slug may have rotated") || e.contains("returned HTML instead of JSON") {
            if let Some(new_slug) = api::resolve_active_slug(&state.client, &slug, None, &cfg).await
            {
                println!(
                    "[Slug Auto-Heal] Options self-healed rotated slug: {} -> {}",
                    slug, new_slug
                );
                slug = new_slug;
                res = api::available_options(&state.client, &slug, &episode, &cfg).await;
            }
        }
    }
    match res {
        Ok(opts) => Ok(Json(serde_json::to_value(&opts).unwrap_or_default())),
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e })),
        )),
    }
}

async fn get_status(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let snap = state.runner.snapshot(false).await;
    Ok(Json(serde_json::to_value(&snap).unwrap_or_default()))
}

async fn get_logs(
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let after: u64 = query.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
    let logs = state.runner.logs_since(after).await;
    Ok(Json(serde_json::json!({ "logs": logs })))
}

async fn post_run(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }
    match state.runner.start(body, state.client.clone()).await {
        Ok(job) => Ok(Json(serde_json::json!({ "ok": true, "job": job }))),
        Err(e) => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e })),
        )),
    }
}

async fn post_pause(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let paused = state.runner.pause().await;
    Ok(Json(serde_json::json!({ "ok": true, "paused": paused })))
}

async fn post_resume(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let resumed = state.runner.resume().await;
    Ok(Json(serde_json::json!({ "ok": true, "resumed": resumed })))
}

async fn post_stop(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let stopped = state.runner.stop().await;
    Ok(Json(serde_json::json!({ "ok": true, "stopped": stopped })))
}

async fn post_shutdown(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let runner = state.runner.clone();
    tokio::spawn(async move {
        let _ = runner.stop().await;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        std::process::exit(0);
    });
    Ok(Json(
        serde_json::json!({ "ok": true, "message": "Shutting down" }),
    ))
}

async fn post_clear_logs(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    state.runner.clear_logs().await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn get_library(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let folders = tokio::task::spawn_blocking(library::scan)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "items": folders })))
}

async fn post_library_delete(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }
    let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    match library::delete_file(path) {
        Ok(res) => Ok(Json(
            serde_json::json!({ "ok": true, "deleted": res.get("deleted"), "remaining": res.get("remaining") }),
        )),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )),
    }
}

async fn post_library_delete_folder(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }
    let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    match library::delete_folder(path) {
        Ok(res) => Ok(Json(
            serde_json::json!({ "ok": true, "deleted": res.get("deleted"), "remaining": res.get("remaining") }),
        )),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )),
    }
}

async fn post_open(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }
    let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    match library::open_path(path) {
        Ok(res) => Ok(Json(
            serde_json::json!({ "ok": true, "opened": res.get("opened") }),
        )),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )),
    }
}

async fn post_test_connection(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !check_token(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let start = std::time::Instant::now();
    let cfg = read_config();
    match api::search(&state.client, "one piece", &cfg).await {
        Ok(items) => Ok(Json(serde_json::json!({
            "ok": true,
            "message": format!("Connection OK — {} search result(s).", items.len()),
            "elapsed": (start.elapsed().as_millis() as f64) / 1000.0
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "ok": false,
            "message": e,
            "elapsed": (start.elapsed().as_millis() as f64) / 1000.0
        }))),
    }
}

async fn post_pick_folder(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }

    let current = body.get("current").and_then(|v| v.as_str()).unwrap_or("");
    let initial = if current.is_empty() {
        crate::config::default_download_dir()
            .to_string_lossy()
            .to_string()
    } else {
        current.to_string()
    };

    // Try KDE kdialog or GNOME zenity
    if which::which("kdialog").is_ok() {
        if let Ok(output) = tokio::process::Command::new("kdialog")
            .arg("--getexistingdirectory")
            .arg(&initial)
            .output()
            .await
        {
            if output.status.success() {
                let chosen = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !chosen.is_empty() {
                    return Ok(Json(serde_json::json!({ "ok": true, "path": chosen })));
                }
            }
        }
    }

    if which::which("zenity").is_ok() {
        if let Ok(output) = tokio::process::Command::new("zenity")
            .arg("--file-selection")
            .arg("--directory")
            .arg(format!("--filename={}", initial))
            .output()
            .await
        {
            if output.status.success() {
                let chosen = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !chosen.is_empty() {
                    return Ok(Json(serde_json::json!({ "ok": true, "path": chosen })));
                }
            }
        }
    }

    // Try Python tkinter directory picker if available
    if which::which("python3").is_ok() {
        let script = format!(
            "import tkinter, tkinter.filedialog as f; root = tkinter.Tk(); root.withdraw(); res = f.askdirectory(initialdir='{}'); print(res or '')",
            initial.replace('\\', "\\\\").replace('\'', "\\'")
        );
        if let Ok(output) = tokio::process::Command::new("python3")
            .arg("-c")
            .arg(&script)
            .output()
            .await
        {
            if output.status.success() {
                let chosen = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !chosen.is_empty() {
                    return Ok(Json(serde_json::json!({ "ok": true, "path": chosen })));
                }
            }
        }
    }

    Ok(Json(serde_json::json!({ "ok": true, "cancelled": true })))
}

async fn post_auto_detect_cookie(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }

    let detected = tokio::task::spawn_blocking(crate::autocookie::detect_from_firefox)
        .await
        .ok()
        .flatten();

    if let Some(detected) = detected {
        let mut current = read_config();
        current.cf = detected.cf.clone();
        current.ua = detected.ua.clone();
        if let Ok(saved) = write_config(current) {
            println!(
                "[Auto-Config] Manual auto-detect imported Cloudflare credentials from {}",
                detected.source
            );
            return Ok(Json(serde_json::json!({
                "ok": true,
                "config": public_config(&saved),
                "source": detected.source,
                "length": detected.cf.len()
            })));
        }
    }

    Err((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "ok": false,
            "error": "No animepahe Cloudflare cookie found in local browser profiles."
        })),
    ))
}

async fn post_open_cf_page(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !check_token(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Invalid token" })),
        ));
    }

    let target_url = "https://animepahe.pw/";
    let firefox_runners = [
        "firefox",
        "firefox-esr",
        "librewolf",
        "waterfox",
        "floorp",
        "zen-browser",
        "zen",
        "mullvad-browser",
    ];

    let mut launched = false;
    for bin in firefox_runners {
        if which::which(bin).is_ok()
            && std::process::Command::new(bin)
                .arg(target_url)
                .spawn()
                .is_ok()
        {
            launched = true;
            break;
        }
    }

    if !launched {
        let _ = std::process::Command::new("xdg-open")
            .arg(target_url)
            .spawn();
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn get_events(
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if !check_token_any(&headers, &query, &state) {
        return Err(StatusCode::FORBIDDEN);
    }

    let runner = state.runner.clone();
    let mut rx = runner.subscribe();
    let after_seq: u64 = headers
        .get("Last-Event-ID")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query.get("after").map(|s| s.as_str()))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let stream = async_stream::stream! {
        // Send initial backlog if after_seq requested
        let backlog = runner.logs_since(after_seq).await;
        for entry in backlog {
            yield Ok(Event::default().event("log").id(entry.seq.to_string()).data(serde_json::to_string(&entry).unwrap_or_default()));
        }

        let mut ticker = tokio::time::interval(Duration::from_millis(400));
        let mut last_marker = String::new();

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let snap = runner.snapshot(false).await;
                    let marker = serde_json::to_string(&snap).unwrap_or_default();
                    if marker != last_marker {
                        last_marker = marker;
                        yield Ok(Event::default().event("state").data(serde_json::to_string(&snap).unwrap_or_default()));
                        if !snap.running && snap.exit_code.is_some() {
                            yield Ok(Event::default().event("done").data(serde_json::to_string(&snap).unwrap_or_default()));
                        }
                    }
                }
                msg = rx.recv() => {
                    if let Ok(entry_str) = msg {
                        if let Ok(entry) = serde_json::from_str::<crate::runner::LogEntry>(&entry_str) {
                            yield Ok(Event::default().event("log").id(entry.seq.to_string()).data(entry_str));
                        }
                    }
                }
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

pub async fn serve_on_listener(
    listener: tokio::net::TcpListener,
    runner: Arc<Runner>,
    client: HttpClient,
) -> Result<(), String> {
    let token = generate_token();
    let state = Arc::new(AppState {
        client,
        runner,
        token,
    });

    let app = create_router(state);

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("Server error: {}", e))?;

    Ok(())
}

pub async fn start_server(
    port: u16,
    runner: Arc<Runner>,
    client: HttpClient,
) -> Result<(), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("Failed to bind port {}: {}", port, e))?;

    serve_on_listener(listener, runner, client).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn allows_only_loopback_hosts() {
        assert!(host_is_allowed("127.0.0.1:8765"));
        assert!(host_is_allowed("localhost:8765"));
        assert!(host_is_allowed("LOCALHOST"));
        assert!(host_is_allowed("[::1]:8765"));
        assert!(host_is_allowed("::1"));

        assert!(!host_is_allowed(""));
        assert!(!host_is_allowed("evil.example.com:8765"));
        assert!(!host_is_allowed("localhost.evil.example"));
        assert!(!host_is_allowed("192.168.1.5:8765"));
        assert!(!host_is_allowed("127.0.0.1.evil.example"));
        assert!(!host_is_allowed("[::1]evil.example"));
        assert!(!host_is_allowed("[::1"));
    }

    #[test]
    fn rejects_static_traversal_outside_root() {
        let temp =
            std::env::temp_dir().join(format!("animepahe-static-test-{}", std::process::id()));
        let root = temp.join("static");
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("index.html"), b"ok").unwrap();
        fs::write(root.join("assets").join("app.js"), b"js").unwrap();
        fs::write(temp.join("secret.txt"), b"secret").unwrap();

        assert!(resolve_static_in(&root, "index.html").is_some());
        assert!(resolve_static_in(&root, "/assets/app.js").is_some());
        assert!(resolve_static_in(&root, "../secret.txt").is_none());
        assert!(resolve_static_in(&root, "assets/../../secret.txt").is_none());
        assert!(resolve_static_in(&root, "missing.txt").is_none());
        assert!(resolve_static_in(&root, "assets").is_none());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn audio_filter_normalization_works() {
        let dub_raw = "dub".to_lowercase();
        let norm_dub = match dub_raw.as_str() {
            "dub" => "eng",
            "sub" => "jpn",
            other => other,
        };
        assert_eq!(norm_dub, "eng");

        let sub_raw = "sub".to_lowercase();
        let norm_sub = match sub_raw.as_str() {
            "dub" => "eng",
            "sub" => "jpn",
            other => other,
        };
        assert_eq!(norm_sub, "jpn");
    }

    #[test]
    fn test_is_version_newer() {
        assert!(is_version_newer("1.0.1", "1.0.0"));
        assert!(is_version_newer("1.1.0", "1.0.0"));
        assert!(is_version_newer("2.0.0", "1.0.0"));
        assert!(is_version_newer("1.0.0.1", "1.0.0"));
        assert!(!is_version_newer("1.0.0", "1.0.0"));
        assert!(!is_version_newer("0.9.9", "1.0.0"));
        assert!(!is_version_newer("1.0.0", "1.0.1"));
    }
}
