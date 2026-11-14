use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex};

use crate::api::{self, ep_matches, PlaySource};
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

/// Per-episode state inside a queue entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeItem {
    pub ep_id: String,  // e.g. "1", "2.5", "3"
    pub label: String,  // display label e.g. "EP 01"
    pub status: String, // "queued" | "downloading" | "done" | "failed" | "paused" | "skipped"
    pub percent: f64,
    pub speed: String,
    pub eta: String,
    pub size: String,
    pub error: String,
}

/// One anime waiting for (or undergoing) a queue-driven download.
/// `status` is one of "queued", "downloading", "failed".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueueEntry {
    pub id: u64,
    pub slug: String,
    pub title: String,
    pub episodes: String,
    pub resolution: String,
    pub audio: String,
    pub replace: bool,
    pub list_only: bool,
    pub status: String,
    #[serde(default)]
    pub error: String,
    pub added_at: f64,
    #[serde(default)]
    pub episode_items: Vec<EpisodeItem>,
    #[serde(default)]
    pub expanded_from_releases: bool,
}

impl QueueEntry {
    /// Adapt this entry into the same JSON spec `Runner::start` accepts.
    pub fn to_spec_value(&self) -> serde_json::Value {
        serde_json::json!({
            "slug": self.slug,
            "title": self.title,
            "episodes": self.episodes,
            "resolution": self.resolution,
            "audio": self.audio,
            "replace": self.replace,
            "listOnly": self.list_only,
        })
    }
}

/// On-disk representation of the download queue (`queue.json` next to
/// `config.json`). Written after every queue mutation so the queue survives
/// app restarts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct QueueFile {
    pub version: u32,
    pub hold: bool,
    pub next_id: u64,
    pub entries: Vec<QueueEntry>,
}

pub fn queue_file_path() -> PathBuf {
    crate::config::data_dir().join("queue.json")
}

/// Serialize + atomically replace the queue file (tmp file, then rename).
pub fn save_queue_file(path: &Path, file: &QueueFile) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Load the queue file; a missing or corrupted file falls back to an empty
/// queue. A "downloading" mark can never survive a restart meaningfully, so
/// it is normalized back to "queued".
pub fn load_queue_file(path: &Path) -> QueueFile {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(_) => return QueueFile::default(),
    };
    let mut file: QueueFile = match serde_json::from_slice(&data) {
        Ok(f) => f,
        Err(_) => return QueueFile::default(),
    };
    if file.version == 0 {
        file.version = 1;
    }
    for entry in &mut file.entries {
        if entry.status == "downloading" {
            entry.status = "queued".to_string();
        }
    }
    if file.next_id <= file.entries.iter().map(|e| e.id).max().unwrap_or(0) {
        file.next_id = file.entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
    }
    file
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
    /// Snapshot of the download queue, delivered with every state update.
    #[serde(default)]
    pub queue: Vec<QueueEntry>,
    /// True when the queue pump is holding off (manual stop / user pause).
    #[serde(default)]
    pub queue_hold: bool,
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
            queue: Vec::new(),
            queue_hold: false,
        }
    }
}

