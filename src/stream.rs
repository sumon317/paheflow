use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex, Semaphore};

use crate::api::{self, PlaySource};
use crate::client::{is_cloudflare_error, HttpClient};
use crate::config::AppConfig;
use crate::hls::parse_hls_attributes;
use crate::kwik;
use crate::runner::select_candidate_sources;

const MAX_STREAM_SESSIONS: usize = 24;
const SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const DEFAULT_REFERER: &str = "https://kwik.cx/";
const DEFAULT_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:159.0) Gecko/20100101 Firefox/159.0";
const TOKEN_PLACEHOLDER: &str = "__STREAM_TOKEN__";
const PREFETCH_AHEAD: usize = 8;
const PREFETCH_BEHIND_KEEP: usize = 2;
const PREFETCH_CONCURRENCY: usize = 8;

type SegmentResult = Result<Bytes, StreamProxyError>;
type InFlightReceiver = watch::Receiver<Option<SegmentResult>>;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamResolveRequest {
    pub slug: String,
    #[serde(default)]
    pub title: Option<String>,
    pub episode: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub audio: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StreamVariantInfo {
    pub resolution: String,
    pub audio: String,
    pub av1: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filesize: Option<String>,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedStream {
    pub ok: bool,
    pub stream_id: String,
    pub playlist_url: String,
    pub slug: String,
    pub title: String,
    pub episode: String,
    pub session: String,
    pub resolution: String,
    pub audio: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filesize: Option<String>,
    pub duration_secs: f64,
    pub segment_count: usize,
    pub variants: Vec<StreamVariantInfo>,
}

#[derive(Debug, Clone)]
pub struct StreamResolveError {
    pub message: String,
    pub cf_challenge: bool,
    pub not_found: bool,
}

impl StreamResolveError {
    fn upstream(msg: impl Into<String>) -> Self {
        let message = msg.into();
        let cf_challenge = is_cloudflare_error(&message);
        Self {
            message,
            cf_challenge,
            not_found: false,
        }
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            cf_challenge: false,
            not_found: true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum StreamProxyError {
    NotFound(String),
    Upstream { status: u16, message: String },
}

#[derive(Debug, Clone)]
struct StreamSession {
    created_at: Instant,
    last_accessed: Instant,
    referer: String,
    user_agent: String,
    rewritten_playlist: String,
    keys: Vec<String>,
    key_cache: HashMap<usize, Vec<u8>>,
    maps: Vec<String>,
    map_cache: HashMap<usize, Bytes>,
    segments: Vec<String>,
    segment_cache: HashMap<usize, Bytes>,
    in_flight: HashMap<usize, InFlightReceiver>,
    playhead: usize,
}

impl StreamSession {
    fn new(
        now: Instant,
        referer: String,
        user_agent: String,
        rewritten_playlist: String,
        keys: Vec<String>,
        maps: Vec<String>,
        segments: Vec<String>,
    ) -> Self {
        Self {
            created_at: now,
            last_accessed: now,
            referer,
            user_agent,
            rewritten_playlist,
            keys,
            key_cache: HashMap::new(),
            maps,
            map_cache: HashMap::new(),
            segments,
            segment_cache: HashMap::new(),
            in_flight: HashMap::new(),
            playhead: 0,
        }
    }
}

struct RewrittenPlaylist {
    body: String,
    keys: Vec<String>,
    maps: Vec<String>,
    segments: Vec<String>,
    duration_secs: f64,
}

pub struct StreamManager {
    sessions: Arc<Mutex<HashMap<String, StreamSession>>>,
    http: reqwest::Client,
    prefetch_sem: Arc<Semaphore>,
}

impl Default for StreamManager {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamManager {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .dns_resolver(Arc::new(crate::dns::SmartDnsResolver::new()))
            .http1_only()
            .tcp_nodelay(true)
            .pool_max_idle_per_host(32)
            .timeout(Duration::from_secs(90))
            .read_timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(15))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            http,
            prefetch_sem: Arc::new(Semaphore::new(PREFETCH_CONCURRENCY)),
        }
    }

    /// Resolves an AnimePahe episode into a locally proxied HLS stream session.
    pub async fn resolve_episode(
        &self,
        api_client: &HttpClient,
        cfg: &AppConfig,
        req: StreamResolveRequest,
    ) -> Result<ResolvedStream, StreamResolveError> {
        let raw_slug = req.slug.trim();
        let raw_ep = req.episode.trim();
        if raw_slug.is_empty() || raw_ep.is_empty() {
            return Err(StreamResolveError::not_found(
                "Missing anime slug or episode number",
            ));
        }

        let mut actual_slug = raw_slug.to_string();
        let mut ep_session = req
            .session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);

        if ep_session.is_none() {
            let (cached_eps, _) = api::load_release_cache_with_meta(&actual_slug);
            if let Some(found) = cached_eps
                .iter()
                .find(|e| episode_matches_value(&e.episode, raw_ep))
            {
                if !found.session.trim().is_empty() {
                    ep_session = Some(found.session.clone());
                }
            }
        }

        if ep_session.is_none() {
            let mut rel = api::release(api_client, &actual_slug, cfg).await;
            if let Err(ref e) = rel {
                if api::is_rotated_or_missing_error(e) {
                    if let Some(healed) = api::resolve_active_slug(
                        api_client,
                        &actual_slug,
                        req.title.as_deref(),
                        cfg,
                    )
                    .await
                    {
                        actual_slug = healed;
                        rel = api::release(api_client, &actual_slug, cfg).await;
                    }
                }
            }
            let episodes = rel.map_err(StreamResolveError::upstream)?;
            if let Some(found) = episodes
                .iter()
                .find(|e| episode_matches_value(&e.episode, raw_ep))
            {
                ep_session = Some(found.session.clone());
            } else {
                return Err(StreamResolveError::not_found(format!(
                    "Episode {} was not found for this anime",
                    raw_ep
                )));
            }
        }

        let mut ep_session = ep_session.unwrap_or_default();
        let mut sources_res = api::play_sources(api_client, &actual_slug, &ep_session, cfg).await;
        if let Err(ref e) = sources_res {
            if api::is_rotated_or_missing_error(e) {
                if let Some(healed) =
                    api::resolve_active_slug(api_client, &actual_slug, req.title.as_deref(), cfg)
                        .await
                {
                    actual_slug = healed;
                    if let Ok(episodes) = api::release(api_client, &actual_slug, cfg).await {
                        if let Some(found) = episodes
                            .iter()
                            .find(|ep| episode_matches_value(&ep.episode, raw_ep))
                        {
                            ep_session = found.session.clone();
                        }
                    }
                    sources_res =
                        api::play_sources(api_client, &actual_slug, &ep_session, cfg).await;
                }
            }
        }

        let sources = sources_res.map_err(StreamResolveError::upstream)?;
        if sources.is_empty() {
            return Err(StreamResolveError::not_found(format!(
                "No stream sources found for episode {}",
                raw_ep
            )));
        }

        let requested_res = req.resolution.as_deref().unwrap_or("").trim();
        let requested_audio = req.audio.as_deref().unwrap_or("").trim();
        let candidates = select_candidate_sources(&sources, requested_res, requested_audio);

        let mut resolved_stream: Option<(&PlaySource, String)> = None;
        let mut cf_error: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut cf_blocked_hosts: HashSet<String> = HashSet::new();

        for src in candidates {
            let src_host = url::Url::parse(&src.url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
                .unwrap_or_default();
            if !src_host.is_empty() && cf_blocked_hosts.contains(&src_host) {
                continue;
            }

            let mut html_opt = None;
            for attempt in 1..=2 {
                match api_client
                    .fetch(&src.url, Some("https://animepahe.pw/"), cfg)
                    .await
                {
                    Ok(html) => {
                        html_opt = Some(html);
                        break;
                    }
                    Err(e) => {
                        if is_cloudflare_error(&e) {
                            if cf_error.is_none() {
                                cf_error = Some(e.clone());
                            }
                            if !src_host.is_empty() {
                                cf_blocked_hosts.insert(src_host.clone());
                            }
                            break;
                        }
                        last_error = Some(e);
                        if attempt < 2 {
                            tokio::time::sleep(Duration::from_millis(450)).await;
                        }
                    }
                }
            }

            if let Some(ref html) = html_opt {
                if let Some(m3u8) = kwik::extract_m3u8_from_kwik(html) {
                    resolved_stream = Some((src, m3u8));
                    break;
                }
                last_error = Some("Could not extract m3u8 playlist from Kwik embed".to_string());
            }
        }

        let (chosen_source, m3u8_url) = match resolved_stream {
            Some(pair) => pair,
            None => {
                if let Some(err) = cf_error {
                    return Err(StreamResolveError {
                        message: err,
                        cf_challenge: true,
                        not_found: false,
                    });
                }
                return Err(StreamResolveError::upstream(last_error.unwrap_or_else(
                    || "Could not resolve a playable HLS stream for this episode".to_string(),
                )));
            }
        };

        let ua = effective_ua(cfg);
        let (stream_id, duration_secs, segment_count) = self
            .register_from_m3u8(&m3u8_url, DEFAULT_REFERER, &ua)
            .await
            .map_err(StreamResolveError::upstream)?;

        let variants = build_variant_list(&sources, chosen_source);
        let title = req
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| api::title_for_slug(&actual_slug));

        let playlist_url = format!("/api/stream/hls/{}/playlist.m3u8", stream_id);

        Ok(ResolvedStream {
            ok: true,
            stream_id,
            playlist_url,
            slug: actual_slug,
            title,
            episode: raw_ep.to_string(),
            session: ep_session,
            resolution: chosen_source.resolution.clone(),
            audio: chosen_source.audio.clone(),
            filesize: chosen_source.filesize.clone(),
            duration_secs,
            segment_count,
            variants,
        })
    }

    /// Fetches the upstream m3u8 playlist (resolving master playlists if needed),
    /// rewrites all URIs to local session-indexed proxy paths, and stores the session.
    pub async fn register_from_m3u8(
        &self,
        m3u8_url: &str,
        referer: &str,
        user_agent: &str,
    ) -> Result<(String, f64, usize), String> {
        let (media_body, media_base_url) = self
            .fetch_media_playlist_body(m3u8_url, referer, user_agent, 0)
            .await?;

        let rewritten = rewrite_media_playlist(&media_body, &media_base_url)?;
        let duration_secs = rewritten.duration_secs;
        let segment_count = rewritten.segments.len();

        let id = generate_stream_id();
        let now = Instant::now();
        let session = StreamSession::new(
            now,
            referer.to_string(),
            user_agent.to_string(),
            rewritten.body,
            rewritten.keys,
            rewritten.maps,
            rewritten.segments,
        );

        {
            let mut guard = self.sessions.lock().await;
            prune_sessions(&mut guard, now);
            guard.insert(id.clone(), session);
        }

        // Immediately begin parallel lookahead prefetching from segment 0 so the initial
        // buffer is already downloading across multiple TCP connections before the player starts.
        self.trigger_prefetch_window(&id, 0, true).await;

        Ok((id, duration_secs, segment_count))
    }

    async fn fetch_media_playlist_body(
        &self,
        m3u8_url: &str,
        referer: &str,
        user_agent: &str,
        depth: u8,
    ) -> Result<(String, url::Url), String> {
        if depth > 3 {
            return Err("Too many nested HLS master playlists".to_string());
        }

        let mut last_err = String::new();
        let mut body = String::new();

        for attempt in 1..=4 {
            let mut req = self
                .http
                .get(m3u8_url)
                .header("User-Agent", user_agent)
                .header("Accept", "*/*")
                .header("Accept-Language", "en-US,en;q=0.9");
            if !referer.is_empty() {
                req = req.header("Referer", referer);
            }

            let mut retry_delay = Duration::from_millis(400 * attempt as u64);
            match req.send().await {
                Ok(res) => {
                    let status = res.status();
                    if !status.is_success() {
                        last_err = format!("m3u8 request failed with HTTP {}", status);
                        if status == reqwest::StatusCode::NOT_FOUND
                            || status == reqwest::StatusCode::GONE
                        {
                            return Err(last_err);
                        }
                        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                            let ra = res
                                .headers()
                                .get(reqwest::header::RETRY_AFTER)
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.parse::<u64>().ok())
                                .unwrap_or(2);
                            retry_delay = Duration::from_secs(ra.clamp(1, 8));
                        } else if status == reqwest::StatusCode::FORBIDDEN && attempt >= 2 {
                            return Err(last_err);
                        }
                    } else {
                        match res.text().await {
                            Ok(text) => {
                                let lower = text.to_ascii_lowercase();
                                if !text.contains("#EXTM3U")
                                    && (lower.contains("<html") || lower.contains("<!doctype"))
                                {
                                    last_err = "m3u8 request returned HTML instead of a playlist"
                                        .to_string();
                                } else {
                                    body = text;
                                    break;
                                }
                            }
                            Err(e) => {
                                last_err = format!("Failed to read m3u8 body: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    last_err = format!("Failed to fetch m3u8: {}", e);
                }
            }

            if attempt < 4 {
                tokio::time::sleep(retry_delay).await;
            }
        }

        if body.is_empty() {
            return Err(if last_err.is_empty() {
                "Empty m3u8 response".to_string()
            } else {
                last_err
            });
        }

        let base_url = url::Url::parse(m3u8_url).map_err(|_| "Invalid m3u8 URL".to_string())?;

        if body.contains("#EXT-X-STREAM-INF") {
            if let Some(best_url) = pick_best_master_variant(&body, &base_url) {
                return Box::pin(self.fetch_media_playlist_body(
                    &best_url,
                    referer,
                    user_agent,
                    depth + 1,
                ))
                .await;
            }
        }

        Ok((body, base_url))
    }

    pub async fn render_playlist(&self, id: &str, token: &str) -> Option<String> {
        let mut guard = self.sessions.lock().await;
        let now = Instant::now();
        let session = guard.get_mut(id)?;
        if now.duration_since(session.created_at) > SESSION_TTL {
            guard.remove(id);
            return None;
        }
        session.last_accessed = now;
        let encoded_token = urlencoding::encode(token);
        Some(
            session
                .rewritten_playlist
                .replace(TOKEN_PLACEHOLDER, encoded_token.as_ref()),
        )
    }

    pub async fn fetch_key(&self, id: &str, idx: usize) -> Result<Vec<u8>, StreamProxyError> {
        let (key_url, referer, user_agent) = {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .get_mut(id)
                .ok_or_else(|| StreamProxyError::NotFound("Stream session expired".to_string()))?;
            session.last_accessed = Instant::now();
            if let Some(cached) = session.key_cache.get(&idx) {
                return Ok(cached.clone());
            }
            let url = session
                .keys
                .get(idx)
                .cloned()
                .ok_or_else(|| StreamProxyError::NotFound("Key index not found".to_string()))?;
            (url, session.referer.clone(), session.user_agent.clone())
        };

        let mut last_status = 502u16;
        let mut last_err = String::from("Failed to fetch AES key");

        for attempt in 1..=4 {
            let mut req = self
                .http
                .get(&key_url)
                .header("User-Agent", &user_agent)
                .header("Accept", "*/*");
            if !referer.is_empty() {
                req = req.header("Referer", &referer);
            }

            match req.send().await {
                Ok(res) => {
                    let status = res.status();
                    if status.is_success() {
                        match res.bytes().await {
                            Ok(bytes) if bytes.len() == 16 => {
                                let vec = bytes.to_vec();
                                let mut guard = self.sessions.lock().await;
                                if let Some(session) = guard.get_mut(id) {
                                    session.key_cache.insert(idx, vec.clone());
                                }
                                return Ok(vec);
                            }
                            Ok(bytes) => {
                                last_status = 502;
                                last_err = format!(
                                    "Invalid AES-128 key length (expected 16, got {})",
                                    bytes.len()
                                );
                            }
                            Err(e) => {
                                last_status = 502;
                                last_err = format!("Failed reading key bytes: {}", e);
                            }
                        }
                    } else {
                        last_status = status.as_u16();
                        last_err = format!("Upstream key HTTP {}", status);
                        if status == reqwest::StatusCode::NOT_FOUND
                            || status == reqwest::StatusCode::GONE
                        {
                            break;
                        }
                    }
                }
                Err(e) => {
                    last_status = 502;
                    last_err = format!("Key connection error: {}", e);
                }
            }

            if attempt < 4 {
                tokio::time::sleep(Duration::from_millis(350 * attempt as u64)).await;
            }
        }

        Err(StreamProxyError::Upstream {
            status: last_status,
            message: last_err,
        })
    }

    pub async fn fetch_segment(&self, id: &str, idx: usize) -> Result<Bytes, StreamProxyError> {
        enum SegmentAction {
            Cached(Bytes),
            WaitInFlight {
                rx: InFlightReceiver,
                url: String,
                referer: String,
                user_agent: String,
            },
            DownloadNow {
                url: String,
                referer: String,
                user_agent: String,
                tx: watch::Sender<Option<SegmentResult>>,
            },
        }

        let action = {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .get_mut(id)
                .ok_or_else(|| StreamProxyError::NotFound("Stream session expired".to_string()))?;
            session.last_accessed = Instant::now();
            session.playhead = idx;

            let url =
                session.segments.get(idx).cloned().ok_or_else(|| {
                    StreamProxyError::NotFound("Segment index not found".to_string())
                })?;
            let referer = session.referer.clone();
            let user_agent = session.user_agent.clone();

            if let Some(cached) = session.segment_cache.get(&idx).cloned() {
                SegmentAction::Cached(cached)
            } else if let Some(rx) = session.in_flight.get(&idx).cloned() {
                SegmentAction::WaitInFlight {
                    rx,
                    url,
                    referer,
                    user_agent,
                }
            } else {
                let (tx, rx) = watch::channel(None);
                session.in_flight.insert(idx, rx);
                SegmentAction::DownloadNow {
                    url,
                    referer,
                    user_agent,
                    tx,
                }
            }
        };

        // Slide lookahead window forward and spawn parallel prefetch tasks for upcoming segments
        self.trigger_prefetch_window(id, idx, false).await;

        match action {
            SegmentAction::Cached(bytes) => Ok(bytes),
            SegmentAction::WaitInFlight {
                mut rx,
                url,
                referer,
                user_agent,
            } => {
                if rx.wait_for(|v| v.is_some()).await.is_ok() {
                    if let Some(res) = rx.borrow().as_ref().cloned() {
                        return res;
                    }
                }
                // Fallback if the in-flight prefetch task was dropped before completing
                let res = fetch_upstream_bytes(&self.http, &url, &referer, &user_agent).await;
                if let Ok(ref bytes) = res {
                    let mut guard = self.sessions.lock().await;
                    if let Some(session) = guard.get_mut(id) {
                        session.segment_cache.insert(idx, bytes.clone());
                    }
                }
                res
            }
            SegmentAction::DownloadNow {
                url,
                referer,
                user_agent,
                tx,
            } => {
                let res = fetch_upstream_bytes(&self.http, &url, &referer, &user_agent).await;
                {
                    let mut guard = self.sessions.lock().await;
                    if let Some(session) = guard.get_mut(id) {
                        session.in_flight.remove(&idx);
                        if let Ok(ref bytes) = res {
                            session.segment_cache.insert(idx, bytes.clone());
                        }
                    }
                }
                let _ = tx.send(Some(res.clone()));
                res
            }
        }
    }

    async fn trigger_prefetch_window(&self, id: &str, playhead: usize, include_playhead: bool) {
        let mut to_spawn = Vec::new();
        let (referer, user_agent) = {
            let mut guard = self.sessions.lock().await;
            let Some(session) = guard.get_mut(id) else {
                return;
            };
            session.playhead = playhead;
            let min_keep = playhead.saturating_sub(PREFETCH_BEHIND_KEEP);
            let max_keep = playhead + PREFETCH_AHEAD + 4;
            session
                .segment_cache
                .retain(|&k, _| k >= min_keep && k <= max_keep);

            let total = session.segments.len();
            let first = if include_playhead {
                playhead
            } else {
                playhead.saturating_add(1)
            };
            let last = (playhead + PREFETCH_AHEAD).min(total.saturating_sub(1));
            if first <= last && first < total {
                for p_idx in first..=last {
                    if session.segment_cache.contains_key(&p_idx)
                        || session.in_flight.contains_key(&p_idx)
                    {
                        continue;
                    }
                    if let Some(url) = session.segments.get(p_idx).cloned() {
                        let (tx, rx) = watch::channel(None);
                        session.in_flight.insert(p_idx, rx);
                        to_spawn.push((p_idx, url, tx));
                    }
                }
            }
            (session.referer.clone(), session.user_agent.clone())
        };

        for (p_idx, seg_url, tx) in to_spawn {
            let sessions = self.sessions.clone();
            let http = self.http.clone();
            let sem = self.prefetch_sem.clone();
            let session_id = id.to_string();
            let referer = referer.clone();
            let user_agent = user_agent.clone();

            tokio::spawn(async move {
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        let mut guard = sessions.lock().await;
                        if let Some(s) = guard.get_mut(&session_id) {
                            s.in_flight.remove(&p_idx);
                        }
                        return;
                    }
                };

                {
                    let mut guard = sessions.lock().await;
                    let Some(s) = guard.get_mut(&session_id) else {
                        return;
                    };
                    let in_window = p_idx >= s.playhead.saturating_sub(1)
                        && p_idx <= s.playhead + PREFETCH_AHEAD + 2;
                    if !in_window && tx.receiver_count() <= 1 {
                        s.in_flight.remove(&p_idx);
                        return;
                    }
                }

                let res = fetch_upstream_bytes(&http, &seg_url, &referer, &user_agent).await;

                {
                    let mut guard = sessions.lock().await;
                    if let Some(s) = guard.get_mut(&session_id) {
                        s.in_flight.remove(&p_idx);
                        if let Ok(ref bytes) = res {
                            let in_window = p_idx
                                >= s.playhead.saturating_sub(PREFETCH_BEHIND_KEEP)
                                && p_idx <= s.playhead + PREFETCH_AHEAD + 4;
                            if in_window {
                                s.segment_cache.insert(p_idx, bytes.clone());
                            }
                        }
                    }
                }

                let _ = tx.send(Some(res));
            });
        }
    }

    pub async fn fetch_map(&self, id: &str, idx: usize) -> Result<Bytes, StreamProxyError> {
        let (map_url, referer, user_agent) = {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .get_mut(id)
                .ok_or_else(|| StreamProxyError::NotFound("Stream session expired".to_string()))?;
            session.last_accessed = Instant::now();
            if let Some(cached) = session.map_cache.get(&idx).cloned() {
                return Ok(cached);
            }
            let url = session
                .maps
                .get(idx)
                .cloned()
                .ok_or_else(|| StreamProxyError::NotFound("Map index not found".to_string()))?;
            (url, session.referer.clone(), session.user_agent.clone())
        };

        let bytes = fetch_upstream_bytes(&self.http, &map_url, &referer, &user_agent).await?;
        {
            let mut guard = self.sessions.lock().await;
            if let Some(session) = guard.get_mut(id) {
                session.map_cache.insert(idx, bytes.clone());
            }
        }
        Ok(bytes)
    }

    pub async fn close_session(&self, id: &str) -> bool {
        let mut guard = self.sessions.lock().await;
        guard.remove(id).is_some()
    }
}

async fn fetch_upstream_bytes(
    http: &reqwest::Client,
    upstream_url: &str,
    referer: &str,
    user_agent: &str,
) -> Result<Bytes, StreamProxyError> {
    let mut last_status = 502u16;
    let mut last_err = String::from("Failed to fetch stream segment");

    for attempt in 1..=4 {
        let mut req = http
            .get(upstream_url)
            .header("User-Agent", user_agent)
            .header("Accept", "*/*");
        if !referer.is_empty() {
            req = req.header("Referer", referer);
        }

        let mut retry_delay = Duration::from_millis(350 * attempt as u64);
        match req.send().await {
            Ok(res) => {
                let status = res.status();
                if status.is_success() {
                    match res.bytes().await {
                        Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
                        Ok(_) => {
                            last_status = 502;
                            last_err = String::from("Upstream segment returned empty body");
                        }
                        Err(e) => {
                            last_status = 502;
                            last_err = format!("Upstream segment body read error: {}", e);
                        }
                    }
                } else {
                    last_status = status.as_u16();
                    last_err = format!("Upstream segment HTTP {}", status);
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let ra = res
                            .headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(2);
                        retry_delay = Duration::from_secs(ra.clamp(1, 6));
                    } else if status == reqwest::StatusCode::NOT_FOUND
                        || status == reqwest::StatusCode::GONE
                        || (status == reqwest::StatusCode::FORBIDDEN && attempt >= 2)
                    {
                        break;
                    }
                }
            }
            Err(e) => {
                last_status = 502;
                last_err = format!("Upstream segment error: {}", e);
            }
        }

        if attempt < 4 {
            tokio::time::sleep(retry_delay).await;
        }
    }

    Err(StreamProxyError::Upstream {
        status: last_status,
        message: last_err,
    })
}

