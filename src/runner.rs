use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use crate::api::{self, PlaySource};
use crate::client::HttpClient;
use crate::config::{clamp_fragments, read_config, resolve_download_dir, AppConfig};
use crate::hls::{DownloadConfig, HlsDownloader, ProgressUpdate};
use crate::kwik;

pub const MAX_LOGS: usize = 2000;
const MAX_EPISODE_RANGE: u32 = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: u64,
    pub time: f64,
    pub level: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressState {
    pub percent: Option<f64>,
    pub size: Option<String>,
    pub speed: Option<String>,
    pub eta: Option<String>,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeProgress {
    pub requested: Vec<String>,
    pub current: Option<String>,
    pub completed: Vec<String>,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnimeMeta {
    pub slug: String,
    pub title: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobState {
    pub running: bool,
    pub exit_code: Option<i32>,
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
    pub args: Vec<String>,
    pub command: String,
    pub anime: AnimeMeta,
    pub episodes: EpisodeProgress,
    pub progress: ProgressState,
    pub playlists: Vec<String>,
    pub list_only: bool,
    pub stopped: bool,
    #[serde(default)]
    pub paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<Vec<LogEntry>>,
    pub log_seq: u64,
}

impl Default for JobState {
    fn default() -> Self {
        Self {
            running: false,
            exit_code: None,
            started_at: None,
            finished_at: None,
            args: Vec::new(),
            command: String::new(),
            anime: AnimeMeta {
                slug: String::new(),
                title: String::new(),
                name: String::new(),
            },
            episodes: EpisodeProgress {
                requested: Vec::new(),
                current: None,
                completed: Vec::new(),
                failed: Vec::new(),
            },
            progress: ProgressState {
                percent: None,
                size: None,
                speed: None,
                eta: None,
                file: None,
            },
            playlists: Vec::new(),
            list_only: false,
            stopped: false,
            paused: false,
            logs: None,
            log_seq: 0,
        }
    }
}

pub struct Runner {
    state: Arc<Mutex<JobState>>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    seq: Arc<AtomicU64>,
    stop_signal: Arc<AtomicBool>,
    pause_signal: Arc<AtomicBool>,
    pause_notify: Arc<tokio::sync::Notify>,
    event_tx: broadcast::Sender<String>,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

fn probe_dir_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let test_file = dir.join(format!(
        ".paheflow_probe_{}_{}.tmp",
        std::process::id(),
        fastrand::u32(..)
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&test_file)
    {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(test_file);
            true
        }
        Err(_) => false,
    }
}

impl Runner {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(100);
        Self {
            state: Arc::new(Mutex::new(JobState::default())),
            logs: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LOGS))),
            seq: Arc::new(AtomicU64::new(0)),
            stop_signal: Arc::new(AtomicBool::new(false)),
            pause_signal: Arc::new(AtomicBool::new(false)),
            pause_notify: Arc::new(tokio::sync::Notify::new()),
            event_tx: tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.event_tx.subscribe()
    }

    pub async fn snapshot(&self, include_logs: bool) -> JobState {
        let mut st = self.state.lock().await.clone();
        st.log_seq = self.seq.load(Ordering::Relaxed);
        if include_logs {
            let logs_guard = self.logs.lock().await;
            st.logs = Some(logs_guard.iter().cloned().collect());
        }
        st
    }

    pub async fn logs_since(&self, after: u64) -> Vec<LogEntry> {
        let logs_guard = self.logs.lock().await;
        logs_guard
            .iter()
            .filter(|e| e.seq > after)
            .cloned()
            .collect()
    }

    pub async fn clear_logs(&self) {
        let mut logs_guard = self.logs.lock().await;
        logs_guard.clear();
    }

    #[allow(dead_code)]
    pub async fn add_log(&self, level: &str, text: &str) {
        let next_seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = LogEntry {
            seq: next_seq,
            time: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            level: level.to_string(),
            text: text.to_string(),
        };

        {
            let mut logs_guard = self.logs.lock().await;
            if logs_guard.len() >= MAX_LOGS {
                logs_guard.pop_front();
            }
            logs_guard.push_back(entry.clone());
        }

        let _ = self
            .event_tx
            .send(serde_json::to_string(&entry).unwrap_or_default());
    }

    pub async fn pause(&self) -> bool {
        let mut st = self.state.lock().await;
        if st.running && !st.paused {
            self.pause_signal.store(true, Ordering::Relaxed);
            st.paused = true;
            st.progress.speed = Some("Paused".to_string());
            drop(st);
            self.add_log("info", "Download paused.").await;
            true
        } else {
            false
        }
    }

    pub async fn resume(&self) -> bool {
        let mut st = self.state.lock().await;
        if st.running && st.paused {
            self.pause_signal.store(false, Ordering::Relaxed);
            st.paused = false;
            self.pause_notify.notify_waiters();
            drop(st);
            self.add_log("info", "Download resumed.").await;
            true
        } else {
            false
        }
    }

    pub async fn stop(&self) -> bool {
        let mut st = self.state.lock().await;
        if st.running {
            self.stop_signal.store(true, Ordering::Relaxed);
            self.pause_signal.store(false, Ordering::Relaxed);
            self.pause_notify.notify_waiters();
            st.stopped = true;
            st.paused = false;
            true
        } else {
            false
        }
    }

    pub async fn start(
        &self,
        spec: serde_json::Value,
        client: HttpClient,
    ) -> Result<JobState, String> {
        let mut st = self.state.lock().await;
        if st.running {
            return Err("A job is already running — stop it first.".to_string());
        }

        let slug = spec
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let title = spec
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let episode_spec = spec
            .get("episodes")
            .and_then(|v| v.as_str())
            .unwrap_or("*")
            .trim()
            .to_string();
        let resolution = spec
            .get("resolution")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let audio_raw = spec
            .get("audio")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let audio = match audio_raw.to_lowercase().as_str() {
            "dub" => "eng".to_string(),
            "sub" => "jpn".to_string(),
            other => other.to_string(),
        };
        let replace = spec
            .get("replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let list_only = spec
            .get("listOnly")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if slug.is_empty() {
            return Err("No anime selected.".to_string());
        }
        if !valid_slug(&slug) {
            return Err("Invalid anime slug.".to_string());
        }

        let resolved_title = if !title.is_empty() {
            title.clone()
        } else {
            let cat_title = api::title_for_slug(&slug);
            if !cat_title.is_empty() {
                cat_title
            } else {
                slug.clone()
            }
        };
        let folder_name = sanitize_folder_name(&resolved_title, &slug);

        api::ensure_entry(&slug, &resolved_title);

        let cfg = read_config();
        let fragments = clamp_fragments(
            spec.get("concurrent_fragments")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(cfg.concurrent_fragments),
        );

        let download_root = resolve_download_dir(&cfg);
        let planned_dir = download_root.join(&folder_name);
        let mut anime_dir = planned_dir.clone();
        let mut storage_warning: Option<String> = None;

        if !probe_dir_writable(&anime_dir) {
            eprintln!("[Runner] Planned anime dir {:?} is not writable", anime_dir);
            let default_root = crate::config::default_download_dir();
            let default_anime_dir = default_root.join(&folder_name);
            if default_anime_dir != planned_dir && probe_dir_writable(&default_anime_dir) {
                anime_dir = default_anime_dir;
                storage_warning = Some(format!(
                    "Notice: Folder '{}' is not writable (Operation not permitted). Saving to default location: '{}'.",
                    planned_dir.display(),
                    anime_dir.display()
                ));
            } else {
                let app_ext_dir = PathBuf::from(
                    "/storage/emulated/0/Android/data/com.animepahe.app/files/Download",
                )
                .join(&folder_name);
                if probe_dir_writable(&app_ext_dir) {
                    anime_dir = app_ext_dir;
                    storage_warning = Some(format!(
                        "Storage permission missing for '{}' (Operation not permitted). Saving anime to app storage: '{}'. Grant 'All files access' in Android Settings to save directly to Download/PaheFlow.",
                        planned_dir.display(),
                        anime_dir.display()
                    ));
                } else {
                    let internal_dir = crate::config::data_dir()
                        .join("downloads")
                        .join(&folder_name);
                    let _ = std::fs::create_dir_all(&internal_dir);
                    anime_dir = internal_dir;
                    storage_warning = Some(format!(
                        "Storage permission missing for '{}'. Saving to internal app storage: '{}'. Grant 'All files access' in Android Settings to save directly to Download/PaheFlow.",
                        planned_dir.display(),
                        anime_dir.display()
                    ));
                }
            }
        }

        let requested_episodes = parse_episode_spec(&episode_spec);

        self.stop_signal.store(false, Ordering::Relaxed);
        self.pause_signal.store(false, Ordering::Relaxed);

        *st = JobState {
            running: true,
            exit_code: None,
            started_at: Some(chrono::Utc::now().timestamp_millis() as f64 / 1000.0),
            finished_at: None,
            args: Vec::new(),
            command: format!(
                "Native download · {} · episodes {}",
                resolved_title, episode_spec
            ),
            anime: AnimeMeta {
                slug: slug.clone(),
                title: resolved_title.clone(),
                name: resolved_title.clone(),
            },
            episodes: EpisodeProgress {
                requested: requested_episodes.clone(),
                current: None,
                completed: Vec::new(),
                failed: Vec::new(),
            },
            progress: ProgressState {
                percent: None,
                size: None,
                speed: None,
                eta: None,
                file: None,
            },
            playlists: Vec::new(),
            list_only,
            stopped: false,
            paused: false,
            logs: None,
            log_seq: self.seq.load(Ordering::Relaxed),
        };

        // Spawn background worker
        let state_arc = self.state.clone();
        let stop_arc = self.stop_signal.clone();
        let pause_arc = self.pause_signal.clone();
        let pause_notify_arc = self.pause_notify.clone();
        let runner_seq = self.seq.clone();
        let event_tx = self.event_tx.clone();
        let logs_arc = self.logs.clone();

        tokio::spawn(async move {
            let runner_helper = RunnerHelper {
                state: state_arc,
                stop: stop_arc,
                pause: pause_arc,
                pause_notify: pause_notify_arc,
                seq: runner_seq,
                event_tx,
                logs: logs_arc,
            };

            runner_helper
                .run_job(JobSpec {
                    client,
                    slug,
                    anime_title: resolved_title,
                    requested_episodes,
                    resolution,
                    audio,
                    replace,
                    list_only,
                    fragments,
                    anime_dir,
                    storage_warning,
                    cfg,
                })
                .await;
        });

        Ok(st.clone())
    }
}

fn parse_episode_spec(spec: &str) -> Vec<String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return vec!["*".to_string()];
    }
    trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Slugs are UUID-like identifiers. Reject anything that could be injected