pub struct Runner {
    state: Arc<Mutex<JobState>>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    seq: Arc<AtomicU64>,
    stop_signal: Arc<AtomicBool>,
    pause_signal: Arc<AtomicBool>,
    ep_cancel_signal: Arc<AtomicBool>,
    job_pause_signal: Arc<AtomicBool>,
    suppress_hold_on_stop: Arc<AtomicBool>,
    pause_notify: Arc<tokio::sync::Notify>,
    pump_notify: Arc<tokio::sync::Notify>,
    event_tx: broadcast::Sender<String>,
    /// FIFO of queued anime. Lock ordering: `state` first, then `queue`.
    /// Never hold the `queue` lock when acquiring `state` (or logging).
    queue: Arc<Mutex<Vec<QueueEntry>>>,
    queue_hold: Arc<AtomicBool>,
    queue_next_id: Arc<AtomicU64>,
    /// Consecutive auto-started jobs that ended failed; trips the pump breaker.
    queue_failures: Arc<AtomicU64>,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner {
    pub fn cancel_current_ep(&self) {
        self.ep_cancel_signal.store(true, Ordering::Relaxed);
        self.pause_notify.notify_waiters();
    }
}

fn probe_dir_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let test_file = dir.join(format!(
        "paheflow_probe_{}_{}.tmp",
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
        let persisted = load_queue_file(&queue_file_path());
        let next_id = persisted.next_id.max(1);
        Self {
            state: Arc::new(Mutex::new(JobState::default())),
            logs: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LOGS))),
            seq: Arc::new(AtomicU64::new(0)),
            stop_signal: Arc::new(AtomicBool::new(false)),
            pause_signal: Arc::new(AtomicBool::new(false)),
            ep_cancel_signal: Arc::new(AtomicBool::new(false)),
            job_pause_signal: Arc::new(AtomicBool::new(false)),
            suppress_hold_on_stop: Arc::new(AtomicBool::new(false)),
            pause_notify: Arc::new(tokio::sync::Notify::new()),
            pump_notify: Arc::new(tokio::sync::Notify::new()),
            event_tx: tx,
            queue: Arc::new(Mutex::new(persisted.entries)),
            queue_hold: Arc::new(AtomicBool::new(persisted.hold)),
            queue_next_id: Arc::new(AtomicU64::new(next_id)),
            queue_failures: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.event_tx.subscribe()
    }

    pub async fn snapshot(&self, include_logs: bool) -> JobState {
        let mut st = self.state.lock().await.clone();
        st.log_seq = self.seq.load(Ordering::Relaxed);
        let mut queue = self.queue.lock().await.clone();
        st.queue_hold = self.queue_hold.load(Ordering::Relaxed);

        // Mirror episode progress from JobState into the active downloading
        // queue entry's episode_items so the frontend gets live per-episode state.
        if st.running {
            let job_slug = st.anime.slug.clone();
            if let Some(entry) = queue
                .iter_mut()
                .find(|e| e.slug == job_slug && e.status == "downloading")
            {
                // Mark completed episodes as "done"
                for ep_id in &st.episodes.completed {
                    if let Some(item) = entry.episode_items.iter_mut().find(|i| &i.ep_id == ep_id) {
                        if item.status != "paused" && item.status != "skipped" {
                            item.status = "done".to_string();
                            item.percent = 100.0;
                        }
                    } else if entry.episodes == "*" {
                        // Lazy-populate: add item for a newly discovered episode
                        let label = if let Ok(n) = ep_id.parse::<u32>() {
                            format!("EP {:02}", n)
                        } else {
                            format!("EP {}", ep_id)
                        };
                        entry.episode_items.push(EpisodeItem {
                            ep_id: ep_id.clone(),
                            label,
                            status: "done".to_string(),
                            percent: 100.0,
                            speed: String::new(),
                            eta: String::new(),
                            size: String::new(),
                            error: String::new(),
                        });
                    }
                }
                // Mark failed episodes
                for ep_id in &st.episodes.failed {
                    if let Some(item) = entry.episode_items.iter_mut().find(|i| &i.ep_id == ep_id) {
                        if item.status != "paused" && item.status != "skipped" {
                            item.status = "failed".to_string();
                        }
                    } else if entry.episodes == "*" {
                        let label = if let Ok(n) = ep_id.parse::<u32>() {
                            format!("EP {:02}", n)
                        } else {
                            format!("EP {}", ep_id)
                        };
                        entry.episode_items.push(EpisodeItem {
                            ep_id: ep_id.clone(),
                            label,
                            status: "failed".to_string(),
                            percent: 0.0,
                            speed: String::new(),
                            eta: String::new(),
                            size: String::new(),
                            error: String::new(),
                        });
                    }
                }
                // Mark currently downloading episode with live progress
                if let Some(ref cur_ep) = st.episodes.current.clone() {
                    if let Some(item) = entry.episode_items.iter_mut().find(|i| &i.ep_id == cur_ep)
                    {
                        if item.status != "paused"
                            && item.status != "skipped"
                            && item.status != "done"
                            && item.status != "failed"
                        {
                            item.status = "downloading".to_string();
                            item.percent = st.progress.percent.unwrap_or(0.0);
                            item.speed = st.progress.speed.clone().unwrap_or_default();
                            item.eta = st.progress.eta.clone().unwrap_or_default();
                            item.size = st.progress.size.clone().unwrap_or_default();
                        }
                    } else if entry.episodes == "*" {
                        let label = if let Ok(n) = cur_ep.parse::<u32>() {
                            format!("EP {:02}", n)
                        } else {
                            format!("EP {}", cur_ep)
                        };
                        entry.episode_items.push(EpisodeItem {
                            ep_id: cur_ep.clone(),
                            label,
                            status: "downloading".to_string(),
                            percent: st.progress.percent.unwrap_or(0.0),
                            speed: st.progress.speed.clone().unwrap_or_default(),
                            eta: st.progress.eta.clone().unwrap_or_default(),
                            size: st.progress.size.clone().unwrap_or_default(),
                            error: String::new(),
                        });
                    }
                }
            }
        }

        st.queue = queue;
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
        let snapshot = {
            let mut st = self.state.lock().await;
            if !st.running {
                return false;
            }
            let active_slug = st.anime.slug.clone();
            let cur_ep = st.episodes.current.clone();
            let cur_pct = st.progress.percent.unwrap_or(0.0);
            let cur_size = st.progress.size.clone().unwrap_or_default();

            self.job_pause_signal.store(true, Ordering::Relaxed);
            self.ep_cancel_signal.store(true, Ordering::Relaxed);
            self.pause_signal.store(false, Ordering::Relaxed);
            st.paused = false;
            st.episodes.current = None;
            st.progress = ProgressState::default();
            (active_slug, cur_ep, cur_pct, cur_size)
        };

        let (active_slug, cur_ep, cur_pct, cur_size) = snapshot;
        let (paused_title, has_next_queued) = {
            let mut q = self.queue.lock().await;
            let mut title = None;
            let idx = q
                .iter()
                .position(|e| e.slug == active_slug && e.status == "downloading")
                .or_else(|| q.iter().position(|e| e.slug == active_slug));
            if let Some(idx) = idx {
                let entry = &mut q[idx];
                title = Some(entry.title.clone());
                for item in &mut entry.episode_items {
                    if item.status == "downloading"
                        || (cur_ep.as_deref() == Some(item.ep_id.as_str())
                            && item.status == "queued")
                    {
                        item.status = "paused".to_string();
                        if cur_ep.as_deref() == Some(item.ep_id.as_str()) {
                            if cur_pct > item.percent {
                                item.percent = cur_pct;
                            }
                            if !cur_size.is_empty() {
                                item.size = cur_size.clone();
                            }
                        }
                        item.speed.clear();
                        item.eta.clear();
                    } else if item.status == "queued" {
                        item.status = "paused".to_string();
                    }
                }
                entry.status = "paused".to_string();
                entry.error.clear();
            }
            let has_next = q.iter().any(|e| {
                e.status == "queued"
                    && (e.episode_items.is_empty()
                        || e.episode_items.iter().any(|i| i.status == "queued"))
            });
            (title, has_next)
        };

        self.queue_persist().await;
        self.pause_notify.notify_waiters();
        self.pump_notify.notify_one();

        let msg = match (paused_title, has_next_queued) {
            (Some(t), true) => format!("Paused '{}' — starting next queued anime.", t),
            (Some(t), false) => format!("Paused '{}'.", t),
            (None, true) => "Download paused — starting next queued anime.".to_string(),
            (None, false) => "Download paused.".to_string(),
        };
        self.add_log("info", &msg).await;
        true
    }

    pub async fn resume(&self) -> bool {
        let active_slug = {
            let mut st = self.state.lock().await;
            if st.running && st.paused {
                self.pause_signal.store(false, Ordering::Relaxed);
                st.paused = false;
                self.pause_notify.notify_waiters();
                drop(st);
                self.add_log("info", "Download resumed.").await;
                return true;
            }
            if st.running {
                return false;
            }
            st.anime.slug.clone()
        };

        let resumed_title = {
            let mut q = self.queue.lock().await;
            let idx = q
                .iter()
                .position(|e| e.slug == active_slug && e.status == "paused")
                .or_else(|| q.iter().position(|e| e.status == "paused"));
            if let Some(i) = idx {
                for item in &mut q[i].episode_items {
                    if item.status == "paused" || item.status == "failed" {
                        item.status = "queued".to_string();
                        item.error.clear();
                    }
                }
                q[i].status = "queued".to_string();
                q[i].error.clear();
                Some(q[i].title.clone())
            } else {
                None
            }
        };

        if let Some(title) = resumed_title {
            self.queue_hold.store(false, Ordering::Relaxed);
            self.queue_failures.store(0, Ordering::Relaxed);
            self.queue_persist().await;
            self.pump_notify.notify_one();
            self.add_log("info", &format!("Resumed '{}'.", title)).await;
            return true;
        }

        if self.queue_hold.swap(false, Ordering::Relaxed) {
            self.queue_failures.store(0, Ordering::Relaxed);
            self.queue_persist().await;
            self.pump_notify.notify_one();
            self.add_log("info", "Download queue resumed.").await;
            return true;
        }

        false
    }

    pub async fn stop(&self) -> bool {
        let mut st = self.state.lock().await;
        if st.running {
            self.stop_signal.store(true, Ordering::Relaxed);
            self.pause_signal.store(false, Ordering::Relaxed);
            self.job_pause_signal.store(false, Ordering::Relaxed);
            self.ep_cancel_signal.store(true, Ordering::Relaxed);
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
        self.start_internal(spec, client, false).await
    }

    async fn start_internal(
        &self,
        spec: serde_json::Value,
        client: HttpClient,
        from_queue: bool,
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

        let app_ext_dir = std::env::var("ANIMEPAHE_APP_EXT_DOWNLOAD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from("/storage/emulated/0/Android/data/com.animepahe.app/files/Download")
            })
            .join(&folder_name);

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
            } else if probe_dir_writable(&app_ext_dir) {
                anime_dir = app_ext_dir.clone();
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

        let requested_episodes = parse_episode_spec(&episode_spec);

        self.stop_signal.store(false, Ordering::Relaxed);
        self.pause_signal.store(false, Ordering::Relaxed);
        self.ep_cancel_signal.store(false, Ordering::Relaxed);
        self.job_pause_signal.store(false, Ordering::Relaxed);
        self.suppress_hold_on_stop.store(false, Ordering::Relaxed);
        if !from_queue {
            self.queue_failures.store(0, Ordering::Relaxed);
        }

        let (queue_snapshot, queue_hold_snapshot, effective_requested) = {
            let mut q = self.queue.lock().await;
            let eff_req = if let Some(entry) = q.iter_mut().find(|e| e.slug == slug) {
                entry.status = "downloading".to_string();
                entry.error.clear();
                if from_queue {
                    if entry.episode_items.is_empty() {
                        entry.episode_items =
                            build_or_cached_episode_items(&episode_spec, &audio, &slug);
                    }
                } else {
                    entry.title = resolved_title.clone();
                    entry.resolution = resolution.clone();
                    entry.audio = audio.clone();
                    entry.replace = replace;
                    entry.list_only = list_only;
                    let new_items = build_or_cached_episode_items(&episode_spec, &audio, &slug);
                    if new_items.is_empty() {
                        entry.episodes = "*".to_string();
                        entry.expanded_from_releases = false;
                        for item in &mut entry.episode_items {
                            if item.status == "paused" || item.status == "failed" {
                                item.status = "queued".to_string();
                                item.error.clear();
                            }
                        }
                    } else {
                        entry.episodes = episode_spec.clone();
                        for new_item in new_items {
                            if let Some(existing) = entry
                                .episode_items
                                .iter_mut()
                                .find(|i| i.ep_id == new_item.ep_id)
                            {
                                if replace || existing.status != "done" {
                                    existing.status = "queued".to_string();
                                    existing.error.clear();
                                }
                            } else {
                                entry.episode_items.push(new_item);
                            }
                        }
                    }
                }
                if entry.episode_items.is_empty() {
                    requested_episodes.clone()
                } else {
                    entry
                        .episode_items
                        .iter()
                        .map(|i| i.ep_id.clone())
                        .collect()
                }
            } else {
                let id = self.queue_next_id.fetch_add(1, Ordering::Relaxed) + 1;
                let episode_items = build_or_cached_episode_items(&episode_spec, &audio, &slug);
                let eff = if episode_items.is_empty() {
                    requested_episodes.clone()
                } else {
                    episode_items.iter().map(|i| i.ep_id.clone()).collect()
                };
                q.insert(
                    0,
                    QueueEntry {
                        id,
                        slug: slug.clone(),
                        title: resolved_title.clone(),
                        episodes: episode_spec.clone(),
                        resolution: resolution.clone(),
                        audio: audio.clone(),
                        replace,
                        list_only,
                        status: "downloading".to_string(),
                        error: String::new(),
                        added_at: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
                        episode_items,
                        expanded_from_releases: false,
                    },
                );
                eff
            };
            (q.clone(), self.queue_hold.load(Ordering::Relaxed), eff_req)
        };
        self.queue_persist().await;

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
                requested: effective_requested,
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
            queue: queue_snapshot,
            queue_hold: queue_hold_snapshot,
        };

        // Spawn background worker
        let state_arc = self.state.clone();
        let stop_arc = self.stop_signal.clone();
        let pause_arc = self.pause_signal.clone();
        let ep_cancel_arc = self.ep_cancel_signal.clone();
        let job_pause_arc = self.job_pause_signal.clone();
        let suppress_hold_arc = self.suppress_hold_on_stop.clone();
        let pause_notify_arc = self.pause_notify.clone();
        let pump_notify_arc = self.pump_notify.clone();
        let runner_seq = self.seq.clone();
        let event_tx = self.event_tx.clone();
        let logs_arc = self.logs.clone();
        let queue_arc = self.queue.clone();
        let queue_hold_arc = self.queue_hold.clone();
        let queue_next_id_arc = self.queue_next_id.clone();

        tokio::spawn(async move {
            let runner_helper = RunnerHelper {
                state: state_arc,
                queue: queue_arc,
                queue_hold: queue_hold_arc,
                queue_next_id: queue_next_id_arc,
                stop: stop_arc,
                pause: pause_arc,
                ep_cancel: ep_cancel_arc,
                job_pause: job_pause_arc,
                suppress_hold_on_stop: suppress_hold_arc,
                pause_notify: pause_notify_arc,
                pump_notify: pump_notify_arc,
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
                    app_ext_dir,
                    storage_warning,
                    cfg,
                })
                .await;
        });

        Ok(st.clone())
    }

    /* ─────────────────────────────────────────────────────────────────
     *  Download queue
     *
     *  A FIFO of anime that the background pump (`run_queue_pump`) starts
     *  automatically whenever the engine is idle. Lock discipline: take
     *  the `state` lock (via `snapshot`) before the `queue` lock, and
     *  drop the `queue` lock before logging or persisting.
     * ───────────────────────────────────────────────────────────────── */

    /// Persist the current queue + hold state to `queue.json`.
    async fn queue_persist(&self) {
        let entries = self.queue.lock().await.clone();
        let file = QueueFile {
            version: 1,
            hold: self.queue_hold.load(Ordering::Relaxed),
            next_id: self.queue_next_id.load(Ordering::Relaxed),
            entries,
        };
        if let Err(e) = save_queue_file(&queue_file_path(), &file) {
            eprintln!("[Queue] Failed to save queue.json: {}", e);
        }
    }

    /// Add an anime to the download queue (or refresh its options when it is
    /// already queued). Clears a queue hold: interacting with the queue
    /// signals intent to continue.
    pub async fn queue_add(&self, spec: &serde_json::Value) -> Result<(String, bool), String> {
        let slug = spec
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if slug.is_empty() {
            return Err("No anime selected.".to_string());
        }
        if !valid_slug(&slug) {
            return Err("Invalid anime slug.".to_string());
        }

        let title = spec
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let resolved_title = if !title.is_empty() {
            title
        } else {
            let cat_title = api::title_for_slug(&slug);
            if cat_title.is_empty() {
                slug.clone()
            } else {
                cat_title
            }
        };

        let mut episodes = spec
            .get("episodes")
            .and_then(|v| v.as_str())
            .unwrap_or("*")
            .trim()
            .chars()
            .take(2000)
            .collect::<String>();
        if episodes.is_empty() {
            episodes = "*".to_string();
        }
        let resolution = spec
            .get("resolution")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .chars()
            .take(16)
            .collect::<String>();
        let audio_raw = spec
            .get("audio")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let audio = match audio_raw.to_lowercase().as_str() {
            "dub" => "eng".to_string(),
            "sub" => "jpn".to_string(),
            _ => audio_raw.chars().take(16).collect(),
        };
        let replace = spec
            .get("replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let list_only = spec
            .get("listOnly")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut q = self.queue.lock().await;
        if let Some(index) = q.iter().position(|e| e.slug == slug) {
            if q[index].status == "downloading" {
                let title = q[index].title.clone();
                let new_items = build_or_cached_episode_items(&episodes, &audio, &slug);
                let mut touched = 0usize;
                if new_items.is_empty() {
                    if episodes == "*" {
                        q[index].episodes = "*".to_string();
                        q[index].expanded_from_releases = false;
                        for item in &mut q[index].episode_items {
                            if item.status == "paused" || item.status == "failed" {
                                item.status = "queued".to_string();
                                item.error.clear();
                                touched += 1;
                            }
                        }
                    }
                } else {
                    for new_item in new_items {
                        if let Some(existing) = q[index]
                            .episode_items
                            .iter_mut()
                            .find(|i| i.ep_id == new_item.ep_id)
                        {
                            if existing.status == "paused"
                                || existing.status == "failed"
                                || (replace && existing.status == "done")
                            {
                                existing.status = "queued".to_string();
                                existing.error.clear();
                                touched += 1;
                            }
                        } else {
                            q[index].episode_items.push(new_item);
                            touched += 1;
                        }
                    }
                }
                drop(q);
                if touched > 0 {
                    self.queue_persist().await;
                    self.pump_notify.notify_one();
                    let msg = format!(
                        "Added/queued {} episode(s) to active download '{}'.",
                        touched, title
                    );
                    self.add_log("info", &msg).await;
                    return Ok((msg, true));
                }
                return Ok((format!("'{}' is already downloading.", title), true));
            }

            let has_progress = q[index]
                .episode_items
                .iter()
                .any(|i| i.status == "done" || i.status == "paused" || i.percent > 0.0);

            q[index].title = resolved_title.clone();
            q[index].resolution = resolution;
            q[index].audio = audio.clone();
            q[index].replace = replace;
            q[index].list_only = list_only;
            q[index].status = "queued".to_string();
            q[index].error.clear();

            let new_items = build_or_cached_episode_items(&episodes, &audio, &slug);
            if !has_progress {
                q[index].episodes = episodes;
                q[index].episode_items = new_items;
                q[index].expanded_from_releases = false;
            } else if new_items.is_empty() {
                q[index].episodes = "*".to_string();
                q[index].expanded_from_releases = false;
                for item in &mut q[index].episode_items {
                    if item.status == "paused" || item.status == "failed" {
                        item.status = "queued".to_string();
                        item.error.clear();
                    }
                }
            } else {
                q[index].episodes = episodes;
                for new_item in new_items {
                    if let Some(existing) = q[index]
                        .episode_items
                        .iter_mut()
                        .find(|i| i.ep_id == new_item.ep_id)
                    {
                        if replace || existing.status != "done" {
                            existing.status = "queued".to_string();
                            existing.error.clear();
                        }
                    } else {
                        q[index].episode_items.push(new_item);
                    }
                }
            }
            drop(q);
            self.queue_hold.store(false, Ordering::Relaxed);
            self.queue_failures.store(0, Ordering::Relaxed);
            self.queue_persist().await;
            self.pump_notify.notify_one();
            let message = format!(
                "'{}' already queued — options updated (position {}).",
                resolved_title,
                index + 1
            );
            self.add_log("info", &message).await;
            return Ok((message, true));
        }

        let id = self.queue_next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let position = q.len() + 1;
        let episode_items = build_or_cached_episode_items(&episodes, &audio, &slug);
        q.push(QueueEntry {
            id,
            slug,
            title: resolved_title.clone(),
            episodes,
            resolution,
            audio,
            replace,
            list_only,
            status: "queued".to_string(),
            error: String::new(),
            added_at: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            episode_items,
            expanded_from_releases: false,
        });
        drop(q);
        self.queue_hold.store(false, Ordering::Relaxed);
        self.queue_failures.store(0, Ordering::Relaxed);
        self.queue_persist().await;
        self.pump_notify.notify_one();
        let message = format!("'{}' queued (position {}).", resolved_title, position);
        self.add_log("info", &message).await;
        Ok((message, false))
    }

    /// Remove a queue entry by slug. If that entry is actively downloading,
    /// it cancels the download and removes it from the queue cleanly without
    /// pausing the rest of the queue.
    pub async fn queue_remove(&self, slug: &str) -> Result<String, String> {
        let is_active = {
            let st = self.state.lock().await;
            st.running && st.anime.slug == slug
        };
        if is_active {
            self.suppress_hold_on_stop.store(true, Ordering::Relaxed);
            self.stop().await;
        }
        let mut q = self.queue.lock().await;
        let position = q
            .iter()
            .position(|e| e.slug == slug)
            .ok_or_else(|| "That anime is not in the queue.".to_string())?;
        let title = q.remove(position).title;
        drop(q);
        self.queue_persist().await;
        self.pump_notify.notify_one();
        self.add_log("info", &format!("Removed '{}' from the queue.", title))
            .await;
        Ok(title)
    }

    /// Drop every entry that is not actively downloading.
    pub async fn queue_clear(&self) -> usize {
        let removed = {
            let mut q = self.queue.lock().await;
            let before = q.len();
            q.retain(|e| e.status == "downloading");
            before - q.len()
        };
        if removed > 0 {
            self.queue_persist().await;
            self.add_log("info", &format!("Cleared {} queue entries.", removed))
                .await;
        }
        removed
    }

    /// Pause (`hold = true`) or resume the automatic queue pump.
    pub async fn queue_set_hold(&self, hold: bool) {
        let previous = self.queue_hold.swap(hold, Ordering::Relaxed);
        if !hold && previous {
            // Explicit resume counts as fresh intent.
            self.queue_failures.store(0, Ordering::Relaxed);
        }
        self.queue_persist().await;
        if !hold {
            self.pump_notify.notify_one();
        }
        if hold != previous {
            self.add_log(
                "info",
                if hold {
                    "Download queue paused."
                } else {
                    "Download queue resumed."
                },
            )
            .await;
        }
    }

    /// Background loop: reap finished queue jobs and start the next queued
    /// entry whenever the engine is idle. Spawned once per server lifetime.
    pub async fn run_queue_pump(self: Arc<Self>, client: HttpClient) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(600)) => {}
                _ = self.pump_notify.notified() => {}
            }
            if self.queue_tick(client.clone()).await {
                self.queue_tick(client.clone()).await;
            }
        }
    }

    /// One iteration of the queue pump. Returns `true` if a finished or stale
    /// entry was reaped so the caller can immediately check for the next entry.
    async fn queue_tick(&self, client: HttpClient) -> bool {
        let snap = self.snapshot(false).await;
        let entries = self.queue.lock().await.clone();
        let hold = self.queue_hold.load(Ordering::Relaxed);

        match pump_decision(
            snap.running,
            snap.stopped,
            snap.exit_code,
            &snap.anime.slug,
            hold,
            &entries,
        ) {
            PumpDecision::Idle => false,
            PumpDecision::HoldAfterStop => {
                self.queue_hold.store(true, Ordering::Relaxed);
                self.queue_persist().await;
                self.add_log(
                    "warn",
                    "Download stopped — queue paused so nothing else starts. Resume it in the Queue tab.",
                )
                .await;
                false
            }
            PumpDecision::Requeue { id } => {
                if let Some(entry) = self.queue_entry(id).await {
                    {
                        let mut q = self.queue.lock().await;
                        if let Some(e) = q.iter_mut().find(|e| e.id == id) {
                            e.status = "queued".to_string();
                            e.error.clear();
                        }
                    }
                    self.queue_hold.store(true, Ordering::Relaxed);
                    self.queue_persist().await;
                    self.add_log(
                        "warn",
                        &format!(
                            "Stopped '{}' — it stays first in the queue. Queue paused; resume in the Queue tab.",
                            entry.title
                        ),
                    )
                    .await;
                }
                false
            }
            PumpDecision::RequeueIncomplete { id } => {
                if let Some(entry) = self.queue_entry(id).await {
                    {
                        let mut q = self.queue.lock().await;
                        if let Some(e) = q.iter_mut().find(|e| e.id == id) {
                            let has_queued = e.episode_items.iter().any(|i| i.status == "queued");
                            e.status = if has_queued {
                                "queued".to_string()
                            } else {
                                "paused".to_string()
                            };
                            e.error.clear();
                        }
                    }
                    self.queue_persist().await;
                    self.add_log(
                        "info",
                        &format!(
                            "Queue: '{}' paused with remaining episodes. Moving to next queued anime.",
                            entry.title
                        ),
                    )
                    .await;
                }
                true
            }
            PumpDecision::DropDone { id } => {
                let title = {
                    let mut q = self.queue.lock().await;
                    let index = q.iter().position(|e| e.id == id);
                    match index {
                        Some(i) => q.remove(i).title,
                        None => return false,
                    }
                };
                self.queue_failures.store(0, Ordering::Relaxed);
                self.queue_persist().await;
                let remaining = self
                    .queue
                    .lock()
                    .await
                    .iter()
                    .filter(|e| e.status == "queued")
                    .count();
                self.add_log(
                    "info",
                    &format!(
                        "Queue: '{}' finished. {}",
                        title,
                        if remaining > 0 {
                            format!("Next up: {} queued anime.", remaining)
                        } else {
                            "Queue is now empty.".to_string()
                        }
                    ),
                )
                .await;
                true
            }
            PumpDecision::MarkFailed { id } => {
                let failures = self.queue_failures.fetch_add(1, Ordering::Relaxed) + 1;
                let title = {
                    let mut q = self.queue.lock().await;
                    let index = q.iter().position(|e| e.id == id);
                    let Some(i) = index else { return false };
                    q[i].error = match snap.exit_code {
                        Some(0) => "Some episodes failed".to_string(),
                        Some(code) => format!("Download failed (exit {})", code),
                        None => "Download failed".to_string(),
                    };
                    if failures >= 3 {
                        // Breaker: repeated failures usually mean the network
                        // or Cloudflare cookie is broken. Pause the queue so we
                        // do not burn through the remaining entries.
                        q[i].status = "queued".to_string();
                        q[i].error.clear();
                    } else {
                        // Keep it visible as a failed entry; the pump moves on
                        // to later entries instead of restarting this one.
                        q[i].status = "failed".to_string();
                    }
                    q[i].title.clone()
                };
                if failures >= 3 {
                    self.queue_hold.store(true, Ordering::Relaxed);
                    self.queue_failures.store(0, Ordering::Relaxed);
                    self.queue_persist().await;
                    self.add_log(
                        "warn",
                        &format!(
                            "Queue paused after 3 consecutive failed downloads (last: '{}'). Check the connection / Cloudflare cookie, then resume in the Queue tab.",
                            title
                        ),
                    )
                    .await;
                    false
                } else {
                    self.queue_persist().await;
                    self.add_log(
                        "error",
                        &format!(
                            "Queue: '{}' failed. Trying the next entry in a moment…",
                            title
                        ),
                    )
                    .await;
                    true
                }
            }
            PumpDecision::RevertStale { id } => {
                let mut q = self.queue.lock().await;
                if let Some(e) = q.iter_mut().find(|e| e.id == id) {
                    e.status = "queued".to_string();
                }
                drop(q);
                self.queue_persist().await;
                true
            }
            PumpDecision::Start { id } => {
                self.queue_start_entry(id, client).await;
                false
            }
        }
    }

    /// Fetch a queue entry clone by id.
    async fn queue_entry(&self, id: u64) -> Option<QueueEntry> {
        self.queue.lock().await.iter().find(|e| e.id == id).cloned()
    }

    /// Mark an entry as downloading and hand it to the engine.
    async fn queue_start_entry(&self, id: u64, client: HttpClient) {
        let entry = {
            let mut q = self.queue.lock().await;
            let Some(i) = q.iter().position(|e| e.id == id) else {
                return;
            };
            if q[i].status != "queued" {
                return;
            }
            q[i].status = "downloading".to_string();
            q[i].error.clear();
            q[i].clone()
        };
        self.queue_persist().await;
        self.add_log(
            "info",
            &format!(
                "Queue: starting '{}' ({})…",
                entry.title,
                if entry.episodes == "*" {
                    "all episodes".to_string()
                } else {
                    format!("episodes {}", entry.episodes)
                }
            ),
        )
        .await;

        if let Err(e) = self
            .start_internal(entry.to_spec_value(), client, true)
            .await
        {
            {
                let mut q = self.queue.lock().await;
                if let Some(i) = q.iter().position(|e| e.id == id) {
                    if e.contains("already running") {
                        // A manual run raced us — retry once the engine frees up.
                        q[i].status = "queued".to_string();
                    } else {
                        q[i].status = "failed".to_string();
                        q[i].error = e.clone();
                    }
                }
            }
            self.queue_persist().await;
            self.add_log(
                "error",
                &format!("Queue: could not start '{}': {}", entry.title, e),
            )
            .await;
        }
    }

    /// Set status of a specific episode in a queue entry.
    pub async fn queue_ep_set_status(
        &self,
        queue_id: u64,
        ep_id: &str,
        status: &str,
    ) -> Result<(), String> {
        let (is_running, active_slug, active_cur_ep, cur_pct, cur_size) = {
            let st = self.state.lock().await;
            (
                st.running,
                st.anime.slug.clone(),
                st.episodes.current.clone(),
                st.progress.percent.unwrap_or(0.0),
                st.progress.size.clone().unwrap_or_default(),
            )
        };

        let (is_active_ep, has_remaining_queued) = {
            let mut q = self.queue.lock().await;
            let entry = q
                .iter_mut()
                .find(|e| e.id == queue_id)
                .ok_or_else(|| format!("Queue entry {} not found.", queue_id))?;
            let is_active_entry =
                is_running && entry.slug == active_slug && entry.status == "downloading";
            let is_active_ep = is_active_entry && active_cur_ep.as_deref() == Some(ep_id);

            let item = entry
                .episode_items
                .iter_mut()
                .find(|i| i.ep_id == ep_id)
                .ok_or_else(|| {
                    format!("Episode {} not found in queue entry {}.", ep_id, queue_id)
                })?;

            item.status = status.to_string();
            if status == "queued" {
                item.error.clear();
            } else if status == "paused" {
                item.speed.clear();
                item.eta.clear();
                if is_active_ep {
                    if cur_pct > item.percent {
                        item.percent = cur_pct;
                    }
                    if !cur_size.is_empty() {
                        item.size = cur_size.clone();
                    }
                }
            }

            let has_queued = entry.episode_items.iter().any(|i| i.status == "queued");
            if is_active_entry {
                if status == "paused" && !has_queued {
                    // No queued episodes left in the active anime: pause the anime so the next queued anime starts.
                    entry.status = "paused".to_string();
                }
            } else if status == "queued" {
                entry.status = "queued".to_string();
                entry.error.clear();
            } else {
                let all_incomplete_paused = entry
                    .episode_items
                    .iter()
                    .filter(|i| i.status != "done" && i.status != "skipped")
                    .all(|i| i.status == "paused" || i.status == "failed");
                if all_incomplete_paused {
                    entry.status = "paused".to_string();
                }
            }
            (is_active_ep, has_queued)
        };

        if status == "queued" {
            // Unpausing an episode indicates intent to resume downloading
            self.queue_hold.store(false, Ordering::Relaxed);
            self.queue_failures.store(0, Ordering::Relaxed);
        }
        self.queue_persist().await;

        if is_active_ep && status == "paused" {
            {
                let mut st = self.state.lock().await;
                if st.episodes.current.as_deref() == Some(ep_id) {
                    st.episodes.current = None;
                    st.progress = ProgressState::default();
                }
            }
            if !has_remaining_queued {
                self.job_pause_signal.store(true, Ordering::Relaxed);
            }
            self.cancel_current_ep();
            self.pump_notify.notify_one();
            self.add_log(
                "info",
                &if has_remaining_queued {
                    format!("Paused episode {} — starting next queued episode.", ep_id)
                } else {
                    format!(
                        "Paused episode {} (no queued episodes left) — starting next queued anime.",
                        ep_id
                    )
                },
            )
            .await;
        } else if status == "queued" {
            self.pump_notify.notify_one();
        }
        Ok(())
    }

    /// Set status of all non-finished episodes in a queue entry.
    pub async fn queue_group_set_status(&self, queue_id: u64, status: &str) -> Result<(), String> {
        let (is_running, active_slug, cur_ep, cur_pct, cur_size) = {
            let st = self.state.lock().await;
            (
                st.running,
                st.anime.slug.clone(),
                st.episodes.current.clone(),
                st.progress.percent.unwrap_or(0.0),
                st.progress.size.clone().unwrap_or_default(),
            )
        };

        let was_active = {
            let mut q = self.queue.lock().await;
            let entry = q
                .iter_mut()
                .find(|e| e.id == queue_id)
                .ok_or_else(|| format!("Queue entry {} not found.", queue_id))?;
            let is_active_entry =
                entry.status == "downloading" || (is_running && entry.slug == active_slug);
            for item in &mut entry.episode_items {
                if item.status != "done" && (status == "queued" || item.status != "failed") {
                    item.status = status.to_string();
                    if status == "queued" {
                        item.error.clear();
                    } else if status == "paused" {
                        item.speed.clear();
                        item.eta.clear();
                        if is_active_entry && cur_ep.as_deref() == Some(item.ep_id.as_str()) {
                            if cur_pct > item.percent {
                                item.percent = cur_pct;
                            }
                            if !cur_size.is_empty() {
                                item.size = cur_size.clone();
                            }
                        }
                    }
                }
            }
            entry.status = status.to_string();
            if status == "queued" {
                entry.error.clear();
            }
            is_active_entry
        };

        if status == "queued" {
            self.queue_hold.store(false, Ordering::Relaxed);
            self.queue_failures.store(0, Ordering::Relaxed);
        }
        self.queue_persist().await;

        if was_active && status == "paused" {
            {
                let mut st = self.state.lock().await;
                if st.anime.slug == active_slug {
                    st.episodes.current = None;
                    st.progress = ProgressState::default();
                }
            }
            self.job_pause_signal.store(true, Ordering::Relaxed);
            self.cancel_current_ep();
            self.pump_notify.notify_one();
            self.add_log("info", "Anime paused — starting next queued anime.")
                .await;
        } else if status == "queued" {
            self.pump_notify.notify_one();
        }
        Ok(())
    }

    /// Remove a specific episode from a queue entry.
    pub async fn queue_ep_remove(&self, queue_id: u64, ep_id: &str) -> Result<(), String> {
        let (is_running, active_slug, cur_ep) = {
            let st = self.state.lock().await;
            (
                st.running,
                st.anime.slug.clone(),
                st.episodes.current.clone(),
            )
        };

        let (is_active_ep, no_more_queued) = {
            let mut q = self.queue.lock().await;
            let pos = q
                .iter()
                .position(|e| e.id == queue_id)
                .ok_or_else(|| format!("Queue entry {} not found.", queue_id))?;
            let before = q[pos].episode_items.len();
            q[pos].episode_items.retain(|i| i.ep_id != ep_id);
            if q[pos].episode_items.len() == before {
                return Err(format!(
                    "Episode {} not found in queue entry {}.",
                    ep_id, queue_id
                ));
            }
            let is_active_entry =
                is_running && q[pos].slug == active_slug && q[pos].status == "downloading";
            let active_ep = is_active_entry && cur_ep.as_deref() == Some(ep_id);
            let has_queued = q[pos].episode_items.iter().any(|i| i.status == "queued");

            if !is_active_entry {
                if q[pos].episode_items.is_empty()
                    || q[pos]
                        .episode_items
                        .iter()
                        .all(|i| i.status == "done" || i.status == "skipped")
                {
                    q.remove(pos);
                } else if !has_queued && q[pos].episode_items.iter().all(|i| i.status != "failed") {
                    q[pos].status = "paused".to_string();
                }
            } else if active_ep
                && !has_queued
                && q[pos].episode_items.iter().any(|i| i.status == "paused")
            {
                q[pos].status = "paused".to_string();
            }

            (active_ep, !has_queued)
        };
        self.queue_persist().await;

        if is_active_ep {
            {
                let mut st = self.state.lock().await;
                if st.episodes.current.as_deref() == Some(ep_id) {
                    st.episodes.current = None;
                    st.progress = ProgressState::default();
                }
            }
            if no_more_queued {
                self.job_pause_signal.store(true, Ordering::Relaxed);
            }
            self.cancel_current_ep();
            self.pump_notify.notify_one();
            self.add_log(
                "info",
                &format!(
                    "Removed active episode {} — starting next queued episode.",
                    ep_id
                ),
            )
            .await;
        }
        Ok(())
    }

    /// Move episode up in its group's priority (swap with the previous non-done item).
    pub async fn queue_ep_move_up(&self, queue_id: u64, ep_id: &str) -> Result<(), String> {
        {
            let mut q = self.queue.lock().await;
            let entry = q
                .iter_mut()
                .find(|e| e.id == queue_id)
                .ok_or_else(|| format!("Queue entry {} not found.", queue_id))?;
            let pos = entry
                .episode_items
                .iter()
                .position(|i| i.ep_id == ep_id)
                .ok_or_else(|| {
                    format!("Episode {} not found in queue entry {}.", ep_id, queue_id)
                })?;
            if pos == 0 {
                return Err(format!("Episode {} is already at the top.", ep_id));
            }
            let mut target_pos = None;
            for i in (0..pos).rev() {
                if entry.episode_items[i].status != "done" {
                    target_pos = Some(i);
                    break;
                }
            }
            if let Some(prev) = target_pos {
                entry.episode_items.swap(prev, pos);
            } else {
                return Err(format!(
                    "Episode {} is already at the highest active priority.",
                    ep_id
                ));
            }
        }
        self.queue_persist().await;
        Ok(())
    }
}

