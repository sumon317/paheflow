use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

pub struct ProgressUpdate {
    pub percent: f64,
    pub size_str: String,
    pub speed_str: String,
    pub eta_str: String,
    pub file_name: String,
}

/// Information about a single HLS media segment
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentInfo {
    pub url: String,
    pub seq_num: u64,
    pub key_url: Option<String>,
    pub iv: Option<[u8; 16]>,
}

#[derive(Clone, Debug)]
struct ActiveKey {
    uri: String,
    explicit_iv: Option<[u8; 16]>,
}

/// Configuration bundle for [`HlsDownloader::download_stream`].
///
/// Grouping these into a struct keeps the call-site clean and silences the
/// `clippy::too_many_arguments` lint without changing any behaviour.
pub struct DownloadConfig<F>
where
    F: Fn(ProgressUpdate) + Send + Sync + 'static,
{
    /// URL of the master or media `.m3u8` playlist.
    pub m3u8_url: String,
    /// Optional `Referer` header value sent with every segment request.
    pub referer: Option<String>,
    /// Final output path (e.g. `<anime_dir>/1.mp4`).
    pub dest_path: PathBuf,
    /// Optional stable key for the fragment cache directory (so storage fallbacks reuse cached fragments).
    pub cache_key: Option<String>,
    /// When `true`, replace existing destination file on completion.
    pub replace_existing: bool,
    /// Maximum concurrent fragment downloads (clamped to 1–64).
    pub concurrency: usize,
    /// Shared cancellation flag; set to `true` to abort mid-download.
    pub stop_signal: Arc<AtomicBool>,
    /// Shared pause flag; set to `true` to pause downloading.
    pub pause_signal: Arc<AtomicBool>,
    /// Notify handle to wake up workers when resumed or cancelled.
    pub pause_notify: Arc<tokio::sync::Notify>,
    /// Called after each fragment completes with cumulative progress metrics.
    pub progress_cb: F,
}