fn episode_matches_value(val: &serde_json::Value, target: &str) -> bool {
    let ep_str = match val {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        _ => return false,
    };
    api::ep_matches(&ep_str, target)
}

fn effective_ua(cfg: &AppConfig) -> String {
    let trimmed = cfg.ua.trim();
    if trimmed.is_empty() {
        DEFAULT_UA.to_string()
    } else {
        trimmed.to_string()
    }
}

fn generate_stream_id() -> String {
    let mut bytes = [0u8; 12];
    for b in &mut bytes {
        *b = fastrand::u8(..);
    }
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn prune_sessions(map: &mut HashMap<String, StreamSession>, now: Instant) {
    map.retain(|_, s| now.duration_since(s.created_at) <= SESSION_TTL);
    while map.len() >= MAX_STREAM_SESSIONS {
        let oldest_key = map
            .iter()
            .min_by_key(|(_, s)| s.last_accessed)
            .map(|(k, _)| k.clone());
        match oldest_key {
            Some(k) => {
                map.remove(&k);
            }
            None => break,
        }
    }
}

fn build_variant_list(sources: &[PlaySource], chosen: &PlaySource) -> Vec<StreamVariantInfo> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    // Prefer non-AV1 entries first when deduplicating (resolution, audio) pairs,
    // while still keeping AV1 if it is the only source for that (resolution, audio).
    let mut ordered: Vec<&PlaySource> = sources.iter().collect();
    ordered.sort_by(|a, b| {
        let ar: u32 = a.resolution.parse().unwrap_or(0);
        let br: u32 = b.resolution.parse().unwrap_or(0);
        if ar != br {
            return br.cmp(&ar);
        }
        let a_audio_rank = audio_sort_rank(&a.audio);
        let b_audio_rank = audio_sort_rank(&b.audio);
        if a_audio_rank != b_audio_rank {
            return a_audio_rank.cmp(&b_audio_rank);
        }
        let a_av1 = a.av1 == "1";
        let b_av1 = b.av1 == "1";
        a_av1.cmp(&b_av1)
    });

    for s in ordered {
        if s.resolution.is_empty() {
            continue;
        }
        let key = (s.resolution.clone(), s.audio.to_ascii_lowercase());
        if !seen.insert(key) {
            continue;
        }
        let selected =
            s.resolution == chosen.resolution && s.audio.eq_ignore_ascii_case(&chosen.audio);
        out.push(StreamVariantInfo {
            resolution: s.resolution.clone(),
            audio: s.audio.clone(),
            av1: s.av1 == "1",
            filesize: s.filesize.clone(),
            selected,
        });
    }

    out
}

