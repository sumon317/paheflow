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
    pause_notify: Arc<tokio::sync::Notify>,
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
            pause_notify: Arc::new(tokio::sync::Notify::new()),
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
        if st.running || st.exit_code.is_some() {
            let job_slug = st.anime.slug.clone();
            if let Some(entry) = queue.iter_mut().find(|e| e.slug == job_slug) {
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
                        if item.status != "paused" && item.status != "skipped" {
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

        let (queue_snapshot, queue_hold_snapshot) = {
            let mut q = self.queue.lock().await;
            if let Some(entry) = q.iter_mut().find(|e| e.slug == slug) {
                entry.status = "downloading".to_string();
                entry.error.clear();
                if entry.episode_items.is_empty() {
                    entry.episode_items = build_episode_items(&episode_spec);
                }
            } else {
                let id = self.queue_next_id.fetch_add(1, Ordering::Relaxed) + 1;
                let episode_items = build_episode_items(&episode_spec);
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
                    },
                );
            }
            (q.clone(), self.queue_hold.load(Ordering::Relaxed))
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
            queue: queue_snapshot,
            queue_hold: queue_hold_snapshot,
        };

        // Spawn background worker
        let state_arc = self.state.clone();
        let stop_arc = self.stop_signal.clone();
        let pause_arc = self.pause_signal.clone();
        let pause_notify_arc = self.pause_notify.clone();
        let runner_seq = self.seq.clone();
        let event_tx = self.event_tx.clone();
        let logs_arc = self.logs.clone();
        let queue_arc = self.queue.clone();

        tokio::spawn(async move {
            let runner_helper = RunnerHelper {
                state: state_arc,
                queue: queue_arc,
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
                return Ok((format!("'{}' is already downloading.", title), true));
            }
            q[index].title = resolved_title.clone();
            q[index].episodes = episodes.clone();
            q[index].resolution = resolution;
            q[index].audio = audio;
            q[index].replace = replace;
            q[index].list_only = list_only;
            q[index].status = "queued".to_string();
            q[index].error.clear();
            q[index].episode_items = build_episode_items(&episodes);
            drop(q);
            self.queue_hold.store(false, Ordering::Relaxed);
            self.queue_failures.store(0, Ordering::Relaxed);
            self.queue_persist().await;
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
        let episode_items = build_episode_items(&episodes);
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
        });
        drop(q);
        self.queue_hold.store(false, Ordering::Relaxed);
        self.queue_failures.store(0, Ordering::Relaxed);
        self.queue_persist().await;
        let message = format!("'{}' queued (position {}).", resolved_title, position);
        self.add_log("info", &message).await;
        Ok((message, false))
    }

    /// Remove a queue entry by slug. If that entry is actively downloading,
    /// it cancels the download and removes it from the queue cleanly.
    pub async fn queue_remove(&self, slug: &str) -> Result<String, String> {
        let snap = self.snapshot(false).await;
        if snap.running && snap.anime.slug == slug {
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
            tokio::time::sleep(Duration::from_millis(600)).await;
            self.queue_tick(client.clone()).await;
        }
    }

    /// One iteration of the queue pump.
    async fn queue_tick(&self, client: HttpClient) {
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
            PumpDecision::Idle => {}
            PumpDecision::HoldAfterStop => {
                self.queue_hold.store(true, Ordering::Relaxed);
                self.queue_persist().await;
                self.add_log(
                    "warn",
                    "Download stopped — queue paused so nothing else starts. Resume it in the Queue tab.",
                )
                .await;
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
            }
            PumpDecision::DropDone { id } => {
                let title = {
                    let mut q = self.queue.lock().await;
                    let index = q.iter().position(|e| e.id == id);
                    match index {
                        Some(i) => q.remove(i).title,
                        None => return,
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
            }
            PumpDecision::MarkFailed { id } => {
                let failures = self.queue_failures.fetch_add(1, Ordering::Relaxed) + 1;
                let title = {
                    let mut q = self.queue.lock().await;
                    let index = q.iter().position(|e| e.id == id);
                    let Some(i) = index else { return };
                    q[i].error = format!("Download failed (exit {})", snap.exit_code.unwrap_or(-1));
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
                }
            }
            PumpDecision::RevertStale { id } => {
                let mut q = self.queue.lock().await;
                if let Some(e) = q.iter_mut().find(|e| e.id == id) {
                    e.status = "queued".to_string();
                }
                drop(q);
                self.queue_persist().await;
            }
            PumpDecision::Start { id } => {
                self.queue_start_entry(id, client).await;
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

        if let Err(e) = self.start(entry.to_spec_value(), client).await {
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
        let is_active_ep = {
            let mut q = self.queue.lock().await;
            let entry = q
                .iter_mut()
                .find(|e| e.id == queue_id)
                .ok_or_else(|| format!("Queue entry {} not found.", queue_id))?;
            let item = entry
                .episode_items
                .iter_mut()
                .find(|i| i.ep_id == ep_id)
                .ok_or_else(|| {
                    format!("Episode {} not found in queue entry {}.", ep_id, queue_id)
                })?;
            item.status = status.to_string();
            entry.status == "downloading"
        };
        self.queue_persist().await;

        let cur_ep = {
            let st = self.state.lock().await;
            st.episodes.current.clone()
        };
        if is_active_ep {
            if status == "queued" {
                self.resume().await;
            } else if status == "paused" && cur_ep.as_deref() == Some(ep_id) {
                self.pause().await;
            }
        }
        Ok(())
    }

    /// Set status of all non-finished episodes in a queue entry.
    pub async fn queue_group_set_status(&self, queue_id: u64, status: &str) -> Result<(), String> {
        let is_active = {
            let mut q = self.queue.lock().await;
            let entry = q
                .iter_mut()
                .find(|e| e.id == queue_id)
                .ok_or_else(|| format!("Queue entry {} not found.", queue_id))?;
            for item in &mut entry.episode_items {
                if item.status != "done" && item.status != "failed" {
                    item.status = status.to_string();
                }
            }
            entry.status == "downloading"
        };
        self.queue_persist().await;
        if is_active {
            if status == "paused" {
                self.pause().await;
            } else if status == "queued" {
                self.resume().await;
            }
        }
        Ok(())
    }

    /// Remove a specific episode from a queue entry.
    pub async fn queue_ep_remove(&self, queue_id: u64, ep_id: &str) -> Result<(), String> {
        {
            let mut q = self.queue.lock().await;
            let entry = q
                .iter_mut()
                .find(|e| e.id == queue_id)
                .ok_or_else(|| format!("Queue entry {} not found.", queue_id))?;
            let before = entry.episode_items.len();
            entry.episode_items.retain(|i| i.ep_id != ep_id);
            if entry.episode_items.len() == before {
                return Err(format!(
                    "Episode {} not found in queue entry {}.",
                    ep_id, queue_id
                ));
            }
        }
        self.queue_persist().await;
        Ok(())
    }

    /// Move episode up in its group's priority (swap with the previous item).
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
            entry.episode_items.swap(pos - 1, pos);
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
            if exit_code == Some(0) {
                let has_incomplete = entry.episode_items.iter().any(|i| i.status != "done");
                if has_incomplete {
                    return PumpDecision::Requeue { id: entry.id };
                }
                return PumpDecision::DropDone { id: entry.id };
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
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut st = state.lock().await;
            st.running = false;
            st.episodes.current = None;
            if st.finished_at.is_none() {
                st.finished_at = Some(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
            }
        });
    }
}

struct RunnerHelper {
    state: Arc<Mutex<JobState>>,
    queue: Arc<Mutex<Vec<QueueEntry>>>,
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
        let _run_guard = RunGuard {
            state: self.state.clone(),
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
        let mut releases_res = api::release_with_status(&client, &actual_slug, &cfg).await;
        if let Err(ref e) = releases_res {
            if api::is_rotated_or_missing_error(e) {
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
            let mut seen = std::collections::HashSet::new();
            for item in &releases {
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
            // Publish the expanded list so progress totals are accurate for
            // ranges and for "*" across multi-page catalogues.
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
                }
            }
        }

        let downloader = HlsDownloader::new(Some(&cfg.ua));
        let mut failures = 0;

        loop {
            while self.pause.load(Ordering::Relaxed) && !self.stop.load(Ordering::Relaxed) {
                self.pause_notify.notified().await;
            }

            if self.stop.load(Ordering::Relaxed) {
                self.log("warn", "Download job cancelled by user.").await;
                break;
            }

            // Pick the next queued episode from the entry's episode_items.
            let next_ep = {
                let q = self.queue.lock().await;
                let entry = q.iter().find(|e| e.slug == actual_slug);
                if let Some(entry) = entry {
                    if !entry.episode_items.is_empty() {
                        entry
                            .episode_items
                            .iter()
                            .find(|i| i.status == "queued")
                            .map(|i| i.ep_id.clone())
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

            // Check if user paused or skipped this specific episode in the queue
            let (is_paused, is_already_done) = {
                let q = self.queue.lock().await;
                q.iter()
                    .find(|e| e.slug == actual_slug)
                    .and_then(|e| e.episode_items.iter().find(|i| i.ep_id == ep_num))
                    .map(|i| {
                        (
                            i.status == "paused" || i.status == "skipped",
                            i.status == "done",
                        )
                    })
                    .unwrap_or((false, false))
            };
            if is_already_done {
                continue;
            }
            if is_paused {
                self.log("info", &format!("Episode {} is paused — skipping.", ep_num))
                    .await;
                continue;
            }

            {
                let mut st = self.state.lock().await;
                st.episodes.current = Some(ep_num.clone());
            }

            self.log("info", &format!("Processing Episode {}...", ep_num))
                .await;

            let audio_pref = audio.trim().to_lowercase();
            let is_audio_match = |e: &api::EpisodeItem| -> bool {
                if audio_pref.is_empty() {
                    return true;
                }
                let a = e.audio.to_lowercase();
                if audio_pref == "eng" || audio_pref == "dub" {
                    a == "eng" || a == "dub"
                } else if audio_pref == "jpn" || audio_pref == "sub" {
                    a == "jpn" || a == "sub" || a.is_empty()
                } else {
                    a == audio_pref
                }
            };

            let target_ep = releases
                .iter()
                .find(|e| {
                    let ep_str = match &e.episode {
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::String(s) => s.clone(),
                        _ => String::new(),
                    };
                    ep_matches(&ep_str, &ep_num) && is_audio_match(e)
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

            let candidate_sources = select_candidate_sources(&sources, &resolution, &audio);
            if candidate_sources.is_empty() {
                self.log(
                    "warn",
                    &format!("Episode {}: no playable source found.", ep_num),
                )
                .await;
                failures += 1;
                continue;
            }

            let mut resolved_stream: Option<(&PlaySource, String)> = None;
            for (idx, src) in candidate_sources.iter().enumerate() {
                let mut html_opt = None;
                for attempt in 1..=2 {
                    if self.stop.load(Ordering::Relaxed) {
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
                                tokio::time::sleep(Duration::from_millis(600)).await;
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

            let (chosen_source, m3u8_url) = match resolved_stream {
                Some((s, u)) => (Some(s.clone()), u),
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
                .as_ref()
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

            let state_for_cb = self.state.clone();
            let stop_for_dl = self.stop.clone();
            let pause_for_dl = self.pause.clone();
            let pause_notify_for_dl = self.pause_notify.clone();

            let mut dl_res = downloader
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
                    dl_res = downloader
                        .download_stream(DownloadConfig {
                            m3u8_url: m3u8_url.clone(),
                            referer: Some("https://kwik.cx/".to_string()),
                            dest_path: fallback_file.clone(),
                            replace_existing: replace,
                            concurrency: fragments,
                            stop_signal: self.stop.clone(),
                            pause_signal: self.pause.clone(),
                            pause_notify: self.pause_notify.clone(),
                            progress_cb: move |update: ProgressUpdate| {
                                let st_clone = state_for_retry.clone();
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
                    if dl_res.is_ok() {
                        anime_dir = app_ext_dir.clone();
                    }
                } else if !self.stop.load(Ordering::Relaxed) {
                    // Automatic retry with halved concurrency resuming cached fragments
                    self.log(
                        "warn",
                        &format!(
                            "Episode {} download encountered an interruption ({}). Resuming remaining fragments...",
                            ep_num, e
                        ),
                    )
                    .await;
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    let retry_concurrency = (fragments / 2).max(1);
                    let state_for_retry = self.state.clone();
                    dl_res = downloader
                        .download_stream(DownloadConfig {
                            m3u8_url: m3u8_url.clone(),
                            referer: Some("https://kwik.cx/".to_string()),
                            dest_path: final_file.clone(),
                            replace_existing: false, // Resume cached fragments!
                            concurrency: retry_concurrency,
                            stop_signal: self.stop.clone(),
                            pause_signal: self.pause.clone(),
                            pause_notify: self.pause_notify.clone(),
                            progress_cb: move |update: ProgressUpdate| {
                                let st_clone = state_for_retry.clone();
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
                }
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
                            }
                        }
                    }
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
                            }
                        }
                    }
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
            PumpDecision::Requeue { id: 1 }
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
}