async fn wait_for_stop(stop: &AtomicBool, notify: Option<&tokio::sync::Notify>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match notify {
            Some(n) => {
                tokio::select! {
                    _ = n.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
            None => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn cancellable_sleep(
    dur: Duration,
    stop: &AtomicBool,
    notify: Option<&tokio::sync::Notify>,
) -> bool {
    if stop.load(Ordering::Relaxed) {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(dur) => stop.load(Ordering::Relaxed),
        _ = wait_for_stop(stop, notify) => true,
    }
}

fn format_reqwest_err(e: &reqwest::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        parts.push(s.to_string());
        source = s.source();
    }
    if parts.len() > 1 {
        format!("{} ({})", parts[0], parts[1..].join(" -> "))
    } else {
        parts[0].clone()
    }
}

pub struct HlsDownloader {
    client: reqwest::Client,
}

impl Default for HlsDownloader {
    fn default() -> Self {
        Self::new(None)
    }
}

impl HlsDownloader {
    pub fn new(user_agent: Option<&str>) -> Self {
        let ua = match user_agent {
            Some(u) if !u.trim().is_empty() => u.trim(),
            _ => "Mozilla/5.0 (X11; Linux x86_64; rv:159.0) Gecko/20100101 Firefox/159.0",
        };
        let client = reqwest::Client::builder()
            .dns_resolver(Arc::new(crate::dns::SmartDnsResolver::new()))
            .timeout(Duration::from_secs(120))
            .read_timeout(Duration::from_secs(25))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(ua)
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }

    /// Resolves an m3u8 playlist to its segment information (including AES-128 decryption keys).
    pub async fn parse_playlist(
        &self,
        m3u8_url: &str,
        referer: Option<&str>,
    ) -> Result<Vec<SegmentInfo>, String> {
        self.parse_playlist_cancellable(m3u8_url, referer, None)
            .await
    }

    async fn parse_playlist_cancellable(
        &self,
        m3u8_url: &str,
        referer: Option<&str>,
        cancel: Option<(&AtomicBool, &tokio::sync::Notify)>,
    ) -> Result<Vec<SegmentInfo>, String> {
        let mut last_err = String::new();
        let mut body = String::new();

        for attempt in 1..=4 {
            if let Some((stop, _)) = cancel {
                if stop.load(Ordering::Relaxed) {
                    return Err("Cancelled by user".to_string());
                }
            }
            let mut req = self.client.get(m3u8_url);
            if let Some(ref_str) = referer {
                req = req.header("Referer", ref_str);
            }
            req = req.header("Accept", "*/*");
            req = req.header("Accept-Language", "en-US,en;q=0.9");

            let send_res = if let Some((stop, notify)) = cancel {
                tokio::select! {
                    r = req.send() => r,
                    _ = wait_for_stop(stop, Some(notify)) => {
                        return Err("Cancelled by user".to_string());
                    }
                }
            } else {
                req.send().await
            };

            let mut retry_delay = Duration::from_millis(500 * attempt as u64);

            match send_res {
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
                            retry_delay = Duration::from_secs(ra.clamp(1, 10));
                        } else if status == reqwest::StatusCode::FORBIDDEN && attempt >= 2 {
                            return Err(last_err);
                        }
                    } else {
                        let text_res = if let Some((stop, notify)) = cancel {
                            tokio::select! {
                                t = res.text() => t,
                                _ = wait_for_stop(stop, Some(notify)) => {
                                    return Err("Cancelled by user".to_string());
                                }
                            }
                        } else {
                            res.text().await
                        };
                        match text_res {
                            Ok(b) => {
                                let lower = b.to_ascii_lowercase();
                                if !b.contains("#EXTM3U")
                                    && (lower.contains("<html") || lower.contains("<!doctype"))
                                {
                                    last_err = "m3u8 request returned HTML instead of a playlist"
                                        .to_string();
                                } else {
                                    body = b;
                                    break;
                                }
                            }
                            Err(e) => {
                                last_err =
                                    format!("Failed to read m3u8 body: {}", format_reqwest_err(&e));
                            }
                        }
                    }
                }
                Err(e) => {
                    last_err = format!("Failed to fetch m3u8: {}", format_reqwest_err(&e));
                }
            }
            if attempt < 4 {
                if let Some((stop, notify)) = cancel {
                    if cancellable_sleep(retry_delay, stop, Some(notify)).await {
                        return Err("Cancelled by user".to_string());
                    }
                } else {
                    tokio::time::sleep(retry_delay).await;
                }
            }
        }

        if body.is_empty() {
            return Err(if !last_err.is_empty() {
                last_err
            } else {
                "Empty m3u8 response".to_string()
            });
        }
        let base_url = match url::Url::parse(m3u8_url) {
            Ok(u) => u,
            Err(_) => return Err("Invalid m3u8 URL".to_string()),
        };

        // If master playlist with STREAM-INF, choose the highest-bandwidth
        // variant instead of blindly taking the first (often the lowest).
        if body.contains("#EXT-X-STREAM-INF") {
            let lines: Vec<&str> = body.lines().collect();
            let mut best: Option<(u64, u64, String)> = None;
            for i in 0..lines.len() {
                let line = lines[i].trim();
                if !line.starts_with("#EXT-X-STREAM-INF") || i + 1 >= lines.len() {
                    continue;
                }
                let next_line = lines[i + 1].trim();
                if next_line.is_empty() || next_line.starts_with('#') {
                    continue;
                }
                let attrs =
                    parse_hls_attributes(line.strip_prefix("#EXT-X-STREAM-INF:").unwrap_or(""));
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
            if let Some((_, _, best_url)) = best {
                return Box::pin(self.parse_playlist_cancellable(&best_url, referer, cancel)).await;
            }
        }

        parse_media_playlist(&body, &base_url)
    }

    /// Downloads segments concurrently with progress tracking and remuxes with ffmpeg.
    ///
    /// All options are passed via [`DownloadConfig`] to avoid an overlong argument list.
    pub async fn download_stream<F>(&self, config: DownloadConfig<F>) -> Result<(), String>
    where
        F: Fn(ProgressUpdate) + Send + Sync + 'static,
    {
        let DownloadConfig {
            m3u8_url,
            referer,
            dest_path,
            cache_key,
            replace_existing,
            concurrency,
            stop_signal,
            pause_signal,
            pause_notify,
            progress_cb,
        } = config;

        let m3u8_url = m3u8_url.as_str();
        let referer = referer.as_deref();
        let dest_path: &Path = dest_path.as_path();

        let final_mp4 = dest_path.to_path_buf();
        if final_mp4.exists() && !replace_existing {
            return Ok(());
        }

        if stop_signal.load(Ordering::Relaxed) {
            return Err("Cancelled by user".to_string());
        }

        // Store intermediate fragments and temporary assembly in the application cache.
        // On Android, this avoids writing hundreds of tiny chunks and dot/temp files
        // through the MediaProvider FUSE layer in public storage (e.g. Download/PaheFlow),
        // which triggers "Operation not permitted" (EPERM / os error 1).
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if let Some(ref key) = cache_key {
            key.hash(&mut hasher);
        } else {
            final_mp4.hash(&mut hasher);
        }
        let stream_hash = format!("{:016x}", hasher.finish());

        let cache_frag_root = crate::config::cache_dir().join("fragments");
        let temp_dir = cache_frag_root.join(&stream_hash);
        let temp_ts = cache_frag_root.join(format!("{}.ts", stream_hash));
        let target_file = cache_frag_root.join(format!("{}.part.mp4", stream_hash));

        fs::create_dir_all(&cache_frag_root).map_err(|e| {
            format!(
                "Failed to create cache fragments root {:?}: {}",
                cache_frag_root, e
            )
        })?;

        let segments = self
            .parse_playlist_cancellable(m3u8_url, referer, Some((&stop_signal, &pause_notify)))
            .await?;
        let total_segments = segments.len();

        if stop_signal.load(Ordering::Relaxed) {
            return Err("Cancelled by user".to_string());
        }

        // Pre-fetch any decryption keys required by the stream
        let mut unique_keys = HashSet::new();
        for seg in &segments {
            if let Some(ref k) = seg.key_url {
                unique_keys.insert(k.clone());
            }
        }

        let mut keys_map: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
        for key_url in unique_keys {
            if stop_signal.load(Ordering::Relaxed) {
                return Err("Cancelled by user".to_string());
            }
            let mut key_bytes = None;
            let mut last_err = String::new();
            for attempt in 1..=6 {
                if stop_signal.load(Ordering::Relaxed) {
                    return Err("Cancelled by user".to_string());
                }
                let mut req = self.client.get(&key_url);
                if let Some(r) = referer {
                    req = req.header("Referer", r);
                }
                req = req.header("Accept", "*/*");
                let send_res = tokio::select! {
                    r = req.send() => r,
                    _ = wait_for_stop(&stop_signal, Some(&pause_notify)) => {
                        return Err("Cancelled by user".to_string());
                    }
                };
                let mut retry_delay = Duration::from_millis(500 * attempt as u64);
                match send_res {
                    Ok(res) => {
                        let status = res.status();
                        if !status.is_success() {
                            last_err = format!("Key request failed with HTTP {}", status);
                            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                                retry_delay = Duration::from_secs(2);
                            } else if status == reqwest::StatusCode::NOT_FOUND
                                || status == reqwest::StatusCode::GONE
                            {
                                return Err(last_err);
                            }
                        } else {
                            let bytes_res = tokio::select! {
                                b = res.bytes() => b,
                                _ = wait_for_stop(&stop_signal, Some(&pause_notify)) => {
                                    return Err("Cancelled by user".to_string());
                                }
                            };
                            match bytes_res {
                                Ok(bytes) => {
                                    if bytes.len() != 16 {
                                        last_err = format!(
                                            "Invalid AES key length (expected 16 bytes, got {})",
                                            bytes.len()
                                        );
                                    } else {
                                        key_bytes = Some(bytes.to_vec());
                                        break;
                                    }
                                }
                                Err(e) => {
                                    last_err = format!(
                                        "Failed to read key bytes: {}",
                                        format_reqwest_err(&e)
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        last_err = format!(
                            "Failed to fetch key {}: {}",
                            key_url,
                            format_reqwest_err(&e)
                        );
                    }
                }
                if attempt < 6
                    && cancellable_sleep(retry_delay, &stop_signal, Some(&pause_notify)).await
                {
                    return Err("Cancelled by user".to_string());
                }
            }
            let bytes = key_bytes.ok_or(last_err)?;
            keys_map.insert(key_url, Arc::new(bytes));
        }

        // Preserve cached fragments across pause/resume and retries even when
        // `replace_existing` is true, as long as the stream signature matches.
        // Only wipe if the stream switched (e.g. different quality/track) or
        // an un-tagged legacy cache exists with `replace_existing`.
        let m3u8_path_sig = url::Url::parse(m3u8_url)
            .map(|u| u.path().to_string())
            .unwrap_or_else(|_| m3u8_url.to_string());
        let expected_meta = format!("{}|{}", m3u8_path_sig, total_segments);
        let meta_path = temp_dir.join(".stream_meta");
        let existing_meta = fs::read_to_string(&meta_path).ok();
        let cache_compatible = match existing_meta.as_deref() {
            Some(m) => m.trim() == expected_meta,
            None => !replace_existing,
        };
        if !cache_compatible {
            let _ = fs::remove_dir_all(&temp_dir);
            let _ = fs::remove_file(&temp_ts);
            let _ = fs::remove_file(&target_file);
        }
        fs::create_dir_all(&temp_dir)
            .map_err(|e| format!("Failed to create fragments directory {:?}: {}", temp_dir, e))?;
        let _ = fs::write(&meta_path, &expected_meta);

        // Remove partial fragment writes left behind by a previous crash.
        // Complete `.ts` fragments are intentionally reused to resume.
        if let Ok(entries) = fs::read_dir(&temp_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().ends_with(".ts.tmp") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }

        let mut preexisting_count = 0usize;
        let mut preexisting_bytes = 0u64;
        let mut missing_segments = Vec::new();
        for (idx, seg) in segments.into_iter().enumerate() {
            let seg_path = temp_dir.join(format!("{:06}.ts", idx));
            let valid_existing = fs::metadata(&seg_path)
                .map(|m| if m.len() > 0 { Some(m.len()) } else { None })
                .ok()
                .flatten();
            if let Some(len) = valid_existing {
                preexisting_count += 1;
                preexisting_bytes += len;
            } else {
                missing_segments.push((idx, seg, seg_path));
            }
        }

        let file_name = dest_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let progress_cb = Arc::new(progress_cb);

        if preexisting_count > 0 {
            let pct = (preexisting_count as f64 / total_segments as f64) * 100.0;
            let size_str = format!("{:.1} MiB", preexisting_bytes as f64 / 1_048_576.0);
            progress_cb(ProgressUpdate {
                percent: pct,
                size_str,
                speed_str: "Resuming...".to_string(),
                eta_str: "--:--".to_string(),
                file_name: file_name.clone(),
            });
        }

        let semaphore = Arc::new(Semaphore::new(concurrency.clamp(1, 64)));
        let completed_segments = Arc::new(AtomicUsize::new(preexisting_count));
        let total_bytes_acc = Arc::new(AtomicU64::new(preexisting_bytes));
        let session_bytes_acc = Arc::new(AtomicU64::new(0));
        let start_time = Instant::now();

        let mut join_set = JoinSet::new();

        for (idx, seg, seg_path) in missing_segments {
            let sem = semaphore.clone();
            let seg_url = seg.url;
            let seg_key_bytes = seg.key_url.as_ref().and_then(|k| keys_map.get(k).cloned());
            let seg_iv = seg.iv;
            let referer_str = referer.map(|s| s.to_string());
            let client = self.client.clone();
            let completed = completed_segments.clone();
            let total_acc = total_bytes_acc.clone();
            let session_acc = session_bytes_acc.clone();
            let stop = stop_signal.clone();
            let pause = pause_signal.clone();
            let p_notify = pause_notify.clone();
            let cb = progress_cb.clone();
            let fname = file_name.clone();

            join_set.spawn(async move {
                let _permit = sem.acquire().await.map_err(|e| e.to_string())?;

                while pause.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                    tokio::select! {
                        _ = p_notify.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                    }
                }

                if stop.load(Ordering::Relaxed) {
                    return Err("Cancelled by user".to_string());
                }

                // Download + decrypt segment with up to 8 retries so transient CDN
                // truncations or decryption failures recover cleanly.
                let mut payload_opt = None;
                let mut last_err = String::from("No response");
                for attempt in 1..=8 {
                    while pause.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                        tokio::select! {
                            _ = p_notify.notified() => {}
                            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                        }
                    }
                    if stop.load(Ordering::Relaxed) {
                        return Err("Cancelled by user".to_string());
                    }
                    let mut req = client.get(&seg_url);
                    if let Some(ref r) = referer_str {
                        req = req.header("Referer", r);
                    }
                    req = req.header("Accept", "*/*");

                    let send_res = tokio::select! {
                        r = req.send() => r,
                        _ = wait_for_stop(&stop, Some(&p_notify)) => {
                            return Err("Cancelled by user".to_string());
                        }
                    };

                    let mut retry_delay = Duration::from_millis(
                        (400 * (1u64 << (attempt - 1).min(4))).min(8000)
                            + fastrand::u64(50..250),
                    );

                    match send_res {
                        Ok(res) => {
                            let status = res.status();
                            if status.is_success() {
                                let bytes_res = tokio::select! {
                                    b = res.bytes() => b,
                                    _ = wait_for_stop(&stop, Some(&p_notify)) => {
                                        return Err("Cancelled by user".to_string());
                                    }
                                };
                                match bytes_res {
                                    Ok(bytes) => {
                                        if bytes.is_empty() {
                                            last_err = "Empty fragment response".to_string();
                                        } else if let (Some(ref key), Some(ref iv)) =
                                            (&seg_key_bytes, &seg_iv)
                                        {
                                            match decrypt_aes128_cbc(&bytes, key, iv) {
                                                Ok(dec) => {
                                                    payload_opt = Some(dec);
                                                    break;
                                                }
                                                Err(e) => {
                                                    last_err = format!(
                                                        "Decrypt failed (corrupt/truncated fragment): {}",
                                                        e
                                                    );
                                                }
                                            }
                                        } else {
                                            payload_opt = Some(bytes.to_vec());
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        last_err = format!(
                                            "Failed to read fragment body: {}",
                                            format_reqwest_err(&e)
                                        );
                                    }
                                }
                            } else {
                                last_err = format!("HTTP {}", status);
                                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                                    let ra = res
                                        .headers()
                                        .get(reqwest::header::RETRY_AFTER)
                                        .and_then(|v| v.to_str().ok())
                                        .and_then(|s| s.parse::<u64>().ok())
                                        .unwrap_or(2);
                                    retry_delay = Duration::from_secs(ra.clamp(1, 10));
                                } else if (status == reqwest::StatusCode::FORBIDDEN
                                    || status == reqwest::StatusCode::NOT_FOUND
                                    || status == reqwest::StatusCode::GONE)
                                    && attempt >= 2
                                {
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            last_err = format!("Connection error: {}", format_reqwest_err(&e));
                        }
                    }

                    if attempt < 8
                        && cancellable_sleep(retry_delay, &stop, Some(&p_notify)).await
                    {
                        return Err("Cancelled by user".to_string());
                    }
                }

                let payload = payload_opt.ok_or_else(|| {
                    format!("Fragment {} failed after retries: {}", idx, last_err)
                })?;

                let byte_len = payload.len() as u64;
                let tmp_seg = seg_path.with_extension("ts.tmp");
                fs::write(&tmp_seg, &payload)
                    .map_err(|e| format!("Writing fragment {} failed: {}", idx, e))?;
                if fs::rename(&tmp_seg, &seg_path).is_err() {
                    if let Err(e) = fs::copy(&tmp_seg, &seg_path) {
                        let _ = fs::remove_file(&tmp_seg);
                        return Err(format!("Saving fragment {} failed: {}", idx, e));
                    }
                    let _ = fs::remove_file(&tmp_seg);
                }

                let total_bytes = total_acc.fetch_add(byte_len, Ordering::Relaxed) + byte_len;
                let session_bytes = session_acc.fetch_add(byte_len, Ordering::Relaxed) + byte_len;
                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;

                // Update progress metrics using session bytes for accurate speed/ETA
                let elapsed_secs = start_time.elapsed().as_secs_f64().max(0.1);
                let speed_bps = session_bytes as f64 / elapsed_secs;
                let pct = (done as f64 / total_segments as f64) * 100.0;

                let avg_seg_bytes = if done > 0 {
                    total_bytes as f64 / done as f64
                } else {
                    0.0
                };
                let rem_segs = total_segments.saturating_sub(done) as f64;
                let rem_bytes = (avg_seg_bytes * rem_segs).max(0.0);
                let eta_secs = if speed_bps > 0.0 {
                    rem_bytes / speed_bps
                } else {
                    0.0
                };

                let eta_str = if eta_secs > 0.0 && eta_secs < 86400.0 {
                    let mins = (eta_secs as u64) / 60;
                    let secs = (eta_secs as u64) % 60;
                    format!("{:02}:{:02}", mins, secs)
                } else {
                    "--:--".to_string()
                };

                let speed_str = if speed_bps > 1_048_576.0 {
                    format!("{:.2} MiB/s", speed_bps / 1_048_576.0)
                } else {
                    format!("{:.1} KiB/s", speed_bps / 1024.0)
                };

                let size_str = format!("{:.1} MiB", total_bytes as f64 / 1_048_576.0);

                cb(ProgressUpdate {
                    percent: pct,
                    size_str,
                    speed_str,
                    eta_str,
                    file_name: fname,
                });

                Ok::<(), String>(())
            });
        }

        // Wait for all remaining fragments; abort all tasks immediately on
        // stop_signal or first permanent failure.
        loop {
            if stop_signal.load(Ordering::Relaxed) {
                join_set.abort_all();
                while join_set.join_next().await.is_some() {}
                return Err("Cancelled by user".to_string());
            }
            tokio::select! {
                res = join_set.join_next() => {
                    match res {
                        Some(Ok(Ok(()))) => {}
                        Some(Ok(Err(e))) => {
                            join_set.abort_all();
                            while join_set.join_next().await.is_some() {}
                            return Err(e);
                        }
                        Some(Err(e)) => {
                            join_set.abort_all();
                            while join_set.join_next().await.is_some() {}
                            return Err(format!("Task error: {}", e));
                        }
                        None => break,
                    }
                }
                _ = wait_for_stop(&stop_signal, Some(&pause_notify)) => {
                    join_set.abort_all();
                    while join_set.join_next().await.is_some() {}
                    return Err("Cancelled by user".to_string());
                }
            }
        }

        while pause_signal.load(Ordering::Relaxed) && !stop_signal.load(Ordering::Relaxed) {
            tokio::select! {
                _ = pause_notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }

        if stop_signal.load(Ordering::Relaxed) {
            return Err("Cancelled by user".to_string());
        }

        // Assemble fragments into temp_ts off the async runtime (preserving temp_dir if cancelled mid-assembly)
        {
            let temp_ts_blk = temp_ts.clone();
            let temp_dir_blk = temp_dir.clone();
            let stop_blk = stop_signal.clone();
            tokio::task::spawn_blocking(move || {
                assemble_segments(&temp_dir_blk, &temp_ts_blk, total_segments, &stop_blk)
            })
            .await
            .map_err(|e| format!("Stream assembly task failed: {}", e))??;
        }

        if stop_signal.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&temp_ts);
            return Err("Cancelled by user".to_string());
        }

        // Remux with ffmpeg into target_file, or fall back to stream copy if ffmpeg is unavailable.
        // `.kill_on_drop(true)` ensures cancelling mid-remux terminates ffmpeg immediately.
        let ffmpeg_bin =
            std::env::var("ANIMEPAHE_FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string());
        let mut ffmpeg = Command::new(&ffmpeg_bin);
        ffmpeg
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .arg("-y")
            .arg("-i")
            .arg(&temp_ts)
            .arg("-c")
            .arg("copy")
            .arg("-bsf:a")
            .arg("aac_adtstoasc")
            .arg("-movflags")
            .arg("+faststart")
            .arg("-f")
            .arg("mp4")
            .arg(&target_file);

        let remux_success = match ffmpeg.spawn() {
            Ok(child) => {
                let wait_fut = child.wait_with_output();
                tokio::select! {
                    out = wait_fut => match out {
                        Ok(output) if output.status.success() => true,
                        Ok(output) => {
                            let err = String::from_utf8_lossy(&output.stderr);
                            eprintln!(
                                "[HLS] ffmpeg exited with error: {}, falling back to assembled stream",
                                err
                            );
                            false
                        }
                        Err(e) => {
                            eprintln!("[HLS] ffmpeg wait error ({}), falling back", e);
                            false
                        }
                    },
                    _ = wait_for_stop(&stop_signal, Some(&pause_notify)) => {
                        let _ = fs::remove_file(&temp_ts);
                        let _ = fs::remove_file(&target_file);
                        return Err("Cancelled by user".to_string());
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[HLS] ffmpeg not available ({}), falling back to assembled stream",
                    e
                );
                false
            }
        };

        if stop_signal.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&temp_ts);
            let _ = fs::remove_file(&target_file);
            return Err("Cancelled by user".to_string());
        }

        if remux_success {
            let _ = fs::remove_file(&temp_ts);
        } else {
            // Fallback: use the assembled MPEG transport stream directly
            let src_blk = temp_ts.clone();
            let dst_blk = target_file.clone();
            let stop_blk = stop_signal.clone();
            tokio::task::spawn_blocking(move || {
                safe_replace_file_cancellable(&src_blk, &dst_blk, &stop_blk)
            })
            .await
            .map_err(|e| format!("Stream assembly fallback task failed: {}", e))?
            .map_err(|e| format!("Stream assembly fallback failed: {}", e))?;
        }

        // Ensure final destination directory exists before swapping
        if let Some(parent) = final_mp4.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create destination folder {:?}: {}", parent, e))?;
        }

        // Only swap in the finished file after stream assembly succeeds. A failed or
        // interrupted attempt therefore cannot masquerade as a completed
        // episode on the next run. Keep temp_dir until final move succeeds so
        // storage fallbacks can reuse cached fragments without re-downloading.
        {
            let src_blk = target_file.clone();
            let dst_blk = final_mp4.clone();
            let stop_blk = stop_signal.clone();
            tokio::task::spawn_blocking(move || {
                safe_replace_file_cancellable(&src_blk, &dst_blk, &stop_blk)
            })
            .await
            .map_err(|e| format!("Replacing output file task failed: {}", e))?
            .map_err(|e| format!("Replacing output file failed: {}", e))?;
        }

        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::remove_file(&temp_ts);
        let _ = fs::remove_file(&target_file);

        Ok(())
    }
}

fn assemble_segments(
    temp_dir: &Path,
    temp_ts: &Path,
    total_segments: usize,
    stop_signal: &AtomicBool,
) -> Result<(), String> {
    if temp_ts.exists() {
        let _ = fs::remove_file(temp_ts);
    }
    let out = File::create(temp_ts).map_err(|e| {
        format!(
            "Failed to create temporary stream file {:?}: {}",
            temp_ts, e
        )
    })?;
    let mut writer = BufWriter::with_capacity(128 * 1024, out);
    let mut buf = [0u8; 64 * 1024];

    for idx in 0..total_segments {
        if stop_signal.load(Ordering::Relaxed) {
            drop(writer);
            let _ = fs::remove_file(temp_ts);
            return Err("Cancelled by user".to_string());
        }
        let seg_path = temp_dir.join(format!("{:06}.ts", idx));
        let mut seg_file =
            File::open(&seg_path).map_err(|e| format!("Missing fragment {:06}.ts: {}", idx, e))?;
        loop {
            if stop_signal.load(Ordering::Relaxed) {
                drop(writer);
                let _ = fs::remove_file(temp_ts);
                return Err("Cancelled by user".to_string());
            }
            let n = seg_file
                .read(&mut buf)
                .map_err(|e| format!("Failed reading fragment {:06}.ts: {}", idx, e))?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .map_err(|e| format!("Failed writing assembled stream to {:?}: {}", temp_ts, e))?;
        }
    }

    writer
        .flush()
        .map_err(|e| format!("Failed flushing assembled stream to {:?}: {}", temp_ts, e))?;
    Ok(())
}

fn safe_replace_file_cancellable(
    src: &Path,
    dst: &Path,
    stop_signal: &AtomicBool,
) -> Result<(), std::io::Error> {
    if stop_signal.load(Ordering::Relaxed) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "Cancelled by user",
        ));
    }

    // Fast path: same filesystem atomic rename (overwrites dst atomically on POSIX)
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }

    // Cross-filesystem path: write completely to a temporary sibling in dst's folder
    // BEFORE touching any existing dst, so failures/cancellations never destroy dst.
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    let tmp_name = format!(
        ".paheflow_tmp_{}_{}",
        dst.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        fastrand::u64(..)
    );
    let tmp_dst = parent.join(tmp_name);

    let copy_res = (|| -> Result<(), std::io::Error> {
        let mut reader = File::open(src)?;
        let mut writer = File::create(&tmp_dst)?;
        let mut buf = [0u8; 128 * 1024];
        loop {
            if stop_signal.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Cancelled by user",
                ));
            }
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
        }
        writer.sync_all()?;
        Ok(())
    })();

    if let Err(e) = copy_res {
        let _ = fs::remove_file(&tmp_dst);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp_dst, dst) {
        if dst.exists() {
            let _ = fs::remove_file(dst);
            if let Err(e2) = fs::rename(&tmp_dst, dst) {
                let _ = fs::remove_file(&tmp_dst);
                return Err(e2);
            }
        } else {
            let _ = fs::remove_file(&tmp_dst);
            return Err(e);
        }
    }

    let _ = fs::remove_file(src);
    Ok(())
}