fn audio_sort_rank(audio: &str) -> u8 {
    match audio.to_ascii_lowercase().as_str() {
        "jpn" | "sub" => 0,
        "eng" | "dub" => 1,
        _ => 2,
    }
}

fn pick_best_master_variant(body: &str, base_url: &url::Url) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    let mut best: Option<(u64, u64, String)> = None;
    for pair in lines.windows(2) {
        let line = pair[0].trim();
        if !line.starts_with("#EXT-X-STREAM-INF") {
            continue;
        }
        let next_line = pair[1].trim();
        if next_line.is_empty() || next_line.starts_with('#') {
            continue;
        }
        let attrs = parse_hls_attributes(line.strip_prefix("#EXT-X-STREAM-INF:").unwrap_or(""));
        let bandwidth = attrs
            .iter()
            .find(|(k, _)| k == "BANDWIDTH")
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .unwrap_or(0);
        let height = attrs
            .iter()
            .find(|(k, _)| k == "RESOLUTION")
            .and_then(|(_, v)| v.split_once('x'))
            .and_then(|(_, h)| h.parse::<u64>().ok())
            .unwrap_or(0);
        if let Ok(resolved) = base_url.join(next_line) {
            let score = (bandwidth, height);
            if best
                .as_ref()
                .map(|(b, h, _)| score > (*b, *h))
                .unwrap_or(true)
            {
                best = Some((bandwidth, height, resolved.to_string()));
            }
        }
    }
    best.map(|(_, _, url)| url)
}