/// into URLs, catalog lines, or filesystem paths.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 128
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

const RESERVED_FOLDER_NAMES: &[&str] = &[
    "app",
    "static",
    "tests",
    "cache",
    ".git",
    ".zcode",
    "__pycache__",
    ".uv",
    "src",
    "src-tauri",
    "target",
    "tools",
    "anime.list",
    "config.json",
    "config.json.bak",
    "config.json.tmp",
    "cargo.toml",
    "cargo.lock",
    "readme.md",
];

fn is_forbidden_fs_char(c: char) -> bool {
    matches!(
        c,
        '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
    ) || c.is_control()
}

fn clean_component(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if is_forbidden_fs_char(c) { ' ' } else { c })
        .collect();
    cleaned
        .split_whitespace()
        .filter(|part| *part != "." && *part != "..")
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches('.')
        .trim()
        .to_string()
}

/// Build a single, safe directory name for an anime title. The result never
/// contains separators and cannot escape the download root or collide with
/// application folders in a project checkout.
fn sanitize_folder_name(title: &str, fallback: &str) -> String {
    let fallback = {
        let cleaned = clean_component(fallback);
        if cleaned.is_empty() {
            "anime".to_string()
        } else {
            cleaned
        }
    };

    let cleaned = clean_component(title);
    let mut name = if cleaned.is_empty() {
        fallback.clone()
    } else {
        cleaned
    };

    let lower = name.to_lowercase();
    if RESERVED_FOLDER_NAMES.contains(&lower.as_str()) {
        name = format!("{} (anime)", fallback);
    }

    let name: String = name.chars().take(120).collect();
    let name = name.trim().trim_end_matches('.').trim().to_string();
    if name.is_empty() {
        fallback.chars().take(120).collect()
    } else {
        name
    }
}