/// Decrypts an AES-128-CBC PKCS7-padded segment buffer.
#[allow(clippy::manual_is_multiple_of)] // Keep compatibility with older Rust toolchains.
pub fn decrypt_aes128_cbc(data: &[u8], key: &[u8], iv: &[u8; 16]) -> Result<Vec<u8>, String> {
    use aes::Aes128;
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    if key.len() != 16 {
        return Err("AES-128 key must be exactly 16 bytes".to_string());
    }
    if data.is_empty() || data.len() % 16 != 0 {
        return Err(format!(
            "Encrypted data length ({}) is not a multiple of 16",
            data.len()
        ));
    }

    let dec = cbc::Decryptor::<Aes128>::new_from_slices(key, iv)
        .map_err(|e| format!("Cipher init error: {}", e))?;
    let mut buf = data.to_vec();
    let pt = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| format!("AES decryption/unpadding error: {}", e))?;
    Ok(pt.to_vec())
}

/// Parses key/value attributes from an HLS tag (e.g. #EXT-X-KEY).
pub fn parse_hls_attributes(input: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut chars = input.chars().peekable();
    while chars.peek().is_some() {
        while let Some(&c) = chars.peek() {
            if c == ',' || c.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c == ',' {
                break;
            }
            key.push(c);
            chars.next();
        }
        let key = key.trim().to_uppercase();
        if chars.peek() == Some(&'=') {
            chars.next(); // consume '='
            let mut val = String::new();
            if chars.peek() == Some(&'"') {
                chars.next(); // consume opening quote
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    val.push(c);
                }
            } else {
                while let Some(&c) = chars.peek() {
                    if c == ',' {
                        break;
                    }
                    val.push(c);
                    chars.next();
                }
            }
            result.push((key, val.trim().to_string()));
        }
    }
    result
}