/// What the queue pump should do on a given tick. Pure so it is unit-testable.
#[derive(Debug, Clone, PartialEq)]
enum PumpDecision {
    Idle,
    /// A manually stopped job finished while queued entries remain pending.
    HoldAfterStop,
    /// A queue-launched job was manually stopped: back to the front, hold.
    Requeue {
        id: u64,
    },
    /// A queue-launched job finished its unpaused episodes while remaining are paused:
    /// requeue without hold so the next queued anime can start immediately.
    RequeueIncomplete {
        id: u64,
    },
    /// A queue-launched job succeeded: drop it and proceed.
    DropDone {
        id: u64,
    },
    /// A queue-launched job failed: mark it (breaker handled by the caller).
    MarkFailed {
        id: u64,
    },
    /// A "downloading" mark lost its job (e.g. app restart): back to queued.
    RevertStale {
        id: u64,
    },
    /// Start the next queued entry.
    Start {
        id: u64,
    },
}

fn pump_decision(
    running: bool,
    stopped: bool,
    exit_code: Option<i32>,
    job_slug: &str,
    hold: bool,
    entries: &[QueueEntry],
) -> PumpDecision {
    if running {
        return PumpDecision::Idle;
    }
    if let Some(entry) = entries.iter().find(|e| e.status == "downloading") {
        if entry.slug == job_slug && exit_code.is_some() {
            if stopped {
                return PumpDecision::Requeue { id: entry.id };
            }
            let has_queued = entry.episode_items.iter().any(|i| i.status == "queued");
            let has_paused = entry.episode_items.iter().any(|i| i.status == "paused");
            let has_failed = entry.episode_items.iter().any(|i| i.status == "failed");
            if exit_code == Some(0) {
                if has_queued || has_paused {
                    return PumpDecision::RequeueIncomplete { id: entry.id };
                }
                if has_failed {
                    return PumpDecision::MarkFailed { id: entry.id };
                }
                return PumpDecision::DropDone { id: entry.id };
            }
            if has_queued || has_paused {
                return PumpDecision::RequeueIncomplete { id: entry.id };
            }
            return PumpDecision::MarkFailed { id: entry.id };
        }
        // Marked entry whose job is gone (restart / race): requeue it.
        return PumpDecision::RevertStale { id: entry.id };
    }
    if stopped && exit_code.is_some() && !hold && entries.iter().any(|e| e.status == "queued") {
        return PumpDecision::HoldAfterStop;
    }
    if hold {
        return PumpDecision::Idle;
    }
    match entries.iter().find(|e| {
        e.status == "queued"
            && (e.episode_items.is_empty() || e.episode_items.iter().any(|i| i.status == "queued"))
    }) {
        Some(entry) => PumpDecision::Start { id: entry.id },
        None => PumpDecision::Idle,
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

/// Parse an episode spec string (e.g. "1,2,3,5-8") into individual episode
/// number strings. Returns an empty vec when the spec is "*" (lazy population).
fn build_episode_items(episodes: &str) -> Vec<EpisodeItem> {
    let trimmed = episodes.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return Vec::new();
    }
    let mut items = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start_str, end_str)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (
                start_str.trim().parse::<u32>(),
                end_str.trim().parse::<u32>(),
            ) {
                if end >= start && end - start <= MAX_EPISODE_RANGE {
                    for n in start..=end {
                        items.push(EpisodeItem {
                            ep_id: n.to_string(),
                            label: format!("EP {:02}", n),
                            status: "queued".to_string(),
                            percent: 0.0,
                            speed: String::new(),
                            eta: String::new(),
                            size: String::new(),
                            error: String::new(),
                        });
                    }
                    continue;
                }
            }
        }
        // Single episode (possibly non-integer like "2.5")
        let ep_id = part.to_string();
        let label = if let Ok(n) = part.parse::<u32>() {
            format!("EP {:02}", n)
        } else {
            format!("EP {}", part)
        };
        items.push(EpisodeItem {
            ep_id,
            label,
            status: "queued".to_string(),
            percent: 0.0,
            speed: String::new(),
            eta: String::new(),
            size: String::new(),
            error: String::new(),
        });
    }
    items
}