/// Keep an episode label usable as `"<label>.mp4"` without allowing path
/// separators or hidden-file names.
fn sanitize_episode_label(label: &str) -> String {
    let cleaned = label
        .chars()
        .map(|c| if is_forbidden_fs_char(c) { '_' } else { c })
        .collect::<String>();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        "episode".to_string()
    } else {
        cleaned.chars().take(80).collect()
    }
}

/// Generate destination filename differentiated by audio track so Sub and Dub
/// versions of an episode can coexist in the same directory without collision.
pub(crate) fn episode_filename(ep_num: &str, is_dub: bool) -> String {
    let clean = sanitize_episode_label(ep_num);
    if is_dub {
        format!("{} (Dub).mp4", clean)
    } else {
        format!("{}.mp4", clean)
    }
}

/// All parameters for a single download job, passed as one value to
/// [`RunnerHelper::run_job`] to avoid an overlong argument list.
struct JobSpec {
    client: HttpClient,
    slug: String,
    anime_title: String,
    requested_episodes: Vec<String>,
    resolution: String,
    audio: String,
    replace: bool,
    list_only: bool,
    fragments: usize,
    anime_dir: PathBuf,
    storage_warning: Option<String>,
    cfg: AppConfig,
}

struct RunnerHelper {
    state: Arc<Mutex<JobState>>,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    pause_notify: Arc<tokio::sync::Notify>,
    seq: Arc<AtomicU64>,
    event_tx: broadcast::Sender<String>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
}

