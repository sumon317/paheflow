use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Semaphore;

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
    /// When `true`, download to `<dest>.new` and atomically rename on success.
    pub replace_existing: bool,
    /// Maximum concurrent fragment downloads (clamped to 1–64).
    pub concurrency: usize,
    /// Shared cancellation flag; set to `true` to abort mid-download.
    pub stop_signal: Arc<AtomicBool>,
    /// Shared pause flag; set to `true` to pause downloading.
    pub pause_signal: Arc<AtomicBool>,
    /// Notify handle to wake up workers when resumed.
    pub pause_notify: Arc<tokio::sync::Notify>,
    /// Called after each fragment completes with cumulative progress metrics.
    pub progress_cb: F,
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
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
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
        let mut last_err = String::new();
        let mut body = String::new();

        for attempt in 1..=4 {
            let mut req = self.client.get(m3u8_url);
            if let Some(ref_str) = referer {
                req = req.header("Referer", ref_str);
            }
            req = req.header("Accept", "*/*");
            req = req.header("Accept-Language", "en-US,en;q=0.9");

            match req.send().await {
                Ok(res) => {
                    if !res.status().is_success() {
                        last_err = format!("m3u8 request failed with HTTP {}", res.status());
                    } else {
                        match res.text().await {
                            Ok(b) => {
                                body = b;
                                break;
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
                tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
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
                return Box::pin(self.parse_playlist(&best_url, referer)).await;
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

        // Store intermediate fragments and temporary assembly in the application cache.
        // On Android, this avoids writing hundreds of tiny chunks and dot/temp files
        // through the MediaProvider FUSE layer in public storage (e.g. Download/PaheFlow),
        // which triggers "Operation not permitted" (EPERM / os error 1).
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        final_mp4.hash(&mut hasher);
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

        let segments = self.parse_playlist(m3u8_url, referer).await?;
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
                match req.send().await {
                    Ok(res) => {
                        if !res.status().is_success() {
                            last_err = format!("Key request failed with HTTP {}", res.status());
                        } else {
                            match res.bytes().await {
                                Ok(bytes) => {
                                    if bytes.len() != 16 {
                                        return Err(format!(
                                            "Invalid AES key length (expected 16 bytes, got {})",
                                            bytes.len()
                                        ));
                                    }
                                    key_bytes = Some(bytes.to_vec());
                                    break;
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
                if attempt < 6 {
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                }
            }
            let bytes = key_bytes.ok_or(last_err)?;
            keys_map.insert(key_url, Arc::new(bytes));
        }

        let semaphore = Arc::new(Semaphore::new(concurrency.clamp(1, 64)));
        let completed_segments = Arc::new(AtomicUsize::new(0));
        let downloaded_bytes = Arc::new(AtomicU64::new(0));
        let start_time = Instant::now();

        // Prepare temporary directory for fragments in cache
        if replace_existing {
            // A replacement must never reuse fragments from a failed earlier
            // attempt; start from a clean slate.
            let _ = fs::remove_dir_all(&temp_dir);
            let _ = fs::remove_file(&temp_ts);
            let _ = fs::remove_file(&target_file);
        }
        fs::create_dir_all(&temp_dir)
            .map_err(|e| format!("Failed to create fragments directory {:?}: {}", temp_dir, e))?;
        // Remove partial fragment writes left behind by a previous crash.
        // Complete `.ts` fragments are intentionally reused to resume.
        if let Ok(entries) = fs::read_dir(&temp_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().ends_with(".ts.tmp") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }

        let mut tasks = Vec::with_capacity(total_segments);
        let progress_cb = Arc::new(progress_cb);

        for (idx, seg) in segments.into_iter().enumerate() {
            let sem = semaphore.clone();
            let seg_url = seg.url;
            let seg_key_bytes = seg.key_url.as_ref().and_then(|k| keys_map.get(k).cloned());
            let seg_iv = seg.iv;
            let referer_str = referer.map(|s| s.to_string());
            let client = self.client.clone();
            let completed = completed_segments.clone();
            let bytes_acc = downloaded_bytes.clone();
            let stop = stop_signal.clone();
            let pause = pause_signal.clone();
            let p_notify = pause_notify.clone();
            let seg_path = temp_dir.join(format!("{:06}.ts", idx));
            let cb = progress_cb.clone();
            let file_name = dest_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            tasks.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.map_err(|e| e.to_string())?;

                while pause.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                    p_notify.notified().await;
                }

                if stop.load(Ordering::Relaxed) {
                    return Err("Cancelled by user".to_string());
                }

                // If already downloaded and valid from a resumed session, skip re-download
                if seg_path.exists() {
                    if let Ok(meta) = fs::metadata(&seg_path) {
                        if meta.len() > 0 {
                            bytes_acc.fetch_add(meta.len(), Ordering::Relaxed);
                            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                            let elapsed_secs = start_time.elapsed().as_secs_f64().max(0.1);
                            let total_bytes = bytes_acc.load(Ordering::Relaxed);
                            let speed_bps = total_bytes as f64 / elapsed_secs;
                            let pct = (done as f64 / total_segments as f64) * 100.0;
                            let est_total_bytes =
                                (total_bytes as f64 / done as f64) * total_segments as f64;
                            let rem_bytes = (est_total_bytes - total_bytes as f64).max(0.0);
                            let eta_secs = if speed_bps > 0.0 {
                                rem_bytes / speed_bps
                            } else {
                                0.0
                            };
                            let eta_str = if eta_secs > 0.0 && eta_secs < 86400.0 {
                                format!(
                                    "{:02}:{:02}",
                                    (eta_secs as u64) / 60,
                                    (eta_secs as u64) % 60
                                )
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
                                file_name,
                            });
                            return Ok::<(), String>(());
                        }
                    }
                }

                // Download segment with up to 8 retries
                let mut data = None;
                let mut last_err = String::from("No response");
                for attempt in 1..=8 {
                    while pause.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                        p_notify.notified().await;
                    }
                    if stop.load(Ordering::Relaxed) {
                        return Err("Cancelled by user".to_string());
                    }
                    let mut req = client.get(&seg_url);
                    if let Some(ref r) = referer_str {
                        req = req.header("Referer", r);
                    }
                    req = req.header("Accept", "*/*");
                    match req.send().await {
                        Ok(res) => {
                            let status = res.status();
                            if status.is_success() {
                                match res.bytes().await {
                                    Ok(bytes) => {
                                        data = Some(bytes);
                                        break;
                                    }
                                    Err(e) => {
                                        last_err = format!(
                                            "Failed to read fragment body: {}",
                                            format_reqwest_err(&e)
                                        )
                                    }
                                }
                            } else {
                                last_err = format!("HTTP {}", status);
                            }
                        }
                        Err(e) => {
                            last_err = format!("Connection error: {}", format_reqwest_err(&e))
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(attempt * 400)).await;
                }

                let bytes = data.ok_or_else(|| {
                    format!("Fragment {} failed after retries: {}", idx, last_err)
                })?;

                // Decrypt segment if an AES-128 key is present
                let payload = if let (Some(key), Some(iv)) = (seg_key_bytes, seg_iv) {
                    decrypt_aes128_cbc(&bytes, &key, &iv)?
                } else {
                    bytes.to_vec()
                };

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

                bytes_acc.fetch_add(byte_len, Ordering::Relaxed);
                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;

                // Update progress metrics
                let elapsed_secs = start_time.elapsed().as_secs_f64().max(0.1);
                let total_bytes = bytes_acc.load(Ordering::Relaxed);
                let speed_bps = total_bytes as f64 / elapsed_secs;
                let pct = (done as f64 / total_segments as f64) * 100.0;

                let est_total_bytes = if done > 0 {
                    (total_bytes as f64 / done as f64) * total_segments as f64
                } else {
                    0.0
                };
                let rem_bytes = (est_total_bytes - total_bytes as f64).max(0.0);
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
                    file_name,
                });

                Ok::<(), String>(())
            }));
        }

        // Wait for all fragments to finish. Abort the remaining tasks as soon
        // as one fails so a failed job does not keep consuming bandwidth.
        let mut pending = tasks.into_iter();
        let mut failure = None;
        for task in pending.by_ref() {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    failure = Some(e);
                    break;
                }
                Err(e) => {
                    failure = Some(format!("Task error: {}", e));
                    break;
                }
            }
        }
        if let Some(e) = failure {
            for task in pending {
                task.abort();
            }
            return Err(e);
        }

        while pause_signal.load(Ordering::Relaxed) && !stop_signal.load(Ordering::Relaxed) {
            pause_notify.notified().await;
        }

        if stop_signal.load(Ordering::Relaxed) {
            return Err("Cancelled by user".to_string());
        }

        // Assemble fragments into temp_ts
        if temp_ts.exists() {
            let _ = fs::remove_file(&temp_ts);
        }
        let mut out_file = File::create(&temp_ts).map_err(|e| {
            format!(
                "Failed to create temporary stream file {:?}: {}",
                temp_ts, e
            )
        })?;
        for idx in 0..total_segments {
            let seg_path = temp_dir.join(format!("{:06}.ts", idx));
            let bytes = fs::read(&seg_path)
                .map_err(|e| format!("Missing fragment {:06}.ts: {}", idx, e))?;
            out_file
                .write_all(&bytes)
                .map_err(|e| format!("Failed writing assembled stream to {:?}: {}", temp_ts, e))?;
        }
        drop(out_file);

        // Remux with ffmpeg into target_file, or fall back to stream copy if ffmpeg is unavailable
        let ffmpeg_bin =
            std::env::var("ANIMEPAHE_FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string());
        let mut ffmpeg = Command::new(&ffmpeg_bin);
        ffmpeg
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

        let remux_success = match ffmpeg.output().await {
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
                eprintln!(
                    "[HLS] ffmpeg not available ({}), falling back to assembled stream",
                    e
                );
                false
            }
        };

        if remux_success {
            let _ = fs::remove_file(&temp_ts);
        } else {
            // Fallback: use the assembled MPEG transport stream directly
            safe_replace_file(&temp_ts, &target_file)
                .map_err(|e| format!("Stream assembly fallback failed: {}", e))?;
        }

        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::remove_file(&temp_ts);

        // Ensure final destination directory exists before swapping
        if let Some(parent) = final_mp4.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create destination folder {:?}: {}", parent, e))?;
        }

        // Only swap in the finished file after stream assembly succeeds. A failed or
        // interrupted attempt therefore cannot masquerade as a completed
        // episode on the next run.
        safe_replace_file(&target_file, &final_mp4)
            .map_err(|e| format!("Replacing output file failed: {}", e))?;

        let _ = fs::remove_file(&target_file);

        Ok(())
    }
}

fn safe_replace_file(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    if dst.exists() {
        let _ = fs::remove_file(dst);
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(src, dst)?;
            let _ = fs::remove_file(src);
            Ok(())
        }
    }
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
}
