use chrono::TimeZone;
use futures_util::stream::{self, StreamExt};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::client::HttpClient;
use crate::config::{data_dir, resource_dir, AppConfig};

pub const HOST: &str = "https://animepahe.pw";
pub const API_URL: &str = "https://animepahe.pw/api";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchItem {
    pub slug: String,
    pub title: String,
    pub r#type: String,
    pub episodes: serde_json::Value,
    pub status: String,
    pub score: serde_json::Value,
    pub year: serde_json::Value,
    pub poster: String,
    pub cover: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeItem {
    pub episode: serde_json::Value,
    pub audio: String,
    pub created_at: String,
    pub duration: String,
    pub session: String,
    pub episode2: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaySource {
    pub url: String,
    pub resolution: String,
    pub audio: String,
    pub av1: String,
    pub kwik: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesize: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableOptions {
    pub episode: serde_json::Value,
    pub session: String,
    pub resolutions: Vec<String>,
    pub audios: Vec<String>,
    pub combinations: Vec<serde_json::Value>,
    pub source_count: usize,
    #[serde(default)]
    pub filesizes: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub slug: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestReleaseItem {
    pub id: Option<u64>,
    pub anime_id: Option<u64>,
    pub anime_title: String,
    pub anime_session: String,
    pub episode: serde_json::Value,
    pub snapshot: Option<String>,
    pub duration: Option<String>,
    pub session: Option<String>,
    pub created_at: Option<String>,
    pub audio: Option<String>,
    #[serde(default)]
    pub has_sub: bool,
    #[serde(default)]
    pub has_dub: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesize: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub filesizes: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolutions: Vec<String>,
    #[serde(default)]
    pub meta_resolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestMetaQuery {
    pub slug: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub episode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestMetaResult {
    pub slug: String,
    pub session: String,
    pub episode: String,
    pub has_sub: bool,
    pub has_dub: bool,
    pub audios: Vec<String>,
    pub resolutions: Vec<String>,
    pub filesizes: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesize: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestReleasesPage {
    pub page: u32,
    pub current_page: u32,
    pub last_page: u32,
    #[serde(rename = "lastPage")]
    pub last_page_camel: u32,
    pub total: usize,
    pub items: Vec<LatestReleaseItem>,
    pub data: Vec<LatestReleaseItem>,
}

pub fn ep_matches(a: &str, b: &str) -> bool {
    let a_clean = a.trim();
    let b_clean = b.trim();
    if a_clean.is_empty() || b_clean.is_empty() {
        return false;
    }
    if a_clean.eq_ignore_ascii_case(b_clean) {
        return true;
    }
    let a_trimmed = a_clean.trim_start_matches('0');
    let b_trimmed = b_clean.trim_start_matches('0');
    let a_norm = if a_trimmed.is_empty() { "0" } else { a_trimmed };
    let b_norm = if b_trimmed.is_empty() { "0" } else { b_trimmed };
    a_norm == b_norm
}

fn cache_dir() -> PathBuf {
    let dir = data_dir().join("cache");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn cache_file(slug: &str) -> PathBuf {
    let safe: String = slug
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    cache_dir().join(format!("{}.json", safe))
}

pub fn load_release_cache_with_meta(slug: &str) -> (Vec<EpisodeItem>, Option<i64>) {
    let path = cache_file(slug);
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            let saved_at = val.get("savedAt").and_then(|v| v.as_i64());
            if let Some(items) = val.get("items") {
                if let Ok(episodes) = serde_json::from_value::<Vec<EpisodeItem>>(items.clone()) {
                    return (episodes, saved_at);
                }
            }
        }
    }
    (Vec::new(), None)
}

pub fn load_release_cache(slug: &str) -> Vec<EpisodeItem> {
    load_release_cache_with_meta(slug).0
}

pub fn cache_warning(error: &str, count: usize, saved_at: Option<i64>) -> String {
    let when = saved_at
        .and_then(|ts| chrono::Local.timestamp_opt(ts, 0).single())
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "an earlier run".to_string());
    format!(
        "Live episode fetch failed ({}). Showing {} cached episode(s) saved at {}.",
        error, count, when
    )
}

pub fn save_release_cache(slug: &str, episodes: &[EpisodeItem]) {
    if episodes.is_empty() {
        return;
    }
    let path = cache_file(slug);
    let payload = serde_json::json!({
        "slug": slug,
        "savedAt": chrono::Utc::now().timestamp(),
        "items": episodes
    });
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, payload.to_string()).is_ok() {
        let _ = fs::rename(&tmp, &path);
    } else {
        let _ = fs::remove_file(&tmp);
    }
}

fn options_cache_file(slug: &str, episode_num: &str) -> PathBuf {
    let safe_slug: String = slug
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let safe_ep: String = episode_num
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    cache_dir().join(format!("{}.opts.{}.json", safe_slug, safe_ep))
}

pub fn load_options_cache(slug: &str, episode_num: &str) -> Option<AvailableOptions> {
    let path = options_cache_file(slug, episode_num);
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(opts) = serde_json::from_str::<AvailableOptions>(&content) {
            return Some(opts);
        }
    }
    None
}

pub fn save_options_cache(slug: &str, episode_num: &str, opts: &AvailableOptions) {
    let path = options_cache_file(slug, episode_num);
    if let Ok(content) = serde_json::to_string(opts) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, content).is_ok() {
            let _ = fs::rename(&tmp, &path);
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }
}

pub async fn search(
    client: &HttpClient,
    query: &str,
    cfg: &AppConfig,
) -> Result<Vec<SearchItem>, String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("{}?m=search&q={}", API_URL, urlencoding::encode(trimmed));
    let json = client.fetch_json(&url, None, cfg).await?;

    let mut results = Vec::new();
    if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
        for item in data {
            results.push(SearchItem {
                slug: item
                    .get("session")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                title: item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                r#type: item
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                episodes: item
                    .get("episodes")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                status: item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                score: item
                    .get("score")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                year: item.get("year").cloned().unwrap_or(serde_json::Value::Null),
                poster: item
                    .get("poster")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                cover: item
                    .get("cover")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    Ok(results)
}

fn episode_value_str(ep: &serde_json::Value) -> String {
    match ep {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.trim().to_string(),
        _ => String::new(),
    }
}

pub fn pick_preferred_filesize(
    resolutions: &[String],
    filesizes: &HashMap<String, String>,
    preferred_res: &str,
) -> Option<String> {
    let pref = preferred_res.trim();
    if !pref.is_empty() {
        if let Some(sz) = filesizes.get(pref) {
            return Some(format!("{} ({}p)", sz, pref));
        }
    }
    for r in resolutions {
        if let Some(sz) = filesizes.get(r) {
            return Some(format!("{} ({}p)", sz, r));
        }
    }
    filesizes
        .iter()
        .next()
        .map(|(r, sz)| format!("{} ({}p)", sz, r))
}

pub fn audio_flags_from_list(audios: &[String], fallback_dub: bool) -> (bool, bool) {
    if audios.is_empty() {
        return (true, fallback_dub);
    }
    let has_sub = audios
        .iter()
        .any(|a| a.eq_ignore_ascii_case("jpn") || a.eq_ignore_ascii_case("sub"));
    let has_dub = audios
        .iter()
        .any(|a| a.eq_ignore_ascii_case("eng") || a.eq_ignore_ascii_case("dub"));
    (has_sub, has_dub)
}

pub fn parse_latest_release_item(it: &serde_json::Value) -> Option<LatestReleaseItem> {
    parse_latest_release_item_with_pref(it, "")
}

pub fn parse_latest_release_item_with_pref(
    it: &serde_json::Value,
    preferred_res: &str,
) -> Option<LatestReleaseItem> {
    let anime_title = it
        .get("anime_title")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let anime_session = it
        .get("anime_session")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if anime_title.is_empty() || anime_session.is_empty() {
        return None;
    }

    let raw_audio = it
        .get("audio")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let title_lower = anime_title.to_lowercase();
    let is_dub_hint = raw_audio == "dub"
        || raw_audio == "eng"
        || title_lower.contains("(dub)")
        || title_lower.contains("[dub]")
        || title_lower.contains(" dub");
    let audio = if is_dub_hint {
        Some("dub".to_string())
    } else {
        Some("sub".to_string())
    };

    let episode = it
        .get("episode")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let ep_str = episode_value_str(&episode);
    let ep_session = it
        .get("session")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut has_sub = true;
    let mut has_dub = is_dub_hint;
    let mut filesize = None;
    let mut filesizes = HashMap::new();
    let mut resolutions = Vec::new();
    let mut meta_resolved = false;

    if !ep_str.is_empty() {
        if let Some(cached) = load_options_cache(&anime_session, &ep_str) {
            let session_ok = ep_session
                .as_deref()
                .map(|s| s.is_empty() || cached.session == s)
                .unwrap_or(true);
            if session_ok && !cached.filesizes.is_empty() {
                let (sub_flag, dub_flag) = audio_flags_from_list(&cached.audios, is_dub_hint);
                has_sub = sub_flag;
                has_dub = dub_flag || is_dub_hint;
                filesize =
                    pick_preferred_filesize(&cached.resolutions, &cached.filesizes, preferred_res);
                filesizes = cached.filesizes;
                resolutions = cached.resolutions;
                meta_resolved = true;
            }
        }
    }

    Some(LatestReleaseItem {
        id: it.get("id").and_then(|v| v.as_u64()),
        anime_id: it.get("anime_id").and_then(|v| v.as_u64()),
        anime_title,
        anime_session,
        episode,
        snapshot: it.get("snapshot").and_then(|v| v.as_str()).map(|s| {
            let s = s.trim();
            let mut url = if s.starts_with("//") {
                format!("https:{}", s)
            } else if s.starts_with('/') {
                format!("https://animepahe.pw{}", s)
            } else if !s.starts_with("http://") && !s.starts_with("https://") && !s.is_empty() {
                format!("https://{}", s)
            } else {
                s.to_string()
            };
            if url.contains("i.animepahe.ru") {
                url = url.replace("i.animepahe.ru", "i.animepahe.com");
            } else if url.contains("i.animepahe.si") {
                url = url.replace("i.animepahe.si", "i.animepahe.com");
            }
            url
        }),
        duration: it
            .get("duration")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        session: ep_session,
        created_at: it
            .get("created_at")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        audio,
        has_sub,
        has_dub,
        filesize,
        filesizes,
        resolutions,
        meta_resolved,
    })
}

pub fn merge_latest_release_items(items: Vec<LatestReleaseItem>) -> Vec<LatestReleaseItem> {
    let mut merged: Vec<LatestReleaseItem> = Vec::with_capacity(items.len());
    for item in items {
        let ep_str = episode_value_str(&item.episode);
        if let Some(existing) = merged.iter_mut().find(|e| {
            e.anime_session == item.anime_session
                && ep_matches(&episode_value_str(&e.episode), &ep_str)
        }) {
            existing.has_sub = existing.has_sub || item.has_sub;
            existing.has_dub = existing.has_dub || item.has_dub;
            if existing.has_dub {
                existing.audio = Some("dub".to_string());
            }
            if existing.snapshot.as_deref().unwrap_or("").is_empty() {
                existing.snapshot = item.snapshot;
            }
            if existing.duration.as_deref().unwrap_or("").is_empty() {
                existing.duration = item.duration;
            }
            if existing.session.as_deref().unwrap_or("").is_empty() {
                existing.session = item.session;
            }
            if existing.filesize.is_none() && item.filesize.is_some() {
                existing.filesize = item.filesize;
            }
            for (k, v) in item.filesizes {
                existing.filesizes.entry(k).or_insert(v);
            }
            if existing.resolutions.is_empty() && !item.resolutions.is_empty() {
                existing.resolutions = item.resolutions;
            }
            existing.meta_resolved = existing.meta_resolved || item.meta_resolved;
        } else {
            merged.push(item);
        }
    }
    merged
}

pub async fn latest_releases(
    client: &HttpClient,
    page: u32,
    cfg: &AppConfig,
    all: bool,
) -> Result<LatestReleasesPage, String> {
    let pref_res = "";
    if all {
        const MAX_PAGES: u32 = 5;
        let url = format!("{}?m=airing&page=1", API_URL);
        let json = match client.fetch_json(&url, None, cfg).await {
            Ok(j) => j,
            Err(e) => {
                let alt_url = format!("{}/?m=airing&page=1", API_URL);
                match client.fetch_json(&alt_url, None, cfg).await {
                    Ok(j) => j,
                    Err(_) => return Err(format!("Failed to fetch recent releases: {}", e)),
                }
            }
        };

        let lp = json
            .get("last_page")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(1);

        let total = json
            .get("total")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(0);

        let mut all_items = Vec::new();
        if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
            for it in data {
                if let Some(item) = parse_latest_release_item_with_pref(it, pref_res) {
                    all_items.push(item);
                }
            }
        }

        if lp > 1 {
            let target_pages: Vec<u32> = (2..=lp.min(MAX_PAGES)).collect();
            for p in target_pages {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let u = format!("{}?m=airing&page={}", API_URL, p);
                if let Ok(page_json) = client.fetch_json(&u, None, cfg).await {
                    if let Some(data) = page_json.get("data").and_then(|d| d.as_array()) {
                        for it in data {
                            if let Some(item) = parse_latest_release_item_with_pref(it, pref_res) {
                                all_items.push(item);
                            }
                        }
                    }
                }
            }
        }

        let merged = merge_latest_release_items(all_items);
        Ok(LatestReleasesPage {
            page: 1,
            current_page: 1,
            last_page: 1,
            last_page_camel: 1,
            total: if total > 0 { total } else { merged.len() },
            items: merged.clone(),
            data: merged,
        })
    } else {
        // Single page fetch
        let url = format!("{}?m=airing&page={}", API_URL, page.max(1));
        let json = match client.fetch_json(&url, None, cfg).await {
            Ok(j) => j,
            Err(_) => {
                let alt_url = format!("{}/?m=airing&page={}", API_URL, page.max(1));
                client.fetch_json(&alt_url, None, cfg).await?
            }
        };

        let page_val = json
            .get("current_page")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(page);

        let last_page = json
            .get("last_page")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(1);

        let total = json
            .get("total")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(0);

        let mut items = Vec::new();
        if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
            for it in data {
                if let Some(item) = parse_latest_release_item_with_pref(it, pref_res) {
                    items.push(item);
                }
            }
        }

        let merged = merge_latest_release_items(items);
        Ok(LatestReleasesPage {
            page: page_val,
            current_page: page_val,
            last_page,
            last_page_camel: last_page,
            total: if total > 0 { total } else { merged.len() },
            items: merged.clone(),
            data: merged,
        })
    }
}

pub async fn enrich_latest_item_meta(
    client: &HttpClient,
    queries: Vec<LatestMetaQuery>,
    cfg: &AppConfig,
) -> Vec<LatestMetaResult> {
    let capped: Vec<LatestMetaQuery> = queries
        .into_iter()
        .filter(|q| !q.slug.trim().is_empty() && !q.session.trim().is_empty())
        .take(20)
        .collect();

    stream::iter(capped)
        .map(|q| {
            let client = client.clone();
            let cfg = cfg.clone();
            async move {
                let ep_val = q
                    .episode
                    .trim()
                    .parse::<u64>()
                    .map(serde_json::Value::from)
                    .unwrap_or_else(|_| serde_json::Value::String(q.episode.clone()));
                let opts = match episode_options_by_session(
                    &client, &q.slug, &q.session, &q.episode, ep_val, &cfg,
                )
                .await
                {
                    Ok(o) => o,
                    Err(_) => return None,
                };
                let (has_sub, has_dub) = audio_flags_from_list(&opts.audios, false);
                let filesize = pick_preferred_filesize(&opts.resolutions, &opts.filesizes, "");
                Some(LatestMetaResult {
                    slug: q.slug,
                    session: q.session,
                    episode: q.episode,
                    has_sub,
                    has_dub,
                    audios: opts.audios,
                    resolutions: opts.resolutions,
                    filesizes: opts.filesizes,
                    filesize,
                })
            }
        })
        .buffer_unordered(3)
        .filter_map(|x| async move { x })
        .collect()
        .await
}

pub async fn release_page(
    client: &HttpClient,
    slug: &str,
    page: u32,
    cfg: &AppConfig,
) -> Result<(Vec<EpisodeItem>, u32), String> {
    let url = format!(
        "{}?m=release&id={}&sort=episode_asc&page={}",
        API_URL,
        urlencoding::encode(slug),
        page.max(1)
    );
    let json = match client.fetch_json(&url, None, cfg).await {
        Ok(j) => j,
        Err(e) if is_rotated_or_missing_error(&e) => {
            let alt_url = format!(
                "{}/?m=release&id={}&sort=episode_asc&page={}",
                API_URL,
                urlencoding::encode(slug),
                page.max(1)
            );
            client.fetch_json(&alt_url, None, cfg).await?
        }
        Err(e) => return Err(e),
    };

    let last_page = json
        .get("last_page")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(1);

    let mut items = Vec::new();
    if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
        for item in data {
            items.push(EpisodeItem {
                episode: item
                    .get("episode")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                audio: item
                    .get("audio")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                created_at: item
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                duration: item
                    .get("duration")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                session: item
                    .get("session")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                episode2: item
                    .get("episode2")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            });
        }
    }

    Ok((items, last_page))
}

#[derive(Debug, Clone)]
pub struct ReleaseList {
    pub items: Vec<EpisodeItem>,
    pub stale: bool,
    pub warning: Option<String>,
}

pub async fn release_with_status(
    client: &HttpClient,
    slug: &str,
    cfg: &AppConfig,
) -> Result<ReleaseList, String> {
    let mut all_episodes = Vec::new();
    let mut seen_sessions = HashSet::new();

    let (first_items, last_page) = match release_page(client, slug, 1, cfg).await {
        Ok(res) => res,
        Err(err) => {
            let (cached, saved_at) = load_release_cache_with_meta(slug);
            if !cached.is_empty() {
                let warning = cache_warning(&err, cached.len(), saved_at);
                return Ok(ReleaseList {
                    items: cached,
                    stale: true,
                    warning: Some(warning),
                });
            }
            return Err(err);
        }
    };

    let first_count = first_items.len();
    for item in first_items {
        if !item.session.is_empty() && seen_sessions.insert(item.session.clone()) {
            all_episodes.push(item);
        }
    }

    let mut page_errors = Vec::new();
    const MAX_RELEASE_PAGES: u32 = 100;
    if last_page > MAX_RELEASE_PAGES {
        page_errors.push(format!(
            "only the first {} of {} pages were fetched",
            MAX_RELEASE_PAGES, last_page
        ));
    }

    if last_page > 1 {
        let target_pages: Vec<u32> = (2..=last_page.min(MAX_RELEASE_PAGES)).collect();
        for &page in &target_pages {
            match release_page(client, slug, page, cfg).await {
                Ok((page_items, _)) => {
                    for item in page_items {
                        if !item.session.is_empty() && seen_sessions.insert(item.session.clone()) {
                            all_episodes.push(item);
                        }
                    }
                }
                Err(err) => {
                    let lower_err = err.to_lowercase();
                    let already_rate_limited = lower_err.contains("429")
                        || lower_err.contains("rate-limit")
                        || lower_err.contains("rate limit")
                        || lower_err.contains("1015");
                    if already_rate_limited {
                        page_errors.push(format!("page {}: {}", page, err));
                    } else {
                        // Back off 1.5s on transient error and retry once
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        match release_page(client, slug, page, cfg).await {
                            Ok((page_items, _)) => {
                                for item in page_items {
                                    if !item.session.is_empty()
                                        && seen_sessions.insert(item.session.clone())
                                    {
                                        all_episodes.push(item);
                                    }
                                }
                            }
                            Err(_) => page_errors.push(format!("page {}: {}", page, err)),
                        }
                    }
                }
            }
        }
    }

    all_episodes.sort_by(|a, b| {
        let a_num = match &a.episode {
            serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
            serde_json::Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
            _ => 0.0,
        };
        let b_num = match &b.episode {
            serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
            serde_json::Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
            _ => 0.0,
        };
        a_num
            .partial_cmp(&b_num)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Save whenever full or we fetched multiple pages successfully
    if page_errors.is_empty() || all_episodes.len() > first_count {
        save_release_cache(slug, &all_episodes);
    }

    Ok(ReleaseList {
        items: all_episodes,
        stale: false,
        warning: if page_errors.is_empty() {
            None
        } else {
            Some(format!(
                "Some episode pages could not be loaded ({}). The list may be incomplete and was not cached.",
                page_errors.join("; ")
            ))
        },
    })
}

pub async fn release(
    client: &HttpClient,
    slug: &str,
    cfg: &AppConfig,
) -> Result<Vec<EpisodeItem>, String> {
    Ok(release_with_status(client, slug, cfg).await?.items)
}

pub fn format_bytes(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.0} MB", (bytes as f64 / MB as f64).round())
    }
}

pub fn normalize_size_string(size: &str) -> String {
    let clean = size
        .trim()
        .trim_matches(|c| c == '(' || c == ')' || c == '[' || c == ']');
    if let Ok(re) = Regex::new(r#"(?i)(\d+(?:\.\d+)?)\s*(MB|GB|KB)"#) {
        if let Some(caps) = re.captures(clean) {
            let val = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let unit = caps
                .get(2)
                .map(|m| m.as_str().to_uppercase())
                .unwrap_or_default();
            return format!("{} {}", val, unit);
        }
    }
    clean.to_string()
}

pub fn parse_sources_from_html(html: &str) -> Vec<PlaySource> {
    let btn_re = Regex::new(r#"(?is)<button\b([^>]*)>(.*?)</button>"#).ok();
    let attr_re = Regex::new(r#"(?i)data-(src|resolution|audio|av1)\s*=\s*["']([^"']*)["']"#).ok();
    let size_re = Regex::new(r#"(?i)(\d+(?:\.\d+)?)\s*(MB|GB|KB)"#).ok();
    let res_re = Regex::new(r#"(?i)\b(360|480|720|1080)p?\b"#).ok();
    let audio_re = Regex::new(r#"(?i)\b(eng|dub|jpn|sub)\b"#).ok();

    let mut sources = Vec::new();

    // 1. Parse streaming buttons on the player page
    if let (Some(btn_re), Some(attr_re)) = (&btn_re, &attr_re) {
        for cap in btn_re.captures_iter(html) {
            let tag_attrs = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
            let inner_text = cap.get(2).map(|m| m.as_str()).unwrap_or_default();

            let mut src = String::new();
            let mut resolution = String::new();
            let mut audio = String::new();
            let mut av1 = String::from("0");

            for attr_cap in attr_re.captures_iter(tag_attrs) {
                let key = attr_cap
                    .get(1)
                    .map(|m| m.as_str().to_lowercase())
                    .unwrap_or_default();
                let val = attr_cap
                    .get(2)
                    .map(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string();
                match key.as_str() {
                    "src" => src = val,
                    "resolution" => resolution = val,
                    "audio" => audio = val,
                    "av1" => av1 = val,
                    _ => {}
                }
            }

            let mut filesize = None;
            if let Some(ref sz_re) = size_re {
                if let Some(m) = sz_re
                    .captures(inner_text)
                    .or_else(|| sz_re.captures(tag_attrs))
                {
                    if let Some(sz) = m.get(0) {
                        filesize = Some(normalize_size_string(sz.as_str()));
                    }
                }
            }

            if !src.is_empty() {
                let is_kwik = src.contains("kwik");
                sources.push(PlaySource {
                    url: src,
                    resolution,
                    audio,
                    av1,
                    kwik: is_kwik,
                    filesize,
                });
            }
        }
    }

    // 2. Parse download links from #pickDownload or dropdown items
    // e.g. <a href="https://pahe.win/..." class="dropdown-item" target="_blank">Show - 720p (145MB)</a>
    if let (Some(res_re), Some(size_re)) = (&res_re, &size_re) {
        if let Ok(a_re) = Regex::new(r#"(?is)<a\b([^>]*)>(.*?)</a>"#) {
            if let Ok(href_re) = Regex::new(r#"(?i)href\s*=\s*"([^"]*)""#) {
                for cap in a_re.captures_iter(html) {
                    let attrs = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
                    let text = cap.get(2).map(|m| m.as_str()).unwrap_or_default();
                    let text_upper = text.to_ascii_uppercase();

                    let is_download_link = attrs.contains("dropdown-item")
                        || attrs.contains("pahe.win")
                        || text_upper.contains("MB")
                        || text_upper.contains("GB");

                    if !is_download_link {
                        continue;
                    }

                    let href = href_re
                        .captures(attrs)
                        .and_then(|c| c.get(1))
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();

                    let res = res_re
                        .captures(text)
                        .and_then(|c| c.get(1))
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();

                    let size = size_re
                        .captures(text)
                        .and_then(|c| c.get(0))
                        .map(|m| normalize_size_string(m.as_str()));

                    let audio_tag = audio_re.as_ref().and_then(|a_re| {
                        a_re.captures(text).and_then(|c| c.get(1)).map(|m| {
                            let l = m.as_str().to_lowercase();
                            if l == "dub" || l == "eng" {
                                "eng".to_string()
                            } else {
                                "jpn".to_string()
                            }
                        })
                    });

                    if let Some(ref sz) = size {
                        let mut matched = false;
                        for s in sources.iter_mut() {
                            let res_match = res.is_empty() || s.resolution == res;
                            let audio_match = match &audio_tag {
                                Some(a) => s.audio.eq_ignore_ascii_case(a) || s.audio.is_empty(),
                                None => true,
                            };
                            if res_match && audio_match {
                                if s.filesize.is_none() {
                                    s.filesize = Some(sz.clone());
                                }
                                matched = true;
                            }
                        }
                        if !matched && !res.is_empty() && !href.is_empty() {
                            let is_kwik = href.contains("kwik");
                            sources.push(PlaySource {
                                url: href,
                                resolution: res,
                                audio: audio_tag.unwrap_or_else(|| "jpn".to_string()),
                                av1: String::from("0"),
                                kwik: is_kwik,
                                filesize: Some(sz.clone()),
                            });
                        }
                    }
                }
            }
        }
    }

    sources
}

pub async fn play_sources(
    client: &HttpClient,
    slug: &str,
    session: &str,
    cfg: &AppConfig,
) -> Result<Vec<PlaySource>, String> {
    let url = format!(
        "{}/play/{}/{}",
        HOST,
        urlencoding::encode(slug),
        urlencoding::encode(session)
    );
    let html = client.fetch(&url, None, cfg).await?;

    let mut sources = parse_sources_from_html(&html);

    // Fallback: If no filesizes were extracted, query the links API: api?m=links&id={session}&p=kwik
    if sources.iter().all(|s| s.filesize.is_none()) {
        let links_url = format!(
            "{}/api?m=links&id={}&p=kwik",
            HOST,
            urlencoding::encode(session)
        );
        if let Ok(json_str) = client.fetch(&links_url, Some(&url), cfg).await {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if let Some(data) = val.get("data").and_then(|v| v.as_array()) {
                    for item in data {
                        if let Some(obj) = item.as_object() {
                            for (res_key, sub_val) in obj {
                                let res = res_key.trim();
                                let audio = sub_val
                                    .get("audio")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("jpn");
                                let size_str = sub_val.get("filesize").and_then(|v| {
                                    if let Some(n) = v.as_u64() {
                                        Some(format_bytes(n))
                                    } else if let Some(s) = v.as_str() {
                                        if let Ok(n) = s.parse::<u64>() {
                                            Some(format_bytes(n))
                                        } else {
                                            Some(normalize_size_string(s))
                                        }
                                    } else {
                                        None
                                    }
                                });
                                let kwik_url = sub_val
                                    .get("kwik")
                                    .or_else(|| sub_val.get("kwik_pahewin"))
                                    .and_then(|v| v.as_str());

                                let mut matched = false;
                                for s in sources.iter_mut() {
                                    if s.resolution == res
                                        && (s.audio == audio
                                            || s.audio.is_empty()
                                            || audio.is_empty())
                                    {
                                        if s.filesize.is_none() {
                                            s.filesize = size_str.clone();
                                        }
                                        matched = true;
                                    }
                                }
                                if !matched {
                                    if let Some(kurl) = kwik_url {
                                        sources.push(PlaySource {
                                            url: kurl.to_string(),
                                            resolution: res.to_string(),
                                            audio: audio.to_string(),
                                            av1: String::from("0"),
                                            kwik: kurl.contains("kwik"),
                                            filesize: size_str,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(sources)
}

fn build_available_options(
    episode: serde_json::Value,
    session: String,
    sources: &[PlaySource],
) -> AvailableOptions {
    let mut resolutions_set = HashSet::new();
    let mut audios_set = HashSet::new();
    let mut combinations = Vec::new();
    let mut seen_pairs = HashSet::new();
    let mut filesizes = HashMap::new();

    // Prefer non-AV1 sizes first so size display matches the default downloaded stream.
    for prefer_non_av1 in [true, false] {
        for s in sources {
            if prefer_non_av1 && s.av1 == "1" {
                continue;
            }
            if !s.resolution.is_empty() {
                if let Some(ref sz) = s.filesize {
                    filesizes
                        .entry(s.resolution.clone())
                        .or_insert_with(|| sz.clone());
                }
            }
        }
    }

    for s in sources {
        if !s.resolution.is_empty() {
            resolutions_set.insert(s.resolution.clone());
        }
        if !s.audio.is_empty() {
            audios_set.insert(s.audio.clone());
        }
        let pair = (s.resolution.clone(), s.audio.clone());
        if !s.resolution.is_empty() && seen_pairs.insert(pair) {
            let mut combo = serde_json::json!({
                "resolution": s.resolution,
                "audio": s.audio
            });
            if let Some(ref sz) = s.filesize {
                combo["filesize"] = serde_json::Value::String(sz.clone());
            }
            combinations.push(combo);
        }
    }

    let mut resolutions: Vec<String> = resolutions_set.into_iter().collect();
    resolutions.sort_by(|a, b| {
        let an: u32 = a.parse().unwrap_or(0);
        let bn: u32 = b.parse().unwrap_or(0);
        bn.cmp(&an)
    });

    let mut audios: Vec<String> = audios_set.into_iter().collect();
    audios.sort();

    AvailableOptions {
        episode,
        session,
        resolutions,
        audios,
        combinations,
        source_count: sources.len(),
        filesizes,
    }
}

pub async fn episode_options_by_session(
    client: &HttpClient,
    slug: &str,
    ep_session: &str,
    episode_num: &str,
    episode_val: serde_json::Value,
    cfg: &AppConfig,
) -> Result<AvailableOptions, String> {
    if !episode_num.trim().is_empty() {
        if let Some(cached) = load_options_cache(slug, episode_num) {
            if (cached.session == ep_session || ep_session.trim().is_empty())
                && !cached.filesizes.is_empty()
            {
                return Ok(cached);
            }
        }
    }

    let sources = match play_sources(client, slug, ep_session, cfg).await {
        Ok(s) => s,
        Err(err) => {
            if !episode_num.trim().is_empty() {
                if let Some(cached) = load_options_cache(slug, episode_num) {
                    return Ok(cached);
                }
            }
            return Err(err);
        }
    };

    let res = build_available_options(episode_val, ep_session.to_string(), &sources);
    if !episode_num.trim().is_empty() {
        save_options_cache(slug, episode_num, &res);
    }
    Ok(res)
}

pub async fn available_options(
    client: &HttpClient,
    slug: &str,
    episode_num: &str,
    cfg: &AppConfig,
) -> Result<AvailableOptions, String> {
    // 1. Fast path: check disk cache first!
    let (cached_eps, _) = load_release_cache_with_meta(slug);
    let target = if let Some(target_ep) = cached_eps.iter().find(|e| {
        let ep_str = episode_value_str(&e.episode);
        ep_matches(&ep_str, episode_num)
    }) {
        target_ep.clone()
    } else {
        // 2. Not in disk cache: fetch page 1 first (most requested episodes are on page 1)
        let (p1_items, last_page) = release_page(client, slug, 1, cfg).await?;
        if let Some(target_ep) = p1_items.iter().find(|e| {
            let ep_str = episode_value_str(&e.episode);
            ep_matches(&ep_str, episode_num)
        }) {
            if last_page <= 1 && !p1_items.is_empty() {
                save_release_cache(slug, &p1_items);
            }
            target_ep.clone()
        } else {
            // 3. Fallback: fetch full release list
            let list = release(client, slug, cfg).await?;
            list.into_iter()
                .find(|e| {
                    let ep_str = episode_value_str(&e.episode);
                    ep_matches(&ep_str, episode_num)
                })
                .ok_or_else(|| format!("Episode {} not found for this anime", episode_num))?
        }
    };

    episode_options_by_session(
        client,
        slug,
        &target.session,
        episode_num,
        target.episode.clone(),
        cfg,
    )
    .await
}

pub fn catalog_path() -> PathBuf {
    let dir = data_dir();
    let _ = fs::create_dir_all(&dir);
    dir.join("anime.list")
}

fn ensure_catalog_seeded() {
    let path = catalog_path();
    let bundled = resource_dir().join("anime.list");
    if !bundled.is_file() || bundled == path {
        return;
    }
    if !path.exists() {
        if fs::copy(&bundled, &path).is_ok() {
            invalidate_catalog_cache();
        }
        return;
    }

    // If the app/APK was upgraded with a newer bundled anime.list (for example,
    // after site-wide slug rotations), update the seeded data_dir copy as long
    // as the user hasn't refreshed anime.list more recently than the bundle.
    if let (Ok(b_meta), Ok(p_meta)) = (fs::metadata(&bundled), fs::metadata(&path)) {
        if let (Ok(b_mtime), Ok(p_mtime)) = (b_meta.modified(), p_meta.modified()) {
            if b_mtime > p_mtime && fs::copy(&bundled, &path).is_ok() {
                invalidate_catalog_cache();
            }
        }
    }
}

#[derive(Clone)]
struct CatalogCache {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
    entries: Vec<CatalogEntry>,
}

static CATALOG_CACHE: OnceLock<Mutex<Option<CatalogCache>>> = OnceLock::new();

fn catalog_cache() -> &'static Mutex<Option<CatalogCache>> {
    CATALOG_CACHE.get_or_init(|| Mutex::new(None))
}

fn invalidate_catalog_cache() {
    if let Ok(mut guard) = catalog_cache().lock() {
        *guard = None;
    }
}

fn parse_catalog_line(line: &str) -> Option<CatalogEntry> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('[')?;
    let (slug_raw, after_bracket) = rest.split_once(']')?;
    let slug = slug_raw.trim();
    if slug.is_empty() {
        return None;
    }
    let after = after_bracket.trim_start();
    let (title, poster) = match after.split_once('|') {
        Some((t, p)) => {
            let p_trimmed = p.trim();
            let poster = if p_trimmed.is_empty() {
                None
            } else {
                Some(p_trimmed.to_string())
            };
            (t.trim().to_string(), poster)
        }
        None => (after.trim().to_string(), None),
    };
    Some(CatalogEntry {
        slug: slug.to_string(),
        title,
        poster,
    })
}

pub fn catalog() -> Vec<CatalogEntry> {
    ensure_catalog_seeded();
    let path = catalog_path();

    let meta = fs::metadata(&path).ok();
    let modified = meta.as_ref().and_then(|m| m.modified().ok());
    let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);

    if let Ok(guard) = catalog_cache().lock() {
        if let Some(ref cached) = *guard {
            if cached.path == path && cached.modified == modified && cached.len == len {
                return cached.entries.clone();
            }
        }
    }

    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    if let Ok(raw) = fs::read_to_string(&path) {
        // Format: [slug] title | poster_url
        for line in raw.lines() {
            if let Some(entry) = parse_catalog_line(line) {
                if seen.insert(entry.slug.clone()) {
                    entries.push(entry);
                }
            }
        }
    }

    if meta.is_some() {
        if let Ok(mut guard) = catalog_cache().lock() {
            *guard = Some(CatalogCache {
                path,
                modified,
                len,
                entries: entries.clone(),
            });
        }
    }

    entries
}

pub fn title_for_slug(slug: &str) -> String {
    for entry in catalog() {
        if entry.slug == slug {
            return entry.title;
        }
    }
    String::new()
}

fn catalog_line(slug: &str, title: &str, poster: Option<&str>) -> String {
    let title = title.replace(['\r', '\n'], " ");
    let poster_part = poster.map(|p| format!(" | {}", p)).unwrap_or_default();
    format!("[{}] {}{}   \n", slug, title.trim(), poster_part)
}

fn write_catalog_atomic(content: &str) -> Result<(), String> {
    let path = catalog_path();
    let tmp = path.with_extension("list.tmp");
    fs::write(&tmp, content).map_err(|e| format!("Cannot write anime.list: {}", e))?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Cannot replace anime.list: {}", e));
    }
    invalidate_catalog_cache();
    Ok(())
}

pub fn ensure_entry(slug: &str, title: &str) {
    let path = catalog_path();
    let current = catalog();
    if current.iter().any(|e| e.slug == slug) {
        return;
    }
    use std::io::Write;
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(catalog_line(slug, title, None).as_bytes());
        invalidate_catalog_cache();
    }
}

pub fn update_slug_in_catalog(old_slug: &str, new_slug: &str, title: &str) {
    let mut current = catalog();
    let preserved_poster = current
        .iter()
        .find(|e| e.slug == old_slug || e.title.eq_ignore_ascii_case(title))
        .and_then(|e| e.poster.clone());

    // Remove old_slug or existing duplicate entries for the same title
    current.retain(|e| {
        e.slug != old_slug && !(e.title.eq_ignore_ascii_case(title) && e.slug != new_slug)
    });

    if !current.iter().any(|e| e.slug == new_slug) {
        current.push(CatalogEntry {
            slug: new_slug.to_string(),
            title: title.to_string(),
            poster: preserved_poster,
        });
    }

    let mut content = String::with_capacity(current.len() * 80);
    for e in &current {
        content.push_str(&catalog_line(&e.slug, &e.title, e.poster.as_deref()));
    }
    let _ = write_catalog_atomic(&content);
}

pub async fn refresh_catalog(
    client: &HttpClient,
    cfg: &AppConfig,
) -> Result<Vec<CatalogEntry>, String> {
    let url = format!("{}/anime", HOST);
    let html = client.fetch(&url, None, cfg).await?;

    // More flexible regex to match various HTML structures:
    // 1. <a href="/anime/SLUG" title="TITLE">...<img src="POSTER"...>...</a>
    // 2. <a href="/anime/SLUG" title="TITLE"><img src="POSTER"...></a>
    // 3. <img src="POSTER"...>...<a href="/anime/SLUG" title="TITLE">...</a>
    // 4. <a href="/anime/SLUG" ... title="TITLE" ...> (no img inside, check nearby)
    let anchor_re = Regex::new(r#"<a\s+href="/anime/([0-9a-fA-F-]+)"[^>]*title="([^"]*)"[^>]*>"#)
        .map_err(|e| e.to_string())?;

    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    // First pass: collect all anchors with their positions
    let mut anchor_positions = Vec::new();
    for caps in anchor_re.captures_iter(&html) {
        let slug = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        let title = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
        let anchor_end = caps.get(0).map(|m| m.end()).unwrap_or(0);
        if !slug.is_empty() && seen.insert(slug.to_string()) {
            anchor_positions.push((slug.to_string(), title.to_string(), anchor_end));
        }
    }

    // Second pass: find all img tags with their positions
    let img_re = Regex::new(r#"<img\s+[^>]*src="([^"]*(?:animepahe|anime|cover|poster|thumb)[^"]*|\.[^"]*(?:jpg|png|webp))"[^>]*>"#)
        .map_err(|e| e.to_string())?;
    let mut img_positions = Vec::new();
    for caps in img_re.captures_iter(&html) {
        if let Some(src) = caps.get(1) {
            let src_str = src.as_str().trim();
            if !src_str.is_empty() && !src_str.starts_with("data:") {
                img_positions.push((
                    src_str.to_string(),
                    caps.get(0).map(|m| m.start()).unwrap_or(0),
                ));
            }
        }
    }

    // Match each anchor to the nearest preceding or following image
    for (slug, title, anchor_pos) in anchor_positions {
        let mut best_poster = None;
        let mut best_distance = usize::MAX;

        for (img_src, img_pos) in &img_positions {
            let distance = anchor_pos.abs_diff(*img_pos);
            if distance < best_distance && distance < 5000 {
                // Within 5KB of HTML
                best_distance = distance;
                best_poster = Some(img_src.clone());
            }
        }

        entries.push(CatalogEntry {
            slug,
            title,
            poster: best_poster,
        });
    }

    if entries.is_empty() {
        return Err(
            "Could not read anime listing — the cf_clearance cookie may be expired.".to_string(),
        );
    }

    let mut content = String::with_capacity(entries.len() * 120);
    for e in &entries {
        content.push_str(&catalog_line(&e.slug, &e.title, e.poster.as_deref()));
    }
    write_catalog_atomic(&content)?;

    Ok(entries)
}

pub fn is_rotated_or_missing_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    if lower.contains("cloudflare") || lower.contains("cf_clearance") || lower.contains("403") {
        return false;
    }
    lower.contains("404")
        || lower.contains("410")
        || lower.contains("not found")
        || lower.contains("slug may have rotated")
        || lower.contains("returned html instead of json")
        || lower.contains("no data for this request")
        || lower.contains("empty response")
}

fn push_unique_term(terms: &mut Vec<String>, candidate: String) {
    let c = candidate.split_whitespace().collect::<Vec<_>>().join(" ");
    if c.len() >= 2 && !terms.iter().any(|t| t.eq_ignore_ascii_case(&c)) {
        terms.push(c);
    }
}

fn strip_season_and_tags(title: &str) -> String {
    let mut out = title.to_string();
    if let Ok(re_parens) = Regex::new(r#"(?i)\s*[\(\[][^\)\]]*[\)\]]"#) {
        out = re_parens.replace_all(&out, " ").to_string();
    }
    if let Ok(re_season) = Regex::new(
        r#"(?i)\b(?:season\s*\d+|\d+(?:st|nd|rd|th)\s*season|part\s*\d+|cour\s*\d+|specials?|ova|ona)\b"#,
    ) {
        out = re_season.replace_all(&out, " ").to_string();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn generate_search_terms(title: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return terms;
    }
    terms.push(trimmed.to_string());

    // Cleaned: replace non-alphanumeric with spaces, collapse spaces
    let cleaned: String = trimmed
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if !cleaned.is_empty() && cleaned != trimmed {
        terms.push(cleaned.clone());
    }

    // Segments separated by ':', '/', '-', ';'
    for (idx, seg) in trimmed.split([':', '/', '-', ';']).enumerate() {
        let seg_trim = seg.trim();
        let min_len = if idx == 0 { 3 } else { 5 };
        if seg_trim.len() >= min_len && seg_trim != trimmed {
            push_unique_term(&mut terms, seg_trim.to_string());
            let clean_seg: String = seg_trim
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { ' ' })
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if clean_seg.len() >= min_len {
                push_unique_term(&mut terms, clean_seg);
            }
        }
    }

    // Without season/bracket tags (so search returns all seasons to score)
    let stripped = strip_season_and_tags(&cleaned);
    if stripped.len() >= 3 {
        push_unique_term(&mut terms, stripped);
    }

    // First 4 words / first 3 words for long titles
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.len() > 4 {
        push_unique_term(&mut terms, words[..4].join(" "));
    }
    if words.len() > 3 {
        push_unique_term(&mut terms, words[..3].join(" "));
    }

    terms.truncate(6);
    terms
}

fn normalize_for_match(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn extract_season_number(s: &str) -> Option<u32> {
    let lower = s.to_lowercase();
    if let Ok(re) = Regex::new(
        r#"(?i)(?:\bseason\s*(\d+)|\b(\d+)(?:st|nd|rd|th)\s*season|\bs(\d+)\b|\bpart\s*(\d+)|\bcour\s*(\d+))"#,
    ) {
        if let Some(caps) = re.captures(&lower) {
            for i in 1..=5 {
                if let Some(m) = caps.get(i) {
                    if let Ok(n) = m.as_str().parse::<u32>() {
                        return Some(n);
                    }
                }
            }
        }
    }
    if let Some(last_word) = lower.split_whitespace().last() {
        let clean = last_word.trim_matches(|c: char| !c.is_alphanumeric());
        match clean {
            "ii" => return Some(2),
            "iii" => return Some(3),
            "iv" => return Some(4),
            "v" => return Some(5),
            _ => {
                if let Ok(n) = clean.parse::<u32>() {
                    if (2..=20).contains(&n) {
                        return Some(n);
                    }
                }
            }
        }
    }
    None
}

fn score_slug_match(target: &str, candidate: &str) -> Option<i32> {
    let t_trim = target.trim();
    let c_trim = candidate.trim();
    if t_trim.is_empty() || c_trim.is_empty() {
        return None;
    }
    if c_trim.eq_ignore_ascii_case(t_trim) {
        return Some(1000);
    }
    let norm_t = normalize_for_match(t_trim);
    let norm_c = normalize_for_match(c_trim);
    if norm_t.is_empty() || norm_c.is_empty() {
        return None;
    }
    if norm_t == norm_c {
        return Some(900);
    }

    // Do not confuse different numbered seasons/parts (e.g. Overlord vs Overlord II)
    let season_t = extract_season_number(t_trim);
    let season_c = extract_season_number(c_trim);
    if season_t != season_c && (season_t.is_some() || season_c.is_some()) {
        // Allow "Title 2" vs "Title Season 2" (both extract Some(2)), reject Some(1)/None vs Some(2)
        return None;
    }

    if norm_c.starts_with(&norm_t) || norm_t.starts_with(&norm_c) {
        let diff = norm_c.len().abs_diff(norm_t.len()) as i32;
        return Some((750 - diff * 5).max(450));
    }

    // Token overlap score (handles slightly reworded subtitles or Japanese/English hybrids)
    let t_words: HashSet<String> = t_trim
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(String::from)
        .collect();
    let c_words: HashSet<String> = c_trim
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(String::from)
        .collect();
    if t_words.is_empty() || c_words.is_empty() {
        return None;
    }
    let overlap = t_words.intersection(&c_words).count();
    let union = t_words.union(&c_words).count();
    if union > 0 && overlap * 100 / union >= 65 {
        return Some(450 + ((overlap * 200) / union) as i32);
    }

    None
}

pub async fn resolve_active_slug(
    client: &HttpClient,
    slug: &str,
    preferred_title: Option<&str>,
    cfg: &AppConfig,
) -> Option<String> {
    let title = match preferred_title {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => title_for_slug(slug),
    };
    if title.is_empty() {
        return None;
    }

    let search_terms = generate_search_terms(&title);
    let mut best_match: Option<(i32, String, String)> = None;

    for term in search_terms {
        if let Ok(results) = search(client, &term, cfg).await {
            for res in results {
                if res.slug.is_empty() || res.slug == slug {
                    continue;
                }
                if let Some(score) = score_slug_match(&title, &res.title) {
                    let replace = best_match
                        .as_ref()
                        .map(|(best_s, _, _)| score > *best_s)
                        .unwrap_or(true);
                    if replace {
                        best_match = Some((score, res.slug.clone(), res.title.clone()));
                    }
                    if score >= 850 {
                        update_slug_in_catalog(slug, &res.slug, &res.title);
                        return Some(res.slug);
                    }
                }
            }
        }
    }

    if let Some((score, new_slug, new_title)) = best_match {
        if score >= 450 {
            update_slug_in_catalog(slug, &new_slug, &new_title);
            return Some(new_slug);
        }
    }

    None
}

pub fn detect_available_audios(items: &[EpisodeItem]) -> Vec<String> {
    let mut audios = vec!["jpn".to_string()];
    let has_eng = items
        .iter()
        .any(|it| it.audio.eq_ignore_ascii_case("eng") || it.audio.eq_ignore_ascii_case("dub"));

    if has_eng {
        audios.push("eng".to_string());
    }
    audios
}

pub fn filter_episodes_by_audio(items: &[EpisodeItem], audio_filter: &str) -> Vec<EpisodeItem> {
    let norm = audio_filter.trim().to_lowercase();
    if norm.is_empty() || norm == "all" || norm == "default" {
        return items.to_vec();
    }

    match norm.as_str() {
        "dub" | "eng" => items
            .iter()
            .filter(|it| {
                it.audio.eq_ignore_ascii_case("eng") || it.audio.eq_ignore_ascii_case("dub")
            })
            .cloned()
            .collect(),
        "sub" | "jpn" => {
            // Japanese Sub is universally available for all anime episodes
            items.to_vec()
        }
        other => items
            .iter()
            .filter(|it| it.audio.eq_ignore_ascii_case(other))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lines_stay_single_line() {
        let line = catalog_line("slug-1", "Title\nwith\rbreaks", None);
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.starts_with("[slug-1] Title with breaks"));
    }

    #[test]
    fn filter_episodes_by_audio_handles_mixed_releases() {
        let items = vec![
            EpisodeItem {
                episode: serde_json::json!(1),
                audio: "eng".to_string(),
                created_at: String::new(),
                duration: String::new(),
                session: "s1".to_string(),
                episode2: serde_json::json!(0),
            },
            EpisodeItem {
                episode: serde_json::json!(2),
                audio: "eng".to_string(),
                created_at: String::new(),
                duration: String::new(),
                session: "s2".to_string(),
                episode2: serde_json::json!(0),
            },
            EpisodeItem {
                episode: serde_json::json!(3),
                audio: "eng".to_string(),
                created_at: String::new(),
                duration: String::new(),
                session: "s3".to_string(),
                episode2: serde_json::json!(0),
            },
            EpisodeItem {
                episode: serde_json::json!(4),
                audio: "jpn".to_string(),
                created_at: String::new(),
                duration: String::new(),
                session: "s4".to_string(),
                episode2: serde_json::json!(0),
            },
            EpisodeItem {
                episode: serde_json::json!(5),
                audio: "jpn".to_string(),
                created_at: String::new(),
                duration: String::new(),
                session: "s5".to_string(),
                episode2: serde_json::json!(0),
            },
        ];

        let audios = detect_available_audios(&items);
        assert_eq!(audios, vec!["jpn", "eng"]);

        // Dub should only show the 3 dub episodes
        let dub = filter_episodes_by_audio(&items, "dub");
        assert_eq!(dub.len(), 3);
        assert_eq!(dub[0].session, "s1");
        assert_eq!(dub[1].session, "s2");
        assert_eq!(dub[2].session, "s3");

        let eng = filter_episodes_by_audio(&items, "eng");
        assert_eq!(eng.len(), 3);

        // Sub should include all episodes since original show is Japanese
        let sub = filter_episodes_by_audio(&items, "sub");
        assert_eq!(sub.len(), 5);

        // "all" should return all
        let all = filter_episodes_by_audio(&items, "all");
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn filter_episodes_by_audio_sub_only_anime() {
        let items = vec![EpisodeItem {
            episode: serde_json::json!(1),
            audio: "jpn".to_string(),
            created_at: String::new(),
            duration: String::new(),
            session: "s1".to_string(),
            episode2: serde_json::json!(0),
        }];

        assert_eq!(detect_available_audios(&items), vec!["jpn"]);
        assert_eq!(filter_episodes_by_audio(&items, "dub").len(), 0);
        assert_eq!(filter_episodes_by_audio(&items, "sub").len(), 1);
    }

    #[test]
    fn filter_episodes_by_audio_fully_dubbed_anime() {
        // Many popular anime on AnimePahe (Solo Leveling, AOT, Demon Slayer) have all episodes marked "eng"
        let items = vec![
            EpisodeItem {
                episode: serde_json::json!(1),
                audio: "eng".to_string(),
                created_at: String::new(),
                duration: String::new(),
                session: "s1".to_string(),
                episode2: serde_json::json!(0),
            },
            EpisodeItem {
                episode: serde_json::json!(2),
                audio: "eng".to_string(),
                created_at: String::new(),
                duration: String::new(),
                session: "s2".to_string(),
                episode2: serde_json::json!(0),
            },
        ];

        // Both JPN and ENG must be available!
        assert_eq!(detect_available_audios(&items), vec!["jpn", "eng"]);
        // Dub returns both episodes
        assert_eq!(filter_episodes_by_audio(&items, "dub").len(), 2);
        // Sub ALSO returns both episodes (not 0!)
        assert_eq!(filter_episodes_by_audio(&items, "sub").len(), 2);
    }

    #[test]
    fn formats_bytes_and_normalizes_size_strings() {
        assert_eq!(format_bytes(1024 * 1024), "1 MB");
        assert_eq!(format_bytes(152043520), "145 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 2), "2.0 GB");

        assert_eq!(normalize_size_string("720p (145MB)"), "145 MB");
        assert_eq!(normalize_size_string("1080p (285 MB)"), "285 MB");
        assert_eq!(normalize_size_string("(1.2GB)"), "1.2 GB");
        assert_eq!(normalize_size_string("65MB"), "65 MB");
    }

    #[test]
    fn parses_sources_and_filesizes_from_html() {
        let html = r#"
            <div id="resolutionMenu">
                <button type="button" class="btn btn-dark" data-src="https://kwik.cx/e/123" data-resolution="720" data-audio="jpn" data-av1="0">720p</button>
                <button type="button" class="btn btn-dark" data-src="https://kwik.cx/e/456" data-resolution="1080" data-audio="jpn" data-av1="0">1080p</button>
            </div>
            <div class="dropdown-menu" id="pickDownload">
                <a href="https://pahe.win/dl1" class="dropdown-item" target="_blank">Anime Ep 1 - 720p (145MB)</a>
                <a href="https://pahe.win/dl2" class="dropdown-item" target="_blank">Anime Ep 1 - 1080p (285MB)</a>
            </div>
        "#;

        let sources = parse_sources_from_html(html);
        assert_eq!(sources.len(), 2);
        let s720 = sources.iter().find(|s| s.resolution == "720").unwrap();
        assert_eq!(s720.url, "https://kwik.cx/e/123");
        assert_eq!(s720.filesize, Some("145 MB".to_string()));

        let s1080 = sources.iter().find(|s| s.resolution == "1080").unwrap();
        assert_eq!(s1080.url, "https://kwik.cx/e/456");
        assert_eq!(s1080.filesize, Some("285 MB".to_string()));
    }

    #[test]
    fn parses_latest_release_items_with_dub_sub_labeling() {
        let dub_item = serde_json::json!({
            "id": 100,
            "anime_id": 200,
            "anime_title": "Solo Leveling (Dub)",
            "anime_session": "sess1",
            "episode": 1,
            "audio": null
        });
        let item1 = parse_latest_release_item(&dub_item).unwrap();
        assert_eq!(item1.audio, Some("dub".to_string()));
        assert!(item1.has_sub);
        assert!(item1.has_dub);

        let sub_item = serde_json::json!({
            "id": 101,
            "anime_id": 201,
            "anime_title": "One Piece",
            "anime_session": "sess2",
            "episode": 1120,
            "audio": "jpn"
        });
        let item2 = parse_latest_release_item(&sub_item).unwrap();
        assert_eq!(item2.audio, Some("sub".to_string()));
        assert!(item2.has_sub);
        assert!(!item2.has_dub);

        let explicit_dub = serde_json::json!({
            "id": 102,
            "anime_id": 202,
            "anime_title": "Bleach",
            "anime_session": "sess3",
            "episode": 15,
            "audio": "eng"
        });
        let item3 = parse_latest_release_item(&explicit_dub).unwrap();
        assert_eq!(item3.audio, Some("dub".to_string()));
        assert!(item3.has_sub);
        assert!(item3.has_dub);
    }

    #[test]
    fn merges_duplicate_recent_releases_across_sub_and_dub() {
        let sub_only = parse_latest_release_item(&serde_json::json!({
            "anime_title": "Frieren",
            "anime_session": "frieren-1",
            "episode": 5,
            "audio": "jpn",
            "session": "ep5-sub"
        }))
        .unwrap();
        let dub_only = parse_latest_release_item(&serde_json::json!({
            "anime_title": "Frieren",
            "anime_session": "frieren-1",
            "episode": 5,
            "audio": "eng",
            "session": "ep5-dub"
        }))
        .unwrap();

        let merged = merge_latest_release_items(vec![sub_only, dub_only]);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].has_sub);
        assert!(merged[0].has_dub);
        assert_eq!(merged[0].audio, Some("dub".to_string()));
    }

    #[test]
    fn parses_multiline_pick_download_links_and_sizes() {
        let html = r#"
            <div id="resolutionMenu">
                <button type="button" data-src="https://kwik.cx/e/1" data-resolution="1080" data-audio="eng" data-av1="0">1080p</button>
                <button type="button" data-src="https://kwik.cx/e/2" data-resolution="1080" data-audio="jpn" data-av1="0">1080p</button>
            </div>
            <div id="pickDownload">
                <a href="https://pahe.win/a" class="dropdown-item">
                    Show - 1080p (312MB) <span class="badge">eng</span>
                </a>
                <a href="https://pahe.win/b" class="dropdown-item">
                    Show - 1080p (290MB) <span class="badge">jpn</span>
                </a>
            </div>
        "#;
        let sources = parse_sources_from_html(html);
        let opts = build_available_options(serde_json::json!(1), "ep1".to_string(), &sources);
        assert_eq!(opts.audios, vec!["eng", "jpn"]);
        assert_eq!(opts.resolutions, vec!["1080"]);
        assert_eq!(
            opts.filesizes.get("1080").map(String::as_str),
            Some("312 MB")
        );
    }

    #[test]
    fn detects_rotated_or_missing_errors() {
        assert!(is_rotated_or_missing_error(
            "HTTP request failed with status code 404 Not Found"
        ));
        assert!(is_rotated_or_missing_error("404 Not Found"));
        assert!(is_rotated_or_missing_error("slug may have rotated"));
        assert!(is_rotated_or_missing_error("returned HTML instead of JSON"));
        assert!(!is_rotated_or_missing_error("Connection timed out"));
        assert!(!is_rotated_or_missing_error("500 Internal Server Error"));
    }

    #[test]
    fn generates_search_terms_for_punctuated_titles() {
        let terms = generate_search_terms(".hack//Gift");
        assert!(terms.contains(&".hack//Gift".to_string()));
        assert!(terms.contains(&"hack Gift".to_string()));
        assert!(terms.contains(&".hack".to_string()));
        assert!(terms.contains(&"hack".to_string()));

        let terms_simple = generate_search_terms("Naruto");
        assert_eq!(terms_simple, vec!["Naruto".to_string()]);
    }

    #[test]
    fn normalizes_titles_for_matching() {
        assert_eq!(normalize_for_match(".hack//Gift"), "hackgift");
        assert_eq!(normalize_for_match(".hack // Gift"), "hackgift");
        assert_eq!(normalize_for_match("hack gift"), "hackgift");
        assert_eq!(normalize_for_match(".hack//Roots"), "hackroots");
    }

    #[test]
    fn scores_slug_matches_without_confusing_seasons() {
        assert!(score_slug_match("Overlord", "Overlord").unwrap() == 1000);
        assert!(score_slug_match("Overlord", "Overlord II").is_none());
        assert!(score_slug_match("[Oshi No Ko] Season 2", "Oshi no Ko").is_none());
        assert!(score_slug_match("[Oshi No Ko] Season 2", "Oshi no Ko Season 2").unwrap() >= 900);
        assert!(
            score_slug_match(
                "Solo Leveling Season 2: Arise from the Shadow",
                "Solo Leveling Season 2"
            )
            .unwrap()
                >= 450
        );
    }

    #[test]
    fn generates_search_terms_for_rezero_and_long_titles() {
        let rezero = generate_search_terms("Re:ZERO -Starting Life in Another World-");
        assert!(rezero.iter().any(|t| t.contains("Starting Life")));
        let long =
            generate_search_terms("Chillin in Another World with Level 2 Super Cheat Powers");
        assert!(long.iter().any(|t| t == "Chillin in Another World"));
    }
}