impl RunnerHelper {
    async fn log(&self, level: &str, text: &str) {
        let next_seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = LogEntry {
            seq: next_seq,
            time: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            level: level.to_string(),
            text: text.to_string(),
        };

        {
            let mut logs_guard = self.logs.lock().await;
            if logs_guard.len() >= MAX_LOGS {
                logs_guard.pop_front();
            }
            logs_guard.push_back(entry.clone());
        }

        let _ = self
            .event_tx
            .send(serde_json::to_string(&entry).unwrap_or_default());
    }

    async fn run_job(&self, spec: JobSpec) {
        let JobSpec {
            client,
            slug,
            anime_title,
            requested_episodes,
            resolution,
            audio,
            replace,
            list_only,
            fragments,
            anime_dir,
            storage_warning,
            cfg,
        } = spec;
        self.log(
            "info",
            &format!("Starting download task for '{}'...", anime_title),
        )
        .await;

        if let Some(ref warn) = storage_warning {
            self.log("warn", warn).await;
        }

        let mut actual_slug = slug.clone();
        let mut releases_res = api::release_with_status(&client, &actual_slug, &cfg).await;
        if let Err(ref e) = releases_res {
            if e.contains("slug may have rotated") || e.contains("returned HTML instead of JSON") {
                if let Some(new_slug) =
                    api::resolve_active_slug(&client, &actual_slug, Some(&anime_title), &cfg).await
                {
                    self.log(
                        "info",
                        &format!(
                            "Detected rotated slug for '{}'. Auto-healed session to {}.",
                            anime_title, new_slug
                        ),
                    )
                    .await;
                    actual_slug = new_slug;
                    releases_res = api::release_with_status(&client, &actual_slug, &cfg).await;
                }
            }
        }

        // Fetch release list
        let releases = match releases_res {
            Ok(list) => {
                if let Some(warning) = &list.warning {
                    self.log("warn", warning).await;
                }
                list.items
            }
            Err(e) => {
                self.log("error", &format!("Failed to fetch episodes: {}", e))
                    .await;
                let mut st = self.state.lock().await;
                st.running = false;
                st.exit_code = Some(1);
                st.finished_at = Some(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
                return;
            }
        };

        // Expand episode list
        let mut episode_numbers = Vec::new();
        if requested_episodes.iter().any(|s| s == "*") {
            for item in &releases {
                let ep_str = match &item.episode {
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::String(s) => s.clone(),
                    _ => continue,
                };
                episode_numbers.push(ep_str);
            }
        } else {
            for req in &requested_episodes {
                if let Some((start_str, end_str)) = req.split_once('-') {
                    match (
                        start_str.trim().parse::<u32>(),
                        end_str.trim().parse::<u32>(),
                    ) {
                        (Ok(start), Ok(end)) if end >= start => {
                            if end - start > MAX_EPISODE_RANGE {
                                self.log(
                                    "warn",
                                    &format!(
                                        "Episode range '{}' is larger than {} episodes; skipping.",
                                        req, MAX_EPISODE_RANGE
                                    ),
                                )
                                .await;
                            } else {
                                for n in start..=end {
                                    episode_numbers.push(n.to_string());
                                }
                            }
                        }
                        (Ok(_), Ok(_)) => {
                            self.log(
                                "warn",
                                &format!("Episode range '{}' is reversed; skipping.", req),
                            )
                            .await;
                        }
                        _ => episode_numbers.push(req.clone()),
                    }
                } else {
                    episode_numbers.push(req.clone());
                }
            }
        }

        {
            // Publish the expanded list so progress totals are accurate for
            // ranges and for "*" across multi-page catalogues.
            let mut st = self.state.lock().await;
            st.episodes.requested = episode_numbers.clone();
        }

        let downloader = HlsDownloader::new(Some(&cfg.ua));
        let mut failures = 0;

        for ep_num in episode_numbers {
            while self.pause.load(Ordering::Relaxed) && !self.stop.load(Ordering::Relaxed) {
                self.pause_notify.notified().await;
            }

            if self.stop.load(Ordering::Relaxed) {
                self.log("warn", "Download job cancelled by user.").await;
                break;
            }

            {
                let mut st = self.state.lock().await;
                st.episodes.current = Some(ep_num.clone());
            }

            self.log("info", &format!("Processing Episode {}...", ep_num))
                .await;

            let target_ep = releases.iter().find(|e| {
                let ep_str = match &e.episode {
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                ep_str == ep_num || ep_str.trim_start_matches('0') == ep_num.trim_start_matches('0')
            });

            let ep_session = match target_ep {
                Some(e) => &e.session,
                None => {
                    self.log(
                        "warn",
                        &format!("Episode {} not found in release list.", ep_num),
                    )
                    .await;
                    failures += 1;
                    continue;
                }
            };

            // Fetch play sources
            let sources = match api::play_sources(&client, &actual_slug, ep_session, &cfg).await {
                Ok(s) => s,
                Err(e) => {
                    self.log(
                        "warn",
                        &format!("Episode {}: could not load play page: {}", ep_num, e),
                    )
                    .await;
                    failures += 1;
                    continue;
                }
            };

            let chosen_source = select_best_source(&sources, &resolution, &audio);
            let source_url = match chosen_source {
                Some(s) => &s.url,
                None => {
                    self.log(
                        "warn",
                        &format!("Episode {}: no playable source found.", ep_num),
                    )
                    .await;
                    failures += 1;
                    continue;
                }
            };

            // Fetch Kwik page to extract m3u8
            let kwik_html = match client
                .fetch(source_url, Some("https://animepahe.pw/"), &cfg)
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    self.log(
                        "warn",
                        &format!("Episode {}: could not load stream page: {}", ep_num, e),
                    )
                    .await;
                    failures += 1;
                    continue;
                }
            };

            let m3u8_url = match kwik::extract_m3u8_from_kwik(&kwik_html) {
                Some(u) => u,
                None => {
                    self.log(
                        "warn",
                        &format!("Episode {}: stream resolver did not find playlist.", ep_num),
                    )
                    .await;
                    failures += 1;
                    continue;
                }
            };

            if list_only {
                self.log("info", &format!("Playlist: {}", m3u8_url)).await;
                let mut st = self.state.lock().await;
                st.playlists.push(m3u8_url);
                st.episodes.completed.push(ep_num);
                continue;
            }

            let is_dub = chosen_source
                .map(|s| s.audio.eq_ignore_ascii_case("eng") || s.audio.eq_ignore_ascii_case("dub"))
                .unwrap_or_else(|| {
                    audio.eq_ignore_ascii_case("eng") || audio.eq_ignore_ascii_case("dub")
                });

            let filename = episode_filename(&ep_num, is_dub);
            let final_file = anime_dir.join(&filename);
            if final_file.exists() && !replace {
                self.log(
                    "info",
                    &format!(
                        "Episode {} already exists ({}); skipping (enable Replace existing to replace).",
                        ep_num, filename
                    ),
                )
                .await;
                let mut st = self.state.lock().await;
                st.episodes.completed.push(ep_num);
                continue;
            }

            let size_info = chosen_source
                .and_then(|s| s.filesize.as_ref())
                .map(|sz| format!(" ({})", sz))
                .unwrap_or_default();

            self.log(
                "info",
                &format!(
                    "Downloading Episode {}{} with {} fragments...",
                    ep_num, size_info, fragments
                ),
            )
            .await;

            let state_for_cb = self.state.clone();
            let stop_for_dl = self.stop.clone();
            let pause_for_dl = self.pause.clone();
            let pause_notify_for_dl = self.pause_notify.clone();

            let dl_res = downloader
                .download_stream(DownloadConfig {
                    m3u8_url: m3u8_url.clone(),
                    referer: Some("https://kwik.cx/".to_string()),
                    dest_path: final_file.clone(),
                    replace_existing: replace,
                    concurrency: fragments,
                    stop_signal: stop_for_dl,
                    pause_signal: pause_for_dl,
                    pause_notify: pause_notify_for_dl,
                    progress_cb: move |update: ProgressUpdate| {
                        let st_clone = state_for_cb.clone();
                        tokio::spawn(async move {
                            let mut st = st_clone.lock().await;
                            st.progress.percent = Some(update.percent);
                            st.progress.size = Some(update.size_str);
                            st.progress.speed = Some(update.speed_str);
                            st.progress.eta = Some(update.eta_str);
                            st.progress.file = Some(update.file_name);
                        });
                    },
                })
                .await;

            match dl_res {
                Ok(_) => {
                    self.log(
                        "info",
                        &format!("Episode {} successfully downloaded.", ep_num),
                    )
                    .await;
                    let mut st = self.state.lock().await;
                    st.episodes.completed.push(ep_num);
                }
                Err(e) => {
                    self.log(
                        "error",
                        &format!("Episode {} download failed: {}", ep_num, e),
                    )
                    .await;
                    failures += 1;
                    let mut st = self.state.lock().await;
                    st.episodes.failed.push(ep_num);
                }
            }
        }

        let mut st = self.state.lock().await;
        st.running = false;
        st.episodes.current = None;
        st.exit_code = Some(if failures > 0 { 1 } else { 0 });
        st.finished_at = Some(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);

        if failures > 0 {
            self.log(
                "error",
                &format!("Batch finished with {} failed episode(s).", failures),
            )
            .await;
            send_desktop_notification(
                "PaheFlow Download Warning",
                &format!(
                    "'{}': batch finished with {} failed episode(s).",
                    anime_title, failures
                ),
                true,
            );
        } else {
            self.log("info", "All episodes finished successfully.")
                .await;
            send_desktop_notification(
                "PaheFlow Download Complete",
                &format!(
                    "All requested episodes for '{}' downloaded successfully!",
                    anime_title
                ),
                false,
            );
        }
    }
}