fn audio_matches_pref(ep_audio: &str, audio_pref: &str) -> bool {
    let pref = audio_pref.trim().to_lowercase();
    if pref.is_empty() {
        return true;
    }
    let a = ep_audio.trim().to_lowercase();
    if pref == "eng" || pref == "dub" {
        a == "eng" || a == "dub"
    } else if pref == "jpn" || pref == "sub" {
        a == "jpn" || a == "sub" || a.is_empty()
    } else {
        a == pref
    }
}

fn build_or_cached_episode_items(episodes: &str, audio: &str, slug: &str) -> Vec<EpisodeItem> {
    let mut items = build_episode_items(episodes);
    if items.is_empty() {
        let cached = api::load_release_cache(slug);
        if !cached.is_empty() {
            let has_audio_match = !audio.trim().is_empty()
                && cached.iter().any(|e| audio_matches_pref(&e.audio, audio));
            let mut seen = std::collections::HashSet::new();
            for item in cached {
                if has_audio_match && !audio_matches_pref(&item.audio, audio) {
                    continue;
                }
                let ep_str = match &item.episode {
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::String(s) => s.clone(),
                    _ => continue,
                };
                if seen.insert(ep_str.clone()) {
                    let label = if let Ok(n) = ep_str.parse::<u32>() {
                        format!("EP {:02}", n)
                    } else {
                        format!("EP {}", ep_str)
                    };
                    items.push(EpisodeItem {
                        ep_id: ep_str,
                        label,
                        status: "queued".to_string(),
                        percent: 0.0,
                        speed: String::new(),
                        eta: String::new(),
                        size: String::new(),
                        error: String::new(),
                    });
                }
            }
        }
    }
    items
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
    app_ext_dir: PathBuf,
    storage_warning: Option<String>,
    cfg: AppConfig,
}

struct RunGuard {
    state: Arc<Mutex<JobState>>,
    pump_notify: Arc<tokio::sync::Notify>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let state = self.state.clone();
        let pump_notify = self.pump_notify.clone();
        tokio::spawn(async move {
            let mut st = state.lock().await;
            st.running = false;
            st.episodes.current = None;
            if st.finished_at.is_none() {
                st.finished_at = Some(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
            }
            drop(st);
            pump_notify.notify_one();
        });
    }
}

struct RunnerHelper {
    state: Arc<Mutex<JobState>>,
    queue: Arc<Mutex<Vec<QueueEntry>>>,
    queue_hold: Arc<AtomicBool>,
    queue_next_id: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    ep_cancel: Arc<AtomicBool>,
    job_pause: Arc<AtomicBool>,
    suppress_hold_on_stop: Arc<AtomicBool>,
    pause_notify: Arc<tokio::sync::Notify>,
    pump_notify: Arc<tokio::sync::Notify>,
    seq: Arc<AtomicU64>,
    event_tx: broadcast::Sender<String>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
}

impl RunnerHelper {
    async fn queue_persist(&self) {
        let entries = self.queue.lock().await.clone();
        let file = QueueFile {
            version: 1,
            hold: self.queue_hold.load(Ordering::Relaxed),
            next_id: self.queue_next_id.load(Ordering::Relaxed),
            entries,
        };
        if let Err(e) = save_queue_file(&queue_file_path(), &file) {
            eprintln!("[Queue] Failed to save queue.json: {}", e);
        }
    }
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

    async fn handle_ep_paused(&self, slug: &str, ep_num: &str) {
        let (cur_pct, cur_size) = {
            let mut st = self.state.lock().await;
            let pct = st.progress.percent.unwrap_or(0.0);
            let sz = st.progress.size.clone().unwrap_or_default();
            if st.episodes.current.as_deref() == Some(ep_num) {
                st.episodes.current = None;
                st.progress = ProgressState::default();
            }
            (pct, sz)
        };
        {
            let mut q = self.queue.lock().await;
            if let Some(entry) = q.iter_mut().find(|e| e.slug == slug) {
                if let Some(item) = entry.episode_items.iter_mut().find(|i| i.ep_id == ep_num) {
                    if item.status != "done" && item.status != "failed" {
                        item.status = "paused".to_string();
                        if cur_pct > item.percent {
                            item.percent = cur_pct;
                        }
                        if !cur_size.is_empty() {
                            item.size = cur_size;
                        }
                        item.speed.clear();
                        item.eta.clear();
                    }
                }
            }
        }
        self.queue_persist().await;
    }