/// Parses a 128-bit hex string (with optional 0x prefix) into a 16-byte array.
pub fn parse_hex_iv(hex_str: &str) -> Option<[u8; 16]> {
    let hex_str = hex_str
        .strip_prefix("0x")
        .or_else(|| hex_str.strip_prefix("0X"))
        .unwrap_or(hex_str);
    if hex_str.is_empty() || hex_str.len() > 32 {
        return None;
    }
    let padded = format!("{:0>32}", hex_str);
    let mut iv = [0u8; 16];
    for i in 0..16 {
        iv[i] = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(iv)
}

/// Parses an HLS media playlist body into individual [`SegmentInfo`] items.
pub fn parse_media_playlist(body: &str, base_url: &url::Url) -> Result<Vec<SegmentInfo>, String> {
    let mut segments = Vec::new();
    let mut current_seq: u64 = 0;
    let mut active_key: Option<ActiveKey> = None;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            if let Ok(seq) = rest.trim().parse::<u64>() {
                current_seq = seq;
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("#EXT-X-KEY:") {
            let attrs = parse_hls_attributes(rest);
            let mut method = String::new();
            let mut uri = None;
            let mut explicit_iv = None;

            for (k, v) in attrs {
                match k.as_str() {
                    "METHOD" => method = v.to_uppercase(),
                    "URI" => uri = Some(v),
                    "IV" => explicit_iv = parse_hex_iv(&v),
                    _ => {}
                }
            }

            if method == "NONE" || method.is_empty() {
                active_key = None;
            } else if method == "AES-128" {
                if let Some(u) = uri {
                    let resolved = match base_url.join(&u) {
                        Ok(r) => r.to_string(),
                        Err(_) => u,
                    };
                    active_key = Some(ActiveKey {
                        uri: resolved,
                        explicit_iv,
                    });
                }
            }
            continue;
        }

        if trimmed.starts_with('#') {
            continue;
        }

        // It's a segment URI
        let seg_url = match base_url.join(trimmed) {
            Ok(u) => u.to_string(),
            Err(_) => trimmed.to_string(),
        };

        let (key_url, iv) = match &active_key {
            Some(ak) => {
                let iv = ak
                    .explicit_iv
                    .unwrap_or_else(|| (current_seq as u128).to_be_bytes());
                (Some(ak.uri.clone()), Some(iv))
            }
            None => (None, None),
        };

        segments.push(SegmentInfo {
            url: seg_url,
            seq_num: current_seq,
            key_url,
            iv,
        });

        current_seq += 1;
    }

    if segments.is_empty() {
        return Err("No media segments found in playlist".to_string());
    }

    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::Aes128;
    use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};

    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    #[test]
    fn test_parse_attributes() {
        let attrs =
            parse_hls_attributes(r#"METHOD=AES-128,URI="https://example.com/key.bin",IV=0x1234"#);
        assert_eq!(attrs.len(), 3);
        assert_eq!(attrs[0], ("METHOD".to_string(), "AES-128".to_string()));
        assert_eq!(
            attrs[1],
            ("URI".to_string(), "https://example.com/key.bin".to_string())
        );
        assert_eq!(attrs[2], ("IV".to_string(), "0x1234".to_string()));
    }

    #[test]
    fn test_parse_hex_iv() {
        let iv = parse_hex_iv("0x1").unwrap();
        assert_eq!(iv, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

        let iv2 = parse_hex_iv("0x000102030405060708090a0b0c0d0e0f").unwrap();
        assert_eq!(iv2, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn test_parse_media_playlist_aes_128() {
        let playlist = r#"
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-MEDIA-SEQUENCE:5
#EXT-X-KEY:METHOD=AES-128,URI="mon.key"
#EXTINF:4.0,
seg1.ts
#EXTINF:4.0,
seg2.ts
"#;
        let base = url::Url::parse("https://cdn.example.com/streams/uwu.m3u8").unwrap();
        let segments = parse_media_playlist(playlist, &base).unwrap();
        assert_eq!(segments.len(), 2);

        assert_eq!(segments[0].url, "https://cdn.example.com/streams/seg1.ts");
        assert_eq!(segments[0].seq_num, 5);
        assert_eq!(
            segments[0].key_url.as_deref(),
            Some("https://cdn.example.com/streams/mon.key")
        );
        assert_eq!(segments[0].iv, Some((5u128).to_be_bytes()));

        assert_eq!(segments[1].url, "https://cdn.example.com/streams/seg2.ts");
        assert_eq!(segments[1].seq_num, 6);
        assert_eq!(
            segments[1].key_url.as_deref(),
            Some("https://cdn.example.com/streams/mon.key")
        );
        assert_eq!(segments[1].iv, Some((6u128).to_be_bytes()));
    }

    #[test]
    fn test_cbc_roundtrip() {
        let key = [0x42u8; 16];
        let iv = [0x11u8; 16];
        let plaintext = b"Hello, MPEG-TS encrypted segment data test!";

        // Encrypt using Aes128CbcEnc
        let enc = Aes128CbcEnc::new((&key).into(), (&iv).into());
        let mut buf = [0u8; 64];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        let ct = enc
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .unwrap();

        // Decrypt using our function
        let decrypted = decrypt_aes128_cbc(ct, &key, &iv).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn test_download_encrypted_stream_local_server() {
        use axum::{routing::get, Router};
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};

        let key = [0x77u8; 16];
        let iv1 = (1u128).to_be_bytes();
        let iv2 = (2u128).to_be_bytes();

        let pt1 = [0x47u8; 188]; // MPEG-TS packet
        let pt2 = [0x47u8; 188];

        let mut buf1 = [0u8; 256];
        buf1[..pt1.len()].copy_from_slice(&pt1);
        let enc1 = Aes128CbcEnc::new((&key).into(), (&iv1).into());
        let ct1 = enc1
            .encrypt_padded_mut::<Pkcs7>(&mut buf1, pt1.len())
            .unwrap()
            .to_vec();

        let mut buf2 = [0u8; 256];
        buf2[..pt2.len()].copy_from_slice(&pt2);
        let enc2 = Aes128CbcEnc::new((&key).into(), (&iv2).into());
        let ct2 = enc2
            .encrypt_padded_mut::<Pkcs7>(&mut buf2, pt2.len())
            .unwrap()
            .to_vec();

        let key_bytes = key.to_vec();
        let app = Router::new()
            .route(
                "/playlist.m3u8",
                get(|| async {
                    r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-MEDIA-SEQUENCE:1
#EXT-X-KEY:METHOD=AES-128,URI="key.bin"
#EXTINF:2.0,
seg1.ts
#EXTINF:2.0,
seg2.ts
#EXT-X-ENDLIST
"#
                }),
            )
            .route("/key.bin", get(move || async move { key_bytes }))
            .route("/seg1.ts", get(move || async move { ct1 }))
            .route("/seg2.ts", get(move || async move { ct2 }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let downloader = HlsDownloader::new(None);
        let stop_signal = Arc::new(AtomicBool::new(false));
        let pause_signal = Arc::new(AtomicBool::new(false));
        let pause_notify = Arc::new(tokio::sync::Notify::new());

        let dest = std::env::temp_dir().join(format!("paheflow_test_{}.mp4", fastrand::u64(..)));

        let m3u8_url = format!("http://127.0.0.1:{}/playlist.m3u8", port);
        let res = downloader
            .download_stream(DownloadConfig {
                m3u8_url,
                referer: None,
                dest_path: dest.clone(),
                cache_key: None,
                replace_existing: true,
                concurrency: 4,
                stop_signal,
                pause_signal,
                pause_notify,
                progress_cb: |_| {},
            })
            .await;

        assert!(res.is_ok(), "Download failed: {:?}", res);
        assert!(dest.exists(), "Output file was not created");
        assert!(dest.metadata().unwrap().len() > 0, "Output file is empty");
        let _ = fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn test_pause_resume_preserves_fragments_even_with_replace_existing() {
        use axum::{routing::get, Router};
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};

        let key = [0x55u8; 16];
        let iv1 = (1u128).to_be_bytes();
        let iv2 = (2u128).to_be_bytes();
        let pt = [0x47u8; 188];

        let mut buf1 = [0u8; 256];
        buf1[..pt.len()].copy_from_slice(&pt);
        let ct1 = Aes128CbcEnc::new((&key).into(), (&iv1).into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf1, pt.len())
            .unwrap()
            .to_vec();

        let mut buf2 = [0u8; 256];
        buf2[..pt.len()].copy_from_slice(&pt);
        let ct2 = Aes128CbcEnc::new((&key).into(), (&iv2).into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf2, pt.len())
            .unwrap()
            .to_vec();

        let seg1_hits = Arc::new(AtomicUsize::new(0));
        let seg2_hits = Arc::new(AtomicUsize::new(0));
        let stop_signal = Arc::new(AtomicBool::new(false));
        let pause_notify = Arc::new(tokio::sync::Notify::new());

        let seg1_hits_srv = seg1_hits.clone();
        let seg2_hits_srv = seg2_hits.clone();
        let stop_on_seg2 = stop_signal.clone();
        let notify_on_seg2 = pause_notify.clone();
        let key_bytes = key.to_vec();

        let app = Router::new()
            .route(
                "/stream/test-pause/uwu.m3u8",
                get(|| async {
                    "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:2.0,\nseg1.ts\n#EXTINF:2.0,\nseg2.ts\n#EXT-X-ENDLIST\n"
                }),
            )
            .route("/stream/test-pause/key.bin", get(move || async move { key_bytes }))
            .route(
                "/stream/test-pause/seg1.ts",
                get(move || async move {
                    seg1_hits_srv.fetch_add(1, Ordering::Relaxed);
                    ct1
                }),
            )
            .route(
                "/stream/test-pause/seg2.ts",
                get(move || async move {
                    let hit = seg2_hits_srv.fetch_add(1, Ordering::Relaxed) + 1;
                    if hit == 1 {
                        // Simulate user pressing pause while seg2 is downloading
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        stop_on_seg2.store(true, Ordering::Relaxed);
                        notify_on_seg2.notify_waiters();
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    ct2
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let downloader = HlsDownloader::new(None);
        let pause_signal = Arc::new(AtomicBool::new(false));
        let dest = std::env::temp_dir().join(format!("paheflow_resume_{}.mp4", fastrand::u64(..)));
        let cache_key = format!("test-pause-cache-{}", fastrand::u64(..));
        let m3u8_url = format!("http://127.0.0.1:{}/stream/test-pause/uwu.m3u8", port);

        // First attempt with concurrency=1 downloads seg1, then pauses mid-seg2
        let res1 = downloader
            .download_stream(DownloadConfig {
                m3u8_url: m3u8_url.clone(),
                referer: None,
                dest_path: dest.clone(),
                cache_key: Some(cache_key.clone()),
                replace_existing: true,
                concurrency: 1,
                stop_signal: stop_signal.clone(),
                pause_signal: pause_signal.clone(),
                pause_notify: pause_notify.clone(),
                progress_cb: |_| {},
            })
            .await;
        assert!(res1.is_err());
        assert_eq!(seg1_hits.load(Ordering::Relaxed), 1);

        // Resume with replace_existing STILL true: seg1 MUST NOT be re-downloaded
        stop_signal.store(false, Ordering::Relaxed);
        let res2 = downloader
            .download_stream(DownloadConfig {
                m3u8_url,
                referer: None,
                dest_path: dest.clone(),
                cache_key: Some(cache_key),
                replace_existing: true,
                concurrency: 1,
                stop_signal,
                pause_signal,
                pause_notify,
                progress_cb: |_| {},
            })
            .await;
        assert!(res2.is_ok(), "Resume failed: {:?}", res2);
        assert_eq!(
            seg1_hits.load(Ordering::Relaxed),
            1,
            "Cached seg1.ts must be reused on resume even with replace_existing=true"
        );
        assert!(dest.exists());
        let _ = fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn test_stop_signal_aborts_slow_playlist_fetch_immediately() {
        use axum::{routing::get, Router};

        let app = Router::new().route(
            "/slow.m3u8",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                "#EXTM3U\n"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let downloader = HlsDownloader::new(None);
        let stop_signal = Arc::new(AtomicBool::new(false));
        let pause_signal = Arc::new(AtomicBool::new(false));
        let pause_notify = Arc::new(tokio::sync::Notify::new());
        let dest = std::env::temp_dir().join(format!("paheflow_abort_{}.mp4", fastrand::u64(..)));

        let stop_trig = stop_signal.clone();
        let notify_trig = pause_notify.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            stop_trig.store(true, Ordering::Relaxed);
            notify_trig.notify_waiters();
        });

        let started = Instant::now();
        let res = downloader
            .download_stream(DownloadConfig {
                m3u8_url: format!("http://127.0.0.1:{}/slow.m3u8", port),
                referer: None,
                dest_path: dest,
                cache_key: None,
                replace_existing: false,
                concurrency: 2,
                stop_signal,
                pause_signal,
                pause_notify,
                progress_cb: |_| {},
            })
            .await;

        assert!(res.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "Slow playlist fetch took {:?} instead of cancelling immediately",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn test_corrupt_encrypted_segment_retries_and_succeeds() {
        use axum::{routing::get, Router};
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};

        let key = [0x33u8; 16];
        let iv1 = (1u128).to_be_bytes();
        let pt = [0x47u8; 188];
        let mut buf = [0u8; 256];
        buf[..pt.len()].copy_from_slice(&pt);
        let ct1 = Aes128CbcEnc::new((&key).into(), (&iv1).into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, pt.len())
            .unwrap()
            .to_vec();

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = hits.clone();
        let key_bytes = key.to_vec();

        let app = Router::new()
            .route(
                "/playlist.m3u8",
                get(|| async {
                    "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:2.0,\nseg1.ts\n#EXT-X-ENDLIST\n"
                }),
            )
            .route("/key.bin", get(move || async move { key_bytes }))
            .route(
                "/seg1.ts",
                get(move || async move {
                    let n = hits_srv.fetch_add(1, Ordering::Relaxed);
                    if n == 0 {
                        // Return invalid non-multiple-of-16 truncated garbage on first try
                        vec![0x01, 0x02, 0x03]
                    } else {
                        ct1
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let downloader = HlsDownloader::new(None);
        let dest = std::env::temp_dir().join(format!("paheflow_corrupt_{}.mp4", fastrand::u64(..)));
        let res = downloader
            .download_stream(DownloadConfig {
                m3u8_url: format!("http://127.0.0.1:{}/playlist.m3u8", port),
                referer: None,
                dest_path: dest.clone(),
                cache_key: None,
                replace_existing: true,
                concurrency: 2,
                stop_signal: Arc::new(AtomicBool::new(false)),
                pause_signal: Arc::new(AtomicBool::new(false)),
                pause_notify: Arc::new(tokio::sync::Notify::new()),
                progress_cb: |_| {},
            })
            .await;

        assert!(res.is_ok(), "Retry after corrupt segment failed: {:?}", res);
        assert!(hits.load(Ordering::Relaxed) >= 2);
        let _ = fs::remove_file(&dest);
    }

    #[tokio::test]
    #[ignore]
    async fn test_download_live_stream() {
        let downloader = HlsDownloader::new(None);
        let stop_signal = Arc::new(AtomicBool::new(false));
        let pause_signal = Arc::new(AtomicBool::new(false));
        let pause_notify = Arc::new(tokio::sync::Notify::new());
        let dest = PathBuf::from("/mnt/sdc/entertainment/Anime/5 Centimeters per Second/1.mp4");
        let res = downloader.download_stream(DownloadConfig {
            m3u8_url: "https://vault-12.owocdn.top/stream/12/12/b042d337707e776f90c2e43819650ec82dcf2ad87a18962bbd83ca03510ebbc8/uwu.m3u8".to_string(),
            referer: Some("https://kwik.cx/".to_string()),
            dest_path: dest.clone(),
            cache_key: None,
            replace_existing: true,
            concurrency: 16,
            stop_signal,
            pause_signal,
            pause_notify,
            progress_cb: |p| {
                println!("Progress: {:.1}% | {} | {} | ETA: {}", p.percent, p.size_str, p.speed_str, p.eta_str);
            },
        }).await;
        println!("Result: {:?}", res);
        assert!(res.is_ok());
        assert!(dest.exists());
    }

    #[test]
    fn test_safe_replace_preserves_existing_dst_on_error_or_cancel() {
        let dir = std::env::temp_dir().join(format!("paheflow_safe_replace_{}", fastrand::u64(..)));
        fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("existing.mp4");
        fs::write(&dst, b"precious-completed-episode").unwrap();

        let missing_src = dir.join("does_not_exist.mp4");
        let stop = AtomicBool::new(false);
        assert!(safe_replace_file_cancellable(&missing_src, &dst, &stop).is_err());
        assert_eq!(fs::read(&dst).unwrap(), b"precious-completed-episode");

        let real_src = dir.join("new.mp4");
        fs::write(&real_src, b"replacement").unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(safe_replace_file_cancellable(&real_src, &dst, &stop).is_err());
        assert_eq!(fs::read(&dst).unwrap(), b"precious-completed-episode");

        stop.store(false, Ordering::Relaxed);
        safe_replace_file_cancellable(&real_src, &dst, &stop).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"replacement");
        assert!(!real_src.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