fn send_desktop_notification(summary: &str, body: &str, is_error: bool) {
    if which::which("notify-send").is_ok() {
        let mut cmd = std::process::Command::new("notify-send");
        cmd.arg("-a").arg("PaheFlow").arg("-i").arg(if is_error {
            "dialog-warning"
        } else {
            "video-x-generic"
        });
        if is_error {
            cmd.arg("-u").arg("critical");
        }
        cmd.arg(summary).arg(body);
        let _ = cmd.spawn();
    }
}

fn select_best_source<'a>(
    sources: &'a [PlaySource],
    resolution: &str,
    audio: &str,
) -> Option<&'a PlaySource> {
    let mut filtered: Vec<&PlaySource> = sources.iter().collect();

    let lowered = audio.trim().to_lowercase();
    let norm_audio = match lowered.as_str() {
        "dub" => "eng",
        "sub" => "jpn",
        other => other,
    };

    if !norm_audio.is_empty() {
        let audio_matches: Vec<&PlaySource> = filtered
            .iter()
            .cloned()
            .filter(|s| s.audio.eq_ignore_ascii_case(norm_audio))
            .collect();
        if !audio_matches.is_empty() {
            filtered = audio_matches;
        }
    }

    if !resolution.is_empty() {
        let res_matches: Vec<&PlaySource> = filtered
            .iter()
            .cloned()
            .filter(|s| s.resolution == resolution)
            .collect();
        if !res_matches.is_empty() {
            return res_matches.into_iter().next();
        }
    }

    // Prefer kwik sources, highest resolution
    filtered.sort_by(|a, b| {
        let ar: u32 = a.resolution.parse().unwrap_or(0);
        let br: u32 = b.resolution.parse().unwrap_or(0);
        br.cmp(&ar)
    });

    filtered.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_slugs() {
        assert!(valid_slug("92c6126b-c9ad-e1be-81d8-71f159c422b0"));
        assert!(valid_slug("abc_DEF-123"));
        assert!(!valid_slug(""));
        assert!(!valid_slug("../etc/passwd"));
        assert!(!valid_slug("slug?x=1"));
        assert!(!valid_slug("slug/with/slashes"));
    }

    #[test]
    fn sanitizes_anime_folder_names() {
        assert_eq!(sanitize_folder_name("Frieren", "fallback"), "Frieren");
        assert_eq!(sanitize_folder_name("../../etc", "fallback"), "etc");
        assert_eq!(
            sanitize_folder_name("Show / Season 1", "fallback"),
            "Show Season 1"
        );
        assert_eq!(
            sanitize_folder_name("Anitore! XX: Hitotsu Yane no Shita de Specials", "fallback"),
            "Anitore! XX Hitotsu Yane no Shita de Specials"
        );
        assert_eq!(
            sanitize_folder_name("Fate/stay night: Heaven's Feel", "fallback"),
            "Fate stay night Heaven's Feel"
        );
        assert_eq!(sanitize_folder_name("src", "fallback"), "fallback (anime)");
        assert_eq!(sanitize_folder_name("..", "fallback"), "fallback");
        assert_eq!(sanitize_folder_name("   ", "fallback"), "fallback");
    }

    #[test]
    fn sanitizes_episode_file_names() {
        assert_eq!(sanitize_episode_label("12"), "12");
        assert_eq!(sanitize_episode_label("../evil"), "_evil");
        assert_eq!(sanitize_episode_label("SP: 1"), "SP_ 1");
        assert_eq!(sanitize_episode_label("12*special?"), "12_special_");
        assert_eq!(sanitize_episode_label(".."), "episode");
        assert_eq!(sanitize_episode_label(""), "episode");
    }

    #[test]
    fn generates_audio_aware_episode_filenames() {
        assert_eq!(episode_filename("1", false), "1.mp4");
        assert_eq!(episode_filename("1", true), "1 (Dub).mp4");
        assert_eq!(episode_filename("02", false), "02.mp4");
        assert_eq!(episode_filename("02", true), "02 (Dub).mp4");
        assert_eq!(episode_filename("12.5", false), "12.5.mp4");
        assert_eq!(episode_filename("12.5", true), "12.5 (Dub).mp4");
    }

    #[test]
    fn test_probe_dir_writable() {
        let temp = std::env::temp_dir().join(format!("paheflow_test_{}", fastrand::u32(..)));
        assert!(probe_dir_writable(&temp));
        let _ = std::fs::remove_dir_all(&temp);

        let non_writable = Path::new("/proc/sys/fs/probe_fail_test");
        assert!(!probe_dir_writable(non_writable));
    }
}