    async fn run_job(&self, spec: JobSpec) {
        let _run_guard = RunGuard {
            state: self.state.clone(),
            pump_notify: self.pump_notify.clone(),
        };
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
            mut anime_dir,
            app_ext_dir,
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
        let mut releases_res =
            if self.stop.load(Ordering::Relaxed) || self.job_pause.load(Ordering::Relaxed) {
                Err("Cancelled".to_string())
            } else {
                api::release_with_status(&client, &actual_slug, &cfg).await
            };
        let needs_heal = match &releases_res {
            Err(e) => api::is_rotated_or_missing_error(e),
            Ok(list) => list.items.is_empty(),
        };
        if needs_heal
            && !self.stop.load(Ordering::Relaxed)
            && !self.job_pause.load(Ordering::Relaxed)
        {
            if let Some(new_slug) =
                api::resolve_active_slug(&client, &actual_slug, Some(&anime_title), &cfg).await
            {
                self.log(
                    "info",
                    &format!(
                        "Detected rotated or updated slug for '{}'. Auto-healed session to {}.",
                        anime_title, new_slug
                    ),
                )
                .await;
                actual_slug = new_slug;
                {
                    let mut st = self.state.lock().await;
                    if st.anime.slug == slug {
                        st.anime.slug = actual_slug.clone();
                    }
                }
                {
                    let mut q = self.queue.lock().await;
                    if let Some(entry) = q.iter_mut().find(|e| e.slug == slug) {
                        entry.slug = actual_slug.clone();
                    }
                }
                self.queue_persist().await;
                releases_res = api::release_with_status(&client, &actual_slug, &cfg).await;
            }
        }

        // Fetch release list
        let mut fetch_failed = false;
        let releases = match releases_res {
            Ok(list) => {
                if let Some(warning) = &list.warning {
                    self.log("warn", warning).await;
                }
                list.items
            }
            Err(e) => {
                if !self.stop.load(Ordering::Relaxed) && !self.job_pause.load(Ordering::Relaxed) {
                    if api::is_rotated_or_missing_error(&e) {
                        self.log(
                            "error",
                            &format!(
                                "No releases found for '{}' (HTTP 404). The anime may have been removed or unavailable on AnimePahe.",
                                anime_title
                            ),
                        )
                        .await;
                    } else {
                        self.log("error", &format!("Failed to fetch episodes: {}", e))
                            .await;
                    }
                    fetch_failed = true;
                }
                Vec::new()
            }
        };

        let is_wildcard = requested_episodes.iter().any(|s| s == "*");
        let mut episode_numbers = Vec::new();
        if !fetch_failed
            && !self.stop.load(Ordering::Relaxed)
            && !self.job_pause.load(Ordering::Relaxed)
        {
            if is_wildcard {
                let has_audio_match = !audio.trim().is_empty()
                    && releases
                        .iter()
                        .any(|e| audio_matches_pref(&e.audio, &audio));
                let mut seen = std::collections::HashSet::new();
                for item in &releases {
                    if has_audio_match && !audio_matches_pref(&item.audio, &audio) {
                        continue;
                    }
                    let ep_str = match &item.episode {
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::String(s) => s.clone(),
                        _ => continue,
                    };
                    if seen.insert(ep_str.clone()) {
                        episode_numbers.push(ep_str);
                    }
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
                let mut st = self.state.lock().await;
                st.episodes.requested = episode_numbers.clone();
            }

            {
                let mut q = self.queue.lock().await;
                if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                    if entry.episode_items.is_empty() {
                        entry.episode_items = episode_numbers
                            .iter()
                            .map(|ep| {
                                let label = if let Ok(n) = ep.parse::<u32>() {
                                    format!("EP {:02}", n)
                                } else {
                                    format!("EP {}", ep)
                                };
                                EpisodeItem {
                                    ep_id: ep.clone(),
                                    label,
                                    status: "queued".to_string(),
                                    percent: 0.0,
                                    speed: String::new(),
                                    eta: String::new(),
                                    size: String::new(),
                                    error: String::new(),
                                }
                            })
                            .collect();
                        entry.expanded_from_releases = true;
                    } else if is_wildcard && !entry.expanded_from_releases {
                        for ep in &episode_numbers {
                            if !entry.episode_items.iter().any(|i| &i.ep_id == ep) {
                                let label = if let Ok(n) = ep.parse::<u32>() {
                                    format!("EP {:02}", n)
                                } else {
                                    format!("EP {}", ep)
                                };
                                entry.episode_items.push(EpisodeItem {
                                    ep_id: ep.clone(),
                                    label,
                                    status: "queued".to_string(),
                                    percent: 0.0,
                                    speed: String::new(),
                                    eta: String::new(),
                                    size: String::new(),
                                    error: String::new(),
                                });
                            }
                        }
                        entry.expanded_from_releases = true;
                    }
                }
            }
            self.queue_persist().await;
        }

        let downloader = HlsDownloader::new(Some(&cfg.ua));
        let mut failures = 0;

        if !fetch_failed {
            loop {
                while self.pause.load(Ordering::Relaxed)
                    && !self.stop.load(Ordering::Relaxed)
                    && !self.job_pause.load(Ordering::Relaxed)
                {
                    self.pause_notify.notified().await;
                }

                if self.stop.load(Ordering::Relaxed) {
                    self.log("warn", "Download job cancelled by user.").await;
                    break;
                }
                if self.job_pause.load(Ordering::Relaxed) {
                    break;
                }

                // Atomically pick the next queued episode and mark it downloading.
                let next_ep = {
                    let mut q = self.queue.lock().await;
                    if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                        if let Some(item) = entry
                            .episode_items
                            .iter_mut()
                            .find(|i| i.status == "queued")
                        {
                            self.ep_cancel.store(false, Ordering::Relaxed);
                            item.status = "downloading".to_string();
                            item.error.clear();
                            Some(item.ep_id.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                let ep_num = match next_ep {
                    Some(ep) => ep,
                    None => {
                        break;
                    }
                };

                {
                    let mut st = self.state.lock().await;
                    st.episodes.current = Some(ep_num.clone());
                    st.progress = ProgressState::default();
                }

                self.log("info", &format!("Processing Episode {}...", ep_num))
                    .await;

                let target_ep = releases
                    .iter()
                    .find(|e| {
                        let ep_str = match &e.episode {
                            serde_json::Value::Number(n) => n.to_string(),
                            serde_json::Value::String(s) => s.clone(),
                            _ => String::new(),
                        };
                        ep_matches(&ep_str, &ep_num) && audio_matches_pref(&e.audio, &audio)
                    })
                    .or_else(|| {
                        releases.iter().find(|e| {
                            let ep_str = match &e.episode {
                                serde_json::Value::Number(n) => n.to_string(),
                                serde_json::Value::String(s) => s.clone(),
                                _ => String::new(),
                            };
                            ep_matches(&ep_str, &ep_num)
                        })
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
                        {
                            let mut st = self.state.lock().await;
                            st.episodes.failed.push(ep_num.clone());
                            st.episodes.current = None;
                        }
                        {
                            let mut q = self.queue.lock().await;
                            if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                                if let Some(item) =
                                    entry.episode_items.iter_mut().find(|i| i.ep_id == ep_num)
                                {
                                    item.status = "failed".to_string();
                                    item.error = "Not found in release list".to_string();
                                }
                            }
                        }
                        self.queue_persist().await;
                        continue;
                    }
                };

                // Fetch play sources
                let sources = match api::play_sources(&client, &actual_slug, ep_session, &cfg).await
                {
                    Ok(s) => s,
                    Err(e) => {
                        if self.stop.load(Ordering::Relaxed)
                            || self.job_pause.load(Ordering::Relaxed)
                        {
                            if self.job_pause.load(Ordering::Relaxed) {
                                self.handle_ep_paused(&actual_slug, &ep_num).await;
                            }
                            break;
                        }
                        if self.ep_cancel.swap(false, Ordering::Relaxed) {
                            self.handle_ep_paused(&actual_slug, &ep_num).await;
                            continue;
                        }
                        self.log(
                            "warn",
                            &format!("Episode {}: could not load play page: {}", ep_num, e),
                        )
                        .await;
                        failures += 1;
                        {
                            let mut st = self.state.lock().await;
                            st.episodes.failed.push(ep_num.clone());
                            st.episodes.current = None;
                        }
                        {
                            let mut q = self.queue.lock().await;
                            if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                                if let Some(item) =
                                    entry.episode_items.iter_mut().find(|i| i.ep_id == ep_num)
                                {
                                    item.status = "failed".to_string();
                                    item.error = e.clone();
                                }
                            }
                        }
                        self.queue_persist().await;
                        continue;
                    }
                };

                if self.stop.load(Ordering::Relaxed) || self.job_pause.load(Ordering::Relaxed) {
                    if self.job_pause.load(Ordering::Relaxed) {
                        self.handle_ep_paused(&actual_slug, &ep_num).await;
                    }
                    break;
                }
                if self.ep_cancel.swap(false, Ordering::Relaxed) {
                    self.handle_ep_paused(&actual_slug, &ep_num).await;
                    continue;
                }

                let candidate_sources = select_candidate_sources(&sources, &resolution, &audio);
                if candidate_sources.is_empty() {
                    self.log(
                        "warn",
                        &format!("Episode {}: no playable source found.", ep_num),
                    )
                    .await;
                    failures += 1;
                    {
                        let mut st = self.state.lock().await;
                        st.episodes.failed.push(ep_num.clone());
                        st.episodes.current = None;
                    }
                    {
                        let mut q = self.queue.lock().await;
                        if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                            if let Some(item) =
                                entry.episode_items.iter_mut().find(|i| i.ep_id == ep_num)
                            {
                                item.status = "failed".to_string();
                                item.error = "No playable source found".to_string();
                            }
                        }
                    }
                    self.queue_persist().await;
                    continue;
                }

                let mut resolved_stream: Option<(&PlaySource, String)> = None;
                for (idx, src) in candidate_sources.iter().enumerate() {
                    if self.stop.load(Ordering::Relaxed)
                        || self.job_pause.load(Ordering::Relaxed)
                        || self.ep_cancel.load(Ordering::Relaxed)
                    {
                        break;
                    }
                    let mut html_opt = None;
                    for attempt in 1..=2 {
                        if self.stop.load(Ordering::Relaxed)
                            || self.job_pause.load(Ordering::Relaxed)
                            || self.ep_cancel.load(Ordering::Relaxed)
                        {
                            break;
                        }
                        match client
                            .fetch(&src.url, Some("https://animepahe.pw/"), &cfg)
                            .await
                        {
                            Ok(h) => {
                                html_opt = Some(h);
                                break;
                            }
                            Err(e) => {
                                if attempt < 2 {
                                    tokio::select! {
                                        _ = tokio::time::sleep(Duration::from_millis(600)) => {}
                                        _ = self.pause_notify.notified() => {}
                                    }
                                } else if idx == candidate_sources.len() - 1 {
                                    self.log(
                                        "warn",
                                        &format!(
                                            "Episode {}: could not load stream page: {}",
                                            ep_num, e
                                        ),
                                    )
                                    .await;
                                }
                            }
                        }
                    }

                    if let Some(ref html) = html_opt {
                        if let Some(m3u8) = kwik::extract_m3u8_from_kwik(html) {
                            resolved_stream = Some((src, m3u8));
                            break;
                        }
                    }
                }

                if self.stop.load(Ordering::Relaxed) || self.job_pause.load(Ordering::Relaxed) {
                    if self.job_pause.load(Ordering::Relaxed) {
                        self.handle_ep_paused(&actual_slug, &ep_num).await;
                    }
                    break;
                }
                if self.ep_cancel.swap(false, Ordering::Relaxed) {
                    self.handle_ep_paused(&actual_slug, &ep_num).await;
                    continue;
                }

                let (chosen_source, m3u8_url) = match resolved_stream {
                    Some((s, u)) => (Some(s.clone()), u),
                    None => {
                        self.log(
                            "warn",
                            &format!("Episode {}: stream resolver did not find playlist.", ep_num),
                        )
                        .await;
                        failures += 1;
                        {
                            let mut st = self.state.lock().await;
                            st.episodes.failed.push(ep_num.clone());
                            st.episodes.current = None;
                        }
                        {
                            let mut q = self.queue.lock().await;
                            if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                                if let Some(item) =
                                    entry.episode_items.iter_mut().find(|i| i.ep_id == ep_num)
                                {
                                    item.status = "failed".to_string();
                                    item.error =
                                        "Stream resolver did not find playlist".to_string();
                                }
                            }
                        }
                        self.queue_persist().await;
                        continue;
                    }
                };

                if list_only {
                    self.log("info", &format!("Playlist: {}", m3u8_url)).await;
                    {
                        let mut st = self.state.lock().await;
                        st.playlists.push(m3u8_url);
                        st.episodes.completed.push(ep_num.clone());
                        st.episodes.current = None;
                    }
                    {
                        let mut q = self.queue.lock().await;
                        if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                            if let Some(item) =
                                entry.episode_items.iter_mut().find(|i| i.ep_id == ep_num)
                            {
                                item.status = "done".to_string();
                                item.percent = 100.0;
                            }
                        }
                    }
                    self.queue_persist().await;
                    continue;
                }

                let is_dub = chosen_source
                    .as_ref()
                    .map(|s| {
                        s.audio.eq_ignore_ascii_case("eng") || s.audio.eq_ignore_ascii_case("dub")
                    })
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
                    {
                        let mut st = self.state.lock().await;
                        st.episodes.completed.push(ep_num.clone());
                        st.episodes.current = None;
                    }
                    {
                        let mut q = self.queue.lock().await;
                        if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                            if let Some(item) =
                                entry.episode_items.iter_mut().find(|i| i.ep_id == ep_num)
                            {
                                item.status = "done".to_string();
                                item.percent = 100.0;
                            }
                        }
                    }
                    self.queue_persist().await;
                    continue;
                }

                let size_info = chosen_source
                    .as_ref()
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

                let this_ep_stop = Arc::new(AtomicBool::new(false));
                let watcher_stop = self.stop.clone();
                let watcher_ep_cancel = self.ep_cancel.clone();
                let watcher_job_pause = self.job_pause.clone();
                let stop_flag_inner = this_ep_stop.clone();
                let watcher = tokio::spawn(async move {
                    while !stop_flag_inner.load(Ordering::Relaxed) {
                        if watcher_stop.load(Ordering::Relaxed)
                            || watcher_ep_cancel.load(Ordering::Relaxed)
                            || watcher_job_pause.load(Ordering::Relaxed)
                        {
                            stop_flag_inner.store(true, Ordering::Relaxed);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                });

                let state_for_cb = self.state.clone();
                let ep_id_cb = ep_num.clone();
                let pause_for_dl = self.pause.clone();
                let pause_notify_for_dl = self.pause_notify.clone();

                let mut dl_res = downloader
                    .download_stream(DownloadConfig {
                        m3u8_url: m3u8_url.clone(),
                        referer: Some("https://kwik.cx/".to_string()),
                        dest_path: final_file.clone(),
                        replace_existing: replace,
                        concurrency: fragments,
                        stop_signal: this_ep_stop.clone(),
                        pause_signal: pause_for_dl,
                        pause_notify: pause_notify_for_dl,
                        progress_cb: move |update: ProgressUpdate| {
                            let st_clone = state_for_cb.clone();
                            let ep_id_inner = ep_id_cb.clone();
                            tokio::spawn(async move {
                                let mut st = st_clone.lock().await;
                                if st.running
                                    && st.episodes.current.as_deref() == Some(ep_id_inner.as_str())
                                {
                                    st.progress.percent = Some(update.percent);
                                    st.progress.size = Some(update.size_str);
                                    st.progress.speed = Some(update.speed_str);
                                    st.progress.eta = Some(update.eta_str);
                                    st.progress.file = Some(update.file_name);
                                }
                            });
                        },
                    })
                    .await;

                watcher.abort();

                if self.stop.load(Ordering::Relaxed) {
                    break;
                }
                if self.job_pause.load(Ordering::Relaxed) {
                    self.handle_ep_paused(&actual_slug, &ep_num).await;
                    break;
                }
                if self.ep_cancel.swap(false, Ordering::Relaxed) {
                    self.handle_ep_paused(&actual_slug, &ep_num).await;
                    self.log(
                        "info",
                        &format!("Episode {} paused — starting next queued episode.", ep_num),
                    )
                    .await;
                    continue;
                }

                if let Err(ref e) = dl_res {
                    let err_lower = e.to_lowercase();
                    if (err_lower.contains("operation not permitted")
                        || err_lower.contains("permission denied")
                        || err_lower.contains("failed to create destination folder")
                        || err_lower.contains("replacing output file failed"))
                        && anime_dir != app_ext_dir
                    {
                        self.log(
                            "warn",
                            &format!(
                                "Storage write to '{}' failed ({}). Retrying with app storage '{}'...",
                                anime_dir.display(),
                                e,
                                app_ext_dir.display()
                            ),
                        )
                        .await;
                        let fallback_file = app_ext_dir.join(&filename);
                        let state_for_retry = self.state.clone();
                        let ep_id_cb = ep_num.clone();
                        let watcher_stop = self.stop.clone();
                        let watcher_ep_cancel = self.ep_cancel.clone();
                        let watcher_job_pause = self.job_pause.clone();
                        let stop_flag_inner = this_ep_stop.clone();
                        let retry_watcher = tokio::spawn(async move {
                            while !stop_flag_inner.load(Ordering::Relaxed) {
                                if watcher_stop.load(Ordering::Relaxed)
                                    || watcher_ep_cancel.load(Ordering::Relaxed)
                                    || watcher_job_pause.load(Ordering::Relaxed)
                                {
                                    stop_flag_inner.store(true, Ordering::Relaxed);
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                        });
                        dl_res = downloader
                            .download_stream(DownloadConfig {
                                m3u8_url: m3u8_url.clone(),
                                referer: Some("https://kwik.cx/".to_string()),
                                dest_path: fallback_file.clone(),
                                replace_existing: replace,
                                concurrency: fragments,
                                stop_signal: this_ep_stop.clone(),
                                pause_signal: self.pause.clone(),
                                pause_notify: self.pause_notify.clone(),
                                progress_cb: move |update: ProgressUpdate| {
                                    let st_clone = state_for_retry.clone();
                                    let ep_id_inner = ep_id_cb.clone();
                                    tokio::spawn(async move {
                                        let mut st = st_clone.lock().await;
                                        if st.running
                                            && st.episodes.current.as_deref()
                                                == Some(ep_id_inner.as_str())
                                        {
                                            st.progress.percent = Some(update.percent);
                                            st.progress.size = Some(update.size_str);
                                            st.progress.speed = Some(update.speed_str);
                                            st.progress.eta = Some(update.eta_str);
                                            st.progress.file = Some(update.file_name);
                                        }
                                    });
                                },
                            })
                            .await;
                        retry_watcher.abort();
                        if dl_res.is_ok() {
                            anime_dir = app_ext_dir.clone();
                        }
                    } else if !self.stop.load(Ordering::Relaxed)
                        && !self.ep_cancel.load(Ordering::Relaxed)
                        && !self.job_pause.load(Ordering::Relaxed)
                    {
                        // Automatic retry with halved concurrency resuming cached fragments
                        self.log(
                            "warn",
                            &format!(
                                "Episode {} download encountered an interruption ({}). Resuming remaining fragments...",
                                ep_num, e
                            ),
                        )
                        .await;
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(1500)) => {}
                            _ = self.pause_notify.notified() => {}
                        }
                        if self.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        if self.job_pause.load(Ordering::Relaxed) {
                            self.handle_ep_paused(&actual_slug, &ep_num).await;
                            break;
                        }
                        if self.ep_cancel.swap(false, Ordering::Relaxed) {
                            self.handle_ep_paused(&actual_slug, &ep_num).await;
                            continue;
                        }
                        let retry_concurrency = (fragments / 2).max(1);
                        let state_for_retry = self.state.clone();
                        let ep_id_cb = ep_num.clone();
                        let watcher_stop = self.stop.clone();
                        let watcher_ep_cancel = self.ep_cancel.clone();
                        let watcher_job_pause = self.job_pause.clone();
                        let stop_flag_inner = this_ep_stop.clone();
                        let retry_watcher = tokio::spawn(async move {
                            while !stop_flag_inner.load(Ordering::Relaxed) {
                                if watcher_stop.load(Ordering::Relaxed)
                                    || watcher_ep_cancel.load(Ordering::Relaxed)
                                    || watcher_job_pause.load(Ordering::Relaxed)
                                {
                                    stop_flag_inner.store(true, Ordering::Relaxed);
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                        });
                        dl_res = downloader
                            .download_stream(DownloadConfig {
                                m3u8_url: m3u8_url.clone(),
                                referer: Some("https://kwik.cx/".to_string()),
                                dest_path: final_file.clone(),
                                replace_existing: false, // Resume cached fragments!
                                concurrency: retry_concurrency,
                                stop_signal: this_ep_stop.clone(),
                                pause_signal: self.pause.clone(),
                                pause_notify: self.pause_notify.clone(),
                                progress_cb: move |update: ProgressUpdate| {
                                    let st_clone = state_for_retry.clone();
                                    let ep_id_inner = ep_id_cb.clone();
                                    tokio::spawn(async move {
                                        let mut st = st_clone.lock().await;
                                        if st.running
                                            && st.episodes.current.as_deref()
                                                == Some(ep_id_inner.as_str())
                                        {
                                            st.progress.percent = Some(update.percent);
                                            st.progress.size = Some(update.size_str);
                                            st.progress.speed = Some(update.speed_str);
                                            st.progress.eta = Some(update.eta_str);
                                            st.progress.file = Some(update.file_name);
                                        }
                                    });
                                },
                            })
                            .await;
                        retry_watcher.abort();
                    }
                }

                if self.stop.load(Ordering::Relaxed) {
                    break;
                }
                if self.job_pause.load(Ordering::Relaxed) {
                    self.handle_ep_paused(&actual_slug, &ep_num).await;
                    break;
                }
                if self.ep_cancel.swap(false, Ordering::Relaxed) {
                    self.handle_ep_paused(&actual_slug, &ep_num).await;
                    continue;
                }

                match dl_res {
                    Ok(_) => {
                        self.log(
                            "info",
                            &format!("Episode {} successfully downloaded.", ep_num),
                        )
                        .await;
                        {
                            let mut st = self.state.lock().await;
                            st.episodes.completed.push(ep_num.clone());
                            st.episodes.current = None;
                        }
                        {
                            let mut q = self.queue.lock().await;
                            if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                                if let Some(item) =
                                    entry.episode_items.iter_mut().find(|i| i.ep_id == ep_num)
                                {
                                    item.status = "done".to_string();
                                    item.percent = 100.0;
                                    item.speed.clear();
                                    item.eta.clear();
                                }
                            }
                        }
                        self.queue_persist().await;
                    }
                    Err(e) => {
                        self.log(
                            "error",
                            &format!("Episode {} download failed: {}", ep_num, e),
                        )
                        .await;
                        failures += 1;
                        {
                            let mut st = self.state.lock().await;
                            st.episodes.failed.push(ep_num.clone());
                            st.episodes.current = None;
                        }
                        {
                            let mut q = self.queue.lock().await;
                            if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                                if let Some(item) =
                                    entry.episode_items.iter_mut().find(|i| i.ep_id == ep_num)
                                {
                                    item.status = "failed".to_string();
                                    item.error = e.clone();
                                    item.speed.clear();
                                    item.eta.clear();
                                }
                            }
                        }
                        self.queue_persist().await;
                    }
                }
            }
        }

        let was_stopped = self.stop.load(Ordering::Relaxed);
        let suppress_hold = self.suppress_hold_on_stop.swap(false, Ordering::Relaxed);
        let was_job_paused = self.job_pause.swap(false, Ordering::Relaxed);
        self.ep_cancel.store(false, Ordering::Relaxed);

        if was_job_paused {
            let mut changed = false;
            {
                let mut q = self.queue.lock().await;
                if let Some(entry) = q.iter_mut().find(|e| e.slug == actual_slug) {
                    if entry.status == "downloading" {
                        entry.status = "paused".to_string();
                        changed = true;
                    }
                    for item in &mut entry.episode_items {
                        if item.status == "downloading" || item.status == "queued" {
                            item.status = "paused".to_string();
                            item.speed.clear();
                            item.eta.clear();
                            changed = true;
                        }
                    }
                }
            }
            if changed {
                self.queue_persist().await;
            }
        }

        let has_remaining_queued_or_paused = {
            let q = self.queue.lock().await;
            q.iter()
                .find(|e| e.slug == actual_slug)
                .map(|e| {
                    e.episode_items
                        .iter()
                        .any(|i| i.status == "queued" || i.status == "paused")
                })
                .unwrap_or(false)
        };

        {
            let mut st = self.state.lock().await;
            st.running = false;
            st.paused = false;
            st.episodes.current = None;
            st.progress = ProgressState::default();
            if was_stopped {
                st.stopped = !suppress_hold;
                st.exit_code = Some(if suppress_hold { 0 } else { 1 });
            } else if was_job_paused {
                st.stopped = false;
                st.exit_code = Some(0);
            } else if fetch_failed
                || (failures > 0
                    && st.episodes.completed.is_empty()
                    && !has_remaining_queued_or_paused)
            {
                st.exit_code = Some(1);
            } else {
                st.exit_code = Some(0);
            }
            st.finished_at = Some(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
        }

        if !was_stopped && !was_job_paused && !fetch_failed {
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
        self.pump_notify.notify_one();
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

fn select_candidate_sources<'a>(
    sources: &'a [PlaySource],
    resolution: &str,
    audio: &str,
) -> Vec<&'a PlaySource> {
    let mut candidates: Vec<&PlaySource> = sources.iter().collect();

    let lowered = audio.trim().to_lowercase();
    let norm_audio = match lowered.as_str() {
        "dub" => "eng",
        "sub" => "jpn",
        other => other,
    };

    if !norm_audio.is_empty() {
        let audio_matches: Vec<&PlaySource> = candidates
            .iter()
            .cloned()
            .filter(|s| s.audio.eq_ignore_ascii_case(norm_audio))
            .collect();
        if !audio_matches.is_empty() {
            candidates = audio_matches;
        }
    }

    let target_res = resolution.trim();
    candidates.sort_by(|a, b| {
        let a_res_match = !target_res.is_empty() && a.resolution == target_res;
        let b_res_match = !target_res.is_empty() && b.resolution == target_res;
        if a_res_match != b_res_match {
            return b_res_match.cmp(&a_res_match);
        }

        let a_av1 = a.av1 == "1";
        let b_av1 = b.av1 == "1";
        if a_av1 != b_av1 {
            return a_av1.cmp(&b_av1);
        }

        if a.kwik != b.kwik {
            return b.kwik.cmp(&a.kwik);
        }

        let ar: u32 = a.resolution.parse().unwrap_or(0);
        let br: u32 = b.resolution.parse().unwrap_or(0);
        br.cmp(&ar)
    });

    candidates
}