fn rewrite_uri_attribute(line: &str, replacement_uri: &str) -> String {
    static URI_ATTR_RE: OnceLock<Regex> = OnceLock::new();
    let re = URI_ATTR_RE.get_or_init(|| Regex::new(r#"URI="([^"]*)""#).expect("valid URI regex"));
    re.replace(line, format!(r#"URI="{}""#, replacement_uri))
        .into_owned()
}

fn rewrite_media_playlist(body: &str, base_url: &url::Url) -> Result<RewrittenPlaylist, String> {
    let mut out_lines: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut key_indices: HashMap<String, usize> = HashMap::new();
    let mut maps: Vec<String> = Vec::new();
    let mut map_indices: HashMap<String, usize> = HashMap::new();
    let mut segments: Vec<String> = Vec::new();
    let mut duration_secs: f64 = 0.0;

    for raw_line in body.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("#EXTINF:") {
            if let Some(num_str) = rest.split(',').next() {
                if let Ok(d) = num_str.trim().parse::<f64>() {
                    if d.is_finite() && d > 0.0 {
                        duration_secs += d;
                    }
                }
            }
            out_lines.push(trimmed.to_string());
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("#EXT-X-KEY:") {
            let attrs = parse_hls_attributes(rest);
            let uri_opt = attrs
                .iter()
                .find(|(k, _)| k == "URI")
                .map(|(_, v)| v.clone());
            if let Some(raw_uri) = uri_opt {
                let resolved = base_url
                    .join(&raw_uri)
                    .map(|u| u.to_string())
                    .unwrap_or(raw_uri);
                let idx = if let Some(&existing) = key_indices.get(&resolved) {
                    existing
                } else {
                    let next = keys.len();
                    keys.push(resolved.clone());
                    key_indices.insert(resolved, next);
                    next
                };
                let replacement = format!("key/{}?token={}", idx, TOKEN_PLACEHOLDER);
                out_lines.push(rewrite_uri_attribute(trimmed, &replacement));
            } else {
                out_lines.push(trimmed.to_string());
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("#EXT-X-MAP:") {
            let attrs = parse_hls_attributes(rest);
            let uri_opt = attrs
                .iter()
                .find(|(k, _)| k == "URI")
                .map(|(_, v)| v.clone());
            if let Some(raw_uri) = uri_opt {
                let resolved = base_url
                    .join(&raw_uri)
                    .map(|u| u.to_string())
                    .unwrap_or(raw_uri);
                let idx = if let Some(&existing) = map_indices.get(&resolved) {
                    existing
                } else {
                    let next = maps.len();
                    maps.push(resolved.clone());
                    map_indices.insert(resolved, next);
                    next
                };
                let replacement = format!("map/{}.mp4?token={}", idx, TOKEN_PLACEHOLDER);
                out_lines.push(rewrite_uri_attribute(trimmed, &replacement));
            } else {
                out_lines.push(trimmed.to_string());
            }
            continue;
        }

        if trimmed.starts_with('#') {
            out_lines.push(trimmed.to_string());
            continue;
        }

        // Segment URI line
        let resolved = base_url
            .join(trimmed)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| trimmed.to_string());
        let idx = segments.len();
        segments.push(resolved);
        out_lines.push(format!("seg/{}.ts?token={}", idx, TOKEN_PLACEHOLDER));
    }

    if segments.is_empty() {
        return Err("No media segments found in HLS playlist".to_string());
    }

    out_lines.push(String::new());

    Ok(RewrittenPlaylist {
        body: out_lines.join("\n"),
        keys,
        maps,
        segments,
        duration_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_aes128_media_playlist_with_disguised_jpg_segments() {
        let playlist = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-KEY:METHOD=AES-128,URI=\"https://vault-01.owocdn.top/key/mon.key\",IV=0x00000000000000000000000000000001
#EXTINF:4.170833,
https://vault-01.owocdn.top/stream/seg-0.jpg
#EXTINF:5.000000,
seg-1.jpg
#EXT-X-KEY:METHOD=AES-128,URI=\"https://vault-01.owocdn.top/key/mon.key\",IV=0x00000000000000000000000000000002
#EXTINF:3.500000,
/stream/seg-2.jpg
#EXT-X-ENDLIST
";
        let base = url::Url::parse("https://vault-01.owocdn.top/stream/uwu.m3u8").unwrap();
        let rewritten = rewrite_media_playlist(playlist, &base).unwrap();

        assert_eq!(rewritten.keys.len(), 1);
        assert_eq!(rewritten.keys[0], "https://vault-01.owocdn.top/key/mon.key");
        assert_eq!(rewritten.segments.len(), 3);
        assert_eq!(
            rewritten.segments[0],
            "https://vault-01.owocdn.top/stream/seg-0.jpg"
        );
        assert_eq!(
            rewritten.segments[1],
            "https://vault-01.owocdn.top/stream/seg-1.jpg"
        );
        assert_eq!(
            rewritten.segments[2],
            "https://vault-01.owocdn.top/stream/seg-2.jpg"
        );
        assert!((rewritten.duration_secs - 12.670833).abs() < 0.001);
        assert!(rewritten.body.contains(
            "URI=\"key/0?token=__STREAM_TOKEN__\",IV=0x00000000000000000000000000000001"
        ));
        assert!(rewritten.body.contains("seg/0.ts?token=__STREAM_TOKEN__"));
        assert!(rewritten.body.contains("seg/1.ts?token=__STREAM_TOKEN__"));
        assert!(rewritten.body.contains("seg/2.ts?token=__STREAM_TOKEN__"));
        assert!(!rewritten.body.contains("owocdn.top"));
    }

    #[test]
    fn rewrites_fmp4_map_tag() {
        let playlist = "\
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MAP:URI=\"init.mp4\"
#EXTINF:6.0,
chunk0.m4s
#EXT-X-ENDLIST
";
        let base = url::Url::parse("https://cdn.example.com/hls/playlist.m3u8").unwrap();
        let rewritten = rewrite_media_playlist(playlist, &base).unwrap();

        assert_eq!(rewritten.maps, vec!["https://cdn.example.com/hls/init.mp4"]);
        assert!(rewritten
            .body
            .contains("#EXT-X-MAP:URI=\"map/0.mp4?token=__STREAM_TOKEN__\""));
    }

    #[test]
    fn picks_highest_bandwidth_master_variant() {
        let master = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
low/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3500000,RESOLUTION=1920x1080
high/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1280x720
mid/index.m3u8
";
        let base = url::Url::parse("https://cdn.example.com/master.m3u8").unwrap();
        let best = pick_best_master_variant(master, &base).unwrap();
        assert_eq!(best, "https://cdn.example.com/high/index.m3u8");
    }

    #[test]
    fn build_variant_list_deduplicates_and_marks_selected() {
        let sources = vec![
            PlaySource {
                url: "https://kwik.cx/e/1".to_string(),
                resolution: "1080".to_string(),
                audio: "jpn".to_string(),
                av1: "1".to_string(),
                kwik: true,
                filesize: Some("120 MB".to_string()),
            },
            PlaySource {
                url: "https://kwik.cx/e/2".to_string(),
                resolution: "1080".to_string(),
                audio: "jpn".to_string(),
                av1: "0".to_string(),
                kwik: true,
                filesize: Some("190 MB".to_string()),
            },
            PlaySource {
                url: "https://kwik.cx/e/3".to_string(),
                resolution: "720".to_string(),
                audio: "eng".to_string(),
                av1: "0".to_string(),
                kwik: true,
                filesize: Some("110 MB".to_string()),
            },
        ];

        let variants = build_variant_list(&sources, &sources[1]);
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].resolution, "1080");
        assert_eq!(variants[0].audio, "jpn");
        assert!(!variants[0].av1);
        assert!(variants[0].selected);
        assert_eq!(variants[1].resolution, "720");
        assert_eq!(variants[1].audio, "eng");
        assert!(!variants[1].selected);
    }

    #[tokio::test]
    async fn renders_playlist_with_encoded_token_and_closes_session() {
        let manager = StreamManager::new();
        let base = url::Url::parse("https://cdn.example.com/p/uwu.m3u8").unwrap();
        let rewritten =
            rewrite_media_playlist("#EXTM3U\n#EXTINF:4.0,\nseg0.jpg\n#EXT-X-ENDLIST\n", &base)
                .unwrap();

        let id = "test_session".to_string();
        let now = Instant::now();
        {
            let mut guard = manager.sessions.lock().await;
            guard.insert(
                id.clone(),
                StreamSession::new(
                    now,
                    DEFAULT_REFERER.to_string(),
                    DEFAULT_UA.to_string(),
                    rewritten.body,
                    rewritten.keys,
                    rewritten.maps,
                    rewritten.segments,
                ),
            );
        }

        let rendered = manager.render_playlist(&id, "tok+1/2").await.unwrap();
        assert!(rendered.contains("seg/0.ts?token=tok%2B1%2F2"));
        assert!(!rendered.contains(TOKEN_PLACEHOLDER));

        assert!(manager.close_session(&id).await);
        assert!(manager.render_playlist(&id, "tok+1/2").await.is_none());
    }

    #[tokio::test]
    async fn prefetches_ahead_and_deduplicates_segment_fetches() {
        use axum::{routing::get, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let seg0_hits = Arc::new(AtomicUsize::new(0));
        let seg1_hits = Arc::new(AtomicUsize::new(0));
        let seg2_hits = Arc::new(AtomicUsize::new(0));

        let h0 = seg0_hits.clone();
        let h1 = seg1_hits.clone();
        let h2 = seg2_hits.clone();

        let app = Router::new()
            .route(
                "/uwu.m3u8",
                get(|| async {
                    "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4.0,\nseg0.ts\n#EXTINF:4.0,\nseg1.ts\n#EXTINF:4.0,\nseg2.ts\n#EXT-X-ENDLIST\n"
                }),
            )
            .route(
                "/seg0.ts",
                get(move || async move {
                    h0.fetch_add(1, Ordering::Relaxed);
                    vec![0x47u8; 188]
                }),
            )
            .route(
                "/seg1.ts",
                get(move || async move {
                    h1.fetch_add(1, Ordering::Relaxed);
                    vec![0x48u8; 188]
                }),
            )
            .route(
                "/seg2.ts",
                get(move || async move {
                    h2.fetch_add(1, Ordering::Relaxed);
                    vec![0x49u8; 188]
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let manager = StreamManager::new();
        let m3u8_url = format!("http://127.0.0.1:{}/uwu.m3u8", port);
        let (stream_id, _dur, count) = manager
            .register_from_m3u8(&m3u8_url, DEFAULT_REFERER, DEFAULT_UA)
            .await
            .unwrap();
        assert_eq!(count, 3);

        // Fetch segment 0; segments 1 and 2 should also be prefetched in parallel
        let b0 = manager.fetch_segment(&stream_id, 0).await.unwrap();
        assert_eq!(b0.as_ref(), &[0x47u8; 188]);

        // Wait briefly for background prefetch tasks to complete
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(seg1_hits.load(Ordering::Relaxed), 1);
        assert_eq!(seg2_hits.load(Ordering::Relaxed), 1);

        // Fetching segments 1 and 2 now hits the in-memory lookahead cache without re-fetching
        let b1 = manager.fetch_segment(&stream_id, 1).await.unwrap();
        let b2 = manager.fetch_segment(&stream_id, 2).await.unwrap();
        assert_eq!(b1.as_ref(), &[0x48u8; 188]);
        assert_eq!(b2.as_ref(), &[0x49u8; 188]);
        assert_eq!(seg0_hits.load(Ordering::Relaxed), 1);
        assert_eq!(seg1_hits.load(Ordering::Relaxed), 1);
        assert_eq!(seg2_hits.load(Ordering::Relaxed), 1);
    }
}