#[cfg(test)]
fn select_best_source<'a>(
    sources: &'a [PlaySource],
    resolution: &str,
    audio: &str,
) -> Option<&'a PlaySource> {
    select_candidate_sources(sources, resolution, audio)
        .into_iter()
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// queue_file_path() resolves through ANIMEPAHE_GUI_DATA_DIR; tests that
    /// touch it serialize on this lock (std Mutex, not tokio, is intentional).
    static DATA_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point data_dir at a fresh temp dir. The caller keeps the returned
    /// guard alive for the whole test so parallel tests cannot remove the
    /// env var mid-test.
    fn with_data_dir(test_id: &str) -> (std::sync::MutexGuard<'static, ()>, PathBuf) {
        let guard = DATA_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("paheflow_queue_{}_{}", test_id, fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ANIMEPAHE_GUI_DATA_DIR", &dir);
        (guard, dir)
    }

    fn release_data_dir(dir: &Path) {
        std::env::remove_var("ANIMEPAHE_GUI_DATA_DIR");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Build a bare Runner without touching the real `queue.json`: the
    /// ANIMEPAHE_GUI_DATA_DIR override is taken by the caller.
    async fn runner_with(
        entries: Vec<QueueEntry>,
        running: bool,
        stopped: bool,
        exit_code: Option<i32>,
        job_slug: &str,
    ) -> Runner {
        let runner = Runner::new();
        *runner.queue.lock().await = entries;
        {
            let mut st = runner.state.lock().await;
            st.running = running;
            st.stopped = stopped;
            st.exit_code = exit_code;
            st.anime.slug = job_slug.to_string();
        }
        runner
    }

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
    fn test_ep_matches() {
        assert!(ep_matches("0", "0"));
        assert!(ep_matches("00", "0"));
        assert!(ep_matches("01", "1"));
        assert!(ep_matches("1", "01"));
        assert!(ep_matches("12.5", "12.5"));
        assert!(!ep_matches("0", ""));
        assert!(!ep_matches("", "0"));
        assert!(!ep_matches("1", "2"));
    }

    #[test]
    fn test_select_candidate_sources_prioritizes_non_av1_and_audio() {
        let s1 = PlaySource {
            url: "https://kwik.cx/e1".to_string(),
            resolution: "1080".to_string(),
            audio: "jpn".to_string(),
            av1: "1".to_string(),
            kwik: true,
            filesize: None,
        };
        let s2 = PlaySource {
            url: "https://kwik.cx/e2".to_string(),
            resolution: "1080".to_string(),
            audio: "jpn".to_string(),
            av1: "0".to_string(),
            kwik: true,
            filesize: None,
        };
        let s3 = PlaySource {
            url: "https://kwik.cx/e3".to_string(),
            resolution: "720".to_string(),
            audio: "eng".to_string(),
            av1: "0".to_string(),
            kwik: true,
            filesize: None,
        };
        let sources = vec![s1, s2, s3];
        let chosen = select_best_source(&sources, "1080", "jpn").unwrap();
        assert_eq!(chosen.url, "https://kwik.cx/e2");

        let dub_chosen = select_best_source(&sources, "720", "eng").unwrap();
        assert_eq!(dub_chosen.url, "https://kwik.cx/e3");
    }

    #[test]
    fn test_probe_dir_writable() {
        let temp = std::env::temp_dir().join(format!("paheflow_test_{}", fastrand::u32(..)));
        assert!(probe_dir_writable(&temp));
        let _ = std::fs::remove_dir_all(&temp);

        let non_writable = Path::new("/proc/sys/fs/probe_fail_test");
        assert!(!probe_dir_writable(non_writable));
    }

    /* ─────────────────── download queue ─────────────────── */

    fn queue_entry(id: u64, slug: &str, status: &str) -> QueueEntry {
        QueueEntry {
            id,
            slug: slug.to_string(),
            title: format!("Anime {}", slug),
            episodes: "*".to_string(),
            resolution: String::new(),
            audio: String::new(),
            replace: false,
            list_only: false,
            status: status.to_string(),
            error: String::new(),
            added_at: 0.0,
            episode_items: Vec::new(),
            expanded_from_releases: false,
        }
    }

    #[test]
    fn queue_entry_serializes_camel_case() {
        let entry = QueueEntry {
            list_only: true,
            added_at: 12.5,
            ..queue_entry(1, "abc", "queued")
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"listOnly\":true"), "json: {}", json);
        assert!(json.contains("\"addedAt\":12.5"), "json: {}", json);
        let back: QueueEntry = serde_json::from_str(&json).unwrap();
        assert!(back.list_only);
        // Missing error field must not break deserialization.
        let minimal: QueueEntry =
            serde_json::from_str("{\"id\":7,\"slug\":\"x\",\"title\":\"t\",\"episodes\":\"*\",\"resolution\":\"\",\"audio\":\"\",\"replace\":false,\"listOnly\":false,\"status\":\"queued\",\"addedAt\":0}").unwrap();
        assert_eq!(minimal.id, 7);
        assert!(minimal.error.is_empty());
    }

    #[test]
    fn queue_file_roundtrip_normalizes_downloading() {
        let dir = std::env::temp_dir().join(format!("paheflow_queue_test_{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.json");
        let file = QueueFile {
            version: 1,
            hold: true,
            next_id: 3,
            entries: vec![
                queue_entry(1, "a", "queued"),
                queue_entry(2, "b", "downloading"),
            ],
        };
        save_queue_file(&path, &file).expect("save queue file");

        let loaded = load_queue_file(&path);
        assert_eq!(loaded.version, 1);
        assert!(loaded.hold);
        assert_eq!(loaded.next_id, 3);
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[0].status, "queued");
        // A crash mid-download can never leave a wedged "downloading" mark.
        assert_eq!(loaded.entries[1].status, "queued");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_queue_file_tolerates_missing_and_corrupt() {
        let missing = load_queue_file(Path::new("/nonexistent/dir/queue.json"));
        assert!(missing.entries.is_empty());
        assert!(!missing.hold);

        let dir = std::env::temp_dir().join(format!("paheflow_queue_bad_{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.json");
        std::fs::write(&path, b"{not json").unwrap();
        let loaded = load_queue_file(&path);
        assert!(loaded.entries.is_empty());
        assert_eq!(loaded.version, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queue_file_rejects_path_escape_in_slug() {
        // slugs are validated by valid_slug before reaching the store, and
        // the store only ever serializes them to JSON — but a hand-edited
        // queue.json must not crash loading either.
        let dir = std::env::temp_dir().join(format!("paheflow_queue_evil_{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.json");
        let file = QueueFile {
            entries: vec![queue_entry(1, "../../etc/passwd", "queued")],
            next_id: 2,
            ..QueueFile::default()
        };
        save_queue_file(&path, &file).unwrap();
        assert_eq!(load_queue_file(&path).entries.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!valid_slug("../../etc/passwd"));
    }

    #[test]
    fn pump_starts_queued_entry_only_when_idle_and_not_held() {
        let entries = vec![queue_entry(1, "a", "queued"), queue_entry(2, "b", "queued")];
        assert_eq!(
            pump_decision(false, false, None, "", false, &entries),
            PumpDecision::Start { id: 1 }
        );
        assert_eq!(
            pump_decision(true, false, None, "a", false, &entries),
            PumpDecision::Idle
        );
        assert_eq!(
            pump_decision(false, false, None, "", true, &entries),
            PumpDecision::Idle
        );
        assert_eq!(
            pump_decision(false, false, Some(0), "", false, &[]),
            PumpDecision::Idle
        );
    }

    #[test]
    fn pump_reaps_finished_queue_job() {
        let entries = vec![
            queue_entry(1, "a", "downloading"),
            queue_entry(2, "b", "queued"),
        ];
        // Success: entry removed when all episodes are done or episode_items is empty.
        assert_eq!(
            pump_decision(false, false, Some(0), "a", false, &entries),
            PumpDecision::DropDone { id: 1 }
        );
        // Failure: entry marked failed.
        assert_eq!(
            pump_decision(false, false, Some(1), "a", false, &entries),
            PumpDecision::MarkFailed { id: 1 }
        );
        // Manual stop: entry goes back to queued.
        assert_eq!(
            pump_decision(false, true, Some(1), "a", false, &entries),
            PumpDecision::Requeue { id: 1 }
        );
    }

    #[test]
    fn pump_granular_episode_queue_decisions() {
        // 1. Requeue if finished with Some(0) but some episodes were paused
        let mut entry1 = queue_entry(1, "a", "downloading");
        entry1.episode_items = vec![
            EpisodeItem {
                ep_id: "1".to_string(),
                label: "EP 01".to_string(),
                status: "done".to_string(),
                percent: 100.0,
                speed: String::new(),
                eta: String::new(),
                size: String::new(),
                error: String::new(),
            },
            EpisodeItem {
                ep_id: "2".to_string(),
                label: "EP 02".to_string(),
                status: "paused".to_string(),
                percent: 0.0,
                speed: String::new(),
                eta: String::new(),
                size: String::new(),
                error: String::new(),
            },
        ];
        assert_eq!(
            pump_decision(false, false, Some(0), "a", false, &[entry1.clone()]),
            PumpDecision::RequeueIncomplete { id: 1 }
        );

        // 2. Skip entry if all of its remaining episodes are paused
        entry1.status = "queued".to_string();
        entry1.episode_items[0].status = "done".to_string();
        entry1.episode_items[1].status = "paused".to_string();

        let mut entry2 = queue_entry(2, "b", "queued");
        entry2.episode_items = vec![EpisodeItem {
            ep_id: "1".to_string(),
            label: "EP 01".to_string(),
            status: "queued".to_string(),
            percent: 0.0,
            speed: String::new(),
            eta: String::new(),
            size: String::new(),
            error: String::new(),
        }];
        assert_eq!(
            pump_decision(
                false,
                false,
                None,
                "",
                false,
                &[entry1.clone(), entry2.clone()]
            ),
            PumpDecision::Start { id: 2 }
        );

        // 3. Drop entry when all episodes are done
        entry1.status = "downloading".to_string();
        entry1.episode_items[1].status = "done".to_string();
        assert_eq!(
            pump_decision(false, false, Some(0), "a", false, &[entry1]),
            PumpDecision::DropDone { id: 1 }
        );
    }

    #[test]
    fn pump_holds_after_manual_stop_of_other_job() {
        let entries = vec![queue_entry(1, "a", "queued"), queue_entry(2, "b", "failed")];
        // Stopping a manual job while queued entries exist pauses the queue…
        assert_eq!(
            pump_decision(false, true, Some(0), "other", false, &entries),
            PumpDecision::HoldAfterStop
        );
        // …but only once: while already on hold there is nothing to decide.
        assert_eq!(
            pump_decision(false, true, Some(0), "other", true, &entries),
            PumpDecision::Idle
        );
        // A normally finished manual job does not pause the queue.
        assert_eq!(
            pump_decision(false, false, Some(0), "other", false, &entries),
            PumpDecision::Start { id: 1 }
        );
    }

    #[test]
    fn pump_reverts_stale_downloading_mark() {
        let entries = vec![queue_entry(1, "a", "downloading")];
        // App restarted mid-download: engine idle, job data is default.
        assert_eq!(
            pump_decision(false, false, None, "", false, &entries),
            PumpDecision::RevertStale { id: 1 }
        );
        // Entry marked, but a different job (manually started) is running.
        assert_eq!(
            pump_decision(true, false, None, "other", false, &entries),
            PumpDecision::Idle
        );
        // Entry marked, engine idle, job never ran for this slug.
        assert_eq!(
            pump_decision(false, false, Some(0), "other", false, &entries),
            PumpDecision::RevertStale { id: 1 }
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env-var guard intentionally spans the test
    async fn runner_queue_ops_work_with_persistence() {
        let (_env_guard, dir) = with_data_dir("runner");

        let runner = Runner::new();
        let (msg, updated) = runner
            .queue_add(&serde_json::json!({ "slug": "abc-1", "title": "Test Anime" }))
            .await
            .expect("queue add");
        assert!(!updated, "first add should not be an update");
        assert!(msg.contains("position 1"));

        // Duplicate slug updates the entry instead of queueing twice.
        let (msg2, updated2) = runner
            .queue_add(
                &serde_json::json!({ "slug": "abc-1", "title": "Test Anime", "episodes": "1-4" }),
            )
            .await
            .expect("queue update");
        assert!(updated2, "duplicate slug should update");
        assert!(msg2.contains("options updated"));

        let snap = runner.snapshot(false).await;
        assert_eq!(snap.queue.len(), 1);
        assert_eq!(snap.queue[0].episodes, "1-4");
        assert_eq!(snap.queue[0].status, "queued");

        // Removal + persistence round trip through a fresh Runner.
        runner.queue_remove("abc-1").await.expect("queue remove");
        let file_on_disk = load_queue_file(&queue_file_path());
        assert!(file_on_disk.entries.is_empty());
        assert!(file_on_disk.next_id >= 1);

        release_data_dir(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env-var guard intentionally spans the test
    async fn queue_tick_requeues_and_holds_on_manual_stop() {
        let (_env_guard, dir) = with_data_dir("stop");

        let runner = runner_with(
            vec![
                queue_entry(1, "abc", "downloading"),
                queue_entry(2, "b", "queued"),
            ],
            false,
            true,
            Some(1),
            "abc",
        )
        .await;
        runner.queue_tick(HttpClient::new()).await;

        let snap = runner.snapshot(false).await;
        assert_eq!(
            snap.queue[0].status, "queued",
            "stopped entry goes back to the queue"
        );
        assert!(snap.queue[0].error.is_empty());
        assert_eq!(snap.queue[1].status, "queued");
        assert!(snap.queue_hold, "manual stop must hold the queue");
        assert!(
            load_queue_file(&queue_file_path()).hold,
            "hold is persisted"
        );

        release_data_dir(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env-var guard intentionally spans the test
    async fn queue_tick_marks_failed_and_drops_on_success() {
        let (_env_guard, dir) = with_data_dir("fail");

        // Failure: entry stays visible as failed, queue not held, next entry may start.
        let runner = runner_with(
            vec![
                queue_entry(1, "abc", "downloading"),
                queue_entry(2, "b", "queued"),
            ],
            false,
            false,
            Some(1),
            "abc",
        )
        .await;
        runner.queue_tick(HttpClient::new()).await;
        let snap = runner.snapshot(false).await;
        assert_eq!(snap.queue[0].status, "failed");
        assert_eq!(snap.queue[0].error, "Download failed (exit 1)");
        assert!(!snap.queue_hold);

        // Success: the entry is reaped entirely.
        let runner = runner_with(
            vec![queue_entry(7, "abc", "downloading")],
            false,
            false,
            Some(0),
            "abc",
        )
        .await;
        runner.queue_tick(HttpClient::new()).await;
        let snap = runner.snapshot(false).await;
        assert!(snap.queue.is_empty(), "successful queue entry is removed");
        assert_eq!(load_queue_file(&queue_file_path()).entries.len(), 0);

        release_data_dir(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env-var guard intentionally spans the test
    async fn test_episode_level_queue_controls() {
        let (_env_guard, dir) = with_data_dir("ep_controls");
        let runner = Runner::new();
        runner
            .queue_add(&serde_json::json!({
                "slug": "naruto-1",
                "title": "Naruto",
                "episodes": "1,2,3,4"
            }))
            .await
            .unwrap();

        let snap = runner.snapshot(false).await;
        assert_eq!(snap.queue.len(), 1);
        let qid = snap.queue[0].id;
        assert_eq!(snap.queue[0].episode_items.len(), 4);
        assert_eq!(snap.queue[0].episode_items[0].ep_id, "1");
        assert_eq!(snap.queue[0].episode_items[0].status, "queued");

        // Pause episode 2
        runner
            .queue_ep_set_status(qid, "2", "paused")
            .await
            .unwrap();
        let snap = runner.snapshot(false).await;
        assert_eq!(snap.queue[0].episode_items[1].status, "paused");

        // Move episode 3 up
        runner.queue_ep_move_up(qid, "3").await.unwrap();
        let snap = runner.snapshot(false).await;
        assert_eq!(snap.queue[0].episode_items[1].ep_id, "3");

        // Remove episode 4
        runner.queue_ep_remove(qid, "4").await.unwrap();
        let snap = runner.snapshot(false).await;
        assert_eq!(snap.queue[0].episode_items.len(), 3);

        // Pause group
        runner.queue_group_set_status(qid, "paused").await.unwrap();
        let snap = runner.snapshot(false).await;
        assert!(snap.queue[0]
            .episode_items
            .iter()
            .all(|i| i.status == "paused"));

        // Resume group
        runner.queue_group_set_status(qid, "queued").await.unwrap();
        let snap = runner.snapshot(false).await;
        assert!(snap.queue[0]
            .episode_items
            .iter()
            .all(|i| i.status == "queued"));

        release_data_dir(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env-var guard intentionally spans the test
    async fn test_pause_active_episode_signals_cancel_and_keeps_queue_active() {
        let (_env_guard, dir) = with_data_dir("pause_active");
        let runner = Runner::new();
        runner
            .queue_add(&serde_json::json!({
                "slug": "onepiece-1",
                "title": "One Piece",
                "episodes": "1,2,3"
            }))
            .await
            .unwrap();

        let snap = runner.snapshot(false).await;
        let qid = snap.queue[0].id;

        // Simulate active download on episode 1
        {
            let mut q = runner.queue.lock().await;
            q[0].status = "downloading".to_string();
            q[0].episode_items[0].status = "downloading".to_string();
            let mut st = runner.state.lock().await;
            st.running = true;
            st.anime.slug = "onepiece-1".to_string();
            st.episodes.current = Some("1".to_string());
        }

        // Pause the active episode 1
        runner
            .queue_ep_set_status(qid, "1", "paused")
            .await
            .unwrap();

        // Check ep_cancel_signal is triggered to stop episode 1 without freezing runner
        assert!(runner.ep_cancel_signal.load(Ordering::Relaxed));
        // Global queue hold must remain FALSE so episode 2 can download
        assert!(!runner.queue_hold.load(Ordering::Relaxed));

        let snap = runner.snapshot(false).await;
        assert_eq!(snap.queue[0].episode_items[0].status, "paused");
        assert_eq!(snap.queue[0].episode_items[1].status, "queued");

        // Now test RequeueIncomplete when job completes unpaused episodes:
        let mut entry1 = snap.queue[0].clone();
        entry1.episode_items[0].status = "paused".to_string();
        entry1.episode_items[1].status = "done".to_string();
        entry1.episode_items[2].status = "done".to_string();

        let entry2 = queue_entry(2, "other-anime", "queued");

        let decision = pump_decision(
            false,
            false,
            Some(0),
            "onepiece-1",
            false,
            &[entry1, entry2],
        );
        // Must requeue without hold
        assert_eq!(decision, PumpDecision::RequeueIncomplete { id: qid });

        release_data_dir(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env-var guard intentionally spans the test
    async fn test_runner_pause_preserves_progress_and_allows_next_queued_anime() {
        let (_env_guard, dir) = with_data_dir("runner_pause_next");
        let runner = Runner::new();

        runner
            .queue_add(&serde_json::json!({
                "slug": "anime-a",
                "title": "Anime A",
                "episodes": "1,2"
            }))
            .await
            .unwrap();
        runner
            .queue_add(&serde_json::json!({
                "slug": "anime-b",
                "title": "Anime B",
                "episodes": "1"
            }))
            .await
            .unwrap();

        // Simulate Anime A actively downloading episode 1 at 47.5%
        {
            let mut q = runner.queue.lock().await;
            q[0].status = "downloading".to_string();
            q[0].episode_items[0].status = "downloading".to_string();
        }
        {
            let mut st = runner.state.lock().await;
            st.running = true;
            st.anime.slug = "anime-a".to_string();
            st.episodes.current = Some("1".to_string());
            st.progress.percent = Some(47.5);
            st.progress.size = Some("45.0 MB / 95.0 MB".to_string());
        }

        // Calling pause() must pause Anime A, preserve partial progress, not set st.paused or stop_signal,
        // and signal job_pause + ep_cancel so the active worker exits cleanly.
        assert!(runner.pause().await);
        assert!(runner.job_pause_signal.load(Ordering::Relaxed));
        assert!(runner.ep_cancel_signal.load(Ordering::Relaxed));
        assert!(!runner.pause_signal.load(Ordering::Relaxed));
        assert!(!runner.stop_signal.load(Ordering::Relaxed));
        assert!(!runner.queue_hold.load(Ordering::Relaxed));

        let snap = runner.snapshot(false).await;
        assert!(!snap.paused);
        assert_eq!(snap.queue[0].status, "paused");
        assert_eq!(snap.queue[0].episode_items[0].status, "paused");
        assert_eq!(snap.queue[0].episode_items[1].status, "paused");
        assert!((snap.queue[0].episode_items[0].percent - 47.5).abs() < 0.01);
        assert_eq!(snap.queue[0].episode_items[0].size, "45.0 MB / 95.0 MB");
        assert_eq!(snap.queue[1].status, "queued");

        // Once the worker drops st.running = false, pump_decision must immediately pick Anime B.
        let decision = pump_decision(false, false, Some(0), "anime-a", false, &snap.queue);
        assert_eq!(
            decision,
            PumpDecision::Start {
                id: snap.queue[1].id
            }
        );

        // Calling resume() when idle must re-queue Anime A and all its paused episodes.
        {
            let mut st = runner.state.lock().await;
            st.running = false;
            st.exit_code = Some(0);
        }
        assert!(runner.resume().await);
        let snap2 = runner.snapshot(false).await;
        assert_eq!(snap2.queue[0].status, "queued");
        assert_eq!(snap2.queue[0].episode_items[0].status, "queued");
        assert_eq!(snap2.queue[0].episode_items[1].status, "queued");

        release_data_dir(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env-var guard intentionally spans the test
    async fn test_queue_remove_active_does_not_hold_remaining_queue() {
        let (_env_guard, dir) = with_data_dir("remove_active");
        let runner = Runner::new();

        runner
            .queue_add(&serde_json::json!({
                "slug": "active-1",
                "title": "Active Anime",
                "episodes": "1,2"
            }))
            .await
            .unwrap();
        runner
            .queue_add(&serde_json::json!({
                "slug": "queued-2",
                "title": "Queued Anime",
                "episodes": "1"
            }))
            .await
            .unwrap();

        {
            let mut q = runner.queue.lock().await;
            q[0].status = "downloading".to_string();
        }
        {
            let mut st = runner.state.lock().await;
            st.running = true;
            st.anime.slug = "active-1".to_string();
        }

        runner.queue_remove("active-1").await.unwrap();
        assert!(runner.suppress_hold_on_stop.load(Ordering::Relaxed));
        assert!(runner.stop_signal.load(Ordering::Relaxed));

        // Simulate run_job cleanup seeing suppress_hold_on_stop = true
        let suppress = runner.suppress_hold_on_stop.swap(false, Ordering::Relaxed);
        {
            let mut st = runner.state.lock().await;
            st.running = false;
            st.stopped = !suppress;
            st.exit_code = Some(if suppress { 0 } else { 1 });
        }

        let snap = runner.snapshot(false).await;
        assert_eq!(snap.queue.len(), 1);
        assert_eq!(snap.queue[0].slug, "queued-2");
        let decision = pump_decision(
            snap.running,
            snap.stopped,
            snap.exit_code,
            &snap.anime.slug,
            snap.queue_hold,
            &snap.queue,
        );
        assert_eq!(
            decision,
            PumpDecision::Start {
                id: snap.queue[0].id
            }
        );

        release_data_dir(&dir);
    }

    #[test]
    fn pump_handles_partial_failure_vs_paused_correctly() {
        // Done + Failed + exit 0 -> MarkFailed (not RequeueIncomplete)
        let mut mixed_fail = queue_entry(1, "a", "downloading");
        mixed_fail.episode_items = vec![
            EpisodeItem {
                ep_id: "1".to_string(),
                label: "EP 01".to_string(),
                status: "done".to_string(),
                percent: 100.0,
                speed: String::new(),
                eta: String::new(),
                size: String::new(),
                error: String::new(),
            },
            EpisodeItem {
                ep_id: "2".to_string(),
                label: "EP 02".to_string(),
                status: "failed".to_string(),
                percent: 0.0,
                speed: String::new(),
                eta: String::new(),
                size: String::new(),
                error: "timeout".to_string(),
            },
        ];
        assert_eq!(
            pump_decision(false, false, Some(0), "a", false, &[mixed_fail]),
            PumpDecision::MarkFailed { id: 1 }
        );

        // Failed + Paused + exit 1 -> RequeueIncomplete (preserves paused episode)
        let mut fail_paused = queue_entry(2, "b", "downloading");
        fail_paused.episode_items = vec![
            EpisodeItem {
                ep_id: "1".to_string(),
                label: "EP 01".to_string(),
                status: "failed".to_string(),
                percent: 0.0,
                speed: String::new(),
                eta: String::new(),
                size: String::new(),
                error: "timeout".to_string(),
            },
            EpisodeItem {
                ep_id: "2".to_string(),
                label: "EP 02".to_string(),
                status: "paused".to_string(),
                percent: 30.0,
                speed: String::new(),
                eta: String::new(),
                size: String::new(),
                error: String::new(),
            },
        ];
        assert_eq!(
            pump_decision(false, false, Some(1), "b", false, &[fail_paused]),
            PumpDecision::RequeueIncomplete { id: 2 }
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env-var guard intentionally spans the test
    async fn test_queue_add_merges_active_and_paused_entries() {
        let (_env_guard, dir) = with_data_dir("merge_add");
        let runner = Runner::new();

        // Add episode 1 and mark it downloading
        runner
            .queue_add(&serde_json::json!({
                "slug": "show-1",
                "title": "Show 1",
                "episodes": "1"
            }))
            .await
            .unwrap();
        {
            let mut q = runner.queue.lock().await;
            q[0].status = "downloading".to_string();
        }

        // Adding episode 2 to actively downloading anime merges it in
        let (msg, updated) = runner
            .queue_add(&serde_json::json!({
                "slug": "show-1",
                "title": "Show 1",
                "episodes": "2"
            }))
            .await
            .unwrap();
        assert!(updated);
        assert!(msg.contains("Added/queued 1 episode"));
        let snap = runner.snapshot(false).await;
        assert_eq!(snap.queue[0].episode_items.len(), 2);
        assert_eq!(snap.queue[0].episode_items[1].ep_id, "2");
        assert_eq!(snap.queue[0].episode_items[1].status, "queued");

        // Pause episode 1 at 50%, mark entry paused, then add episode 3 -> preserves done/paused and adds 3
        {
            let mut q = runner.queue.lock().await;
            q[0].status = "paused".to_string();
            q[0].episode_items[0].status = "done".to_string();
            q[0].episode_items[0].percent = 100.0;
            q[0].episode_items[1].status = "paused".to_string();
            q[0].episode_items[1].percent = 50.0;
        }
        runner
            .queue_add(&serde_json::json!({
                "slug": "show-1",
                "title": "Show 1",
                "episodes": "2,3"
            }))
            .await
            .unwrap();
        let snap2 = runner.snapshot(false).await;
        assert_eq!(snap2.queue[0].status, "queued");
        assert_eq!(snap2.queue[0].episode_items.len(), 3);
        assert_eq!(snap2.queue[0].episode_items[0].status, "done");
        assert_eq!(snap2.queue[0].episode_items[1].status, "queued");
        assert_eq!(snap2.queue[0].episode_items[2].status, "queued");

        release_data_dir(&dir);
    }
}
