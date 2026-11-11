use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{read_config, resolve_download_dir};

const VIDEO_SUFFIXES: &[&str] = &[".mp4", ".mkv", ".webm", ".m4v"];
const RESERVED_DIRS: &[&str] = &[
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
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEpisode {
    pub file: String,
    pub path: String,
    pub size: u64,
    pub size_text: String,
    pub modified: f64,
    pub modified_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryFolder {
    pub name: String,
    pub path: String,
    pub count: usize,
    pub total: u64,
    pub total_text: String,
    pub modified: f64,
    pub episodes: Vec<LibraryEpisode>,
}

fn download_root() -> PathBuf {
    let cfg = read_config();
    resolve_download_dir(&cfg)
}

fn is_video(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .map(|name| VIDEO_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)))
        .unwrap_or(false)
}

pub fn human_size(size: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size.max(0.0);
    let mut index = 0;
    while value >= 1024.0 && index < units.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{} {}", value as u64, units[index])
    } else {
        format!("{:.1} {}", value, units[index])
    }
}

fn format_mtime(seconds: f64) -> String {
    Local
        .timestamp_opt(seconds as i64, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Sort `12.mp4` after `2.mp4` instead of lexicographically,
/// and keep `1.mp4` and `1 (Dub).mp4` grouped together before `2.mp4`.
fn episode_sort_key(file: &str) -> (bool, f64, String) {
    let stem = Path::new(file)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if let Ok(number) = stem.parse::<f64>() {
        return (false, number, stem);
    }
    let prefix: String = stem
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if !prefix.is_empty() {
        if let Ok(number) = prefix.parse::<f64>() {
            return (false, number, stem);
        }
    }
    (true, 0.0, stem)
}

pub fn scan() -> Vec<LibraryFolder> {
    let root = download_root();
    let mut folders = Vec::new();

    if !root.is_dir() {
        return folders;
    }

    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(_) => return folders,
    };

    let mut dir_entries: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dir_entries.sort();

    for dir in dir_entries {
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if name.starts_with('.') || RESERVED_DIRS.contains(&name.as_str()) {
            continue;
        }

        let file_entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        let mut episodes = Vec::new();
        let mut total: u64 = 0;
        let mut latest = 0.0_f64;

        let mut files: Vec<PathBuf> = file_entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        files.sort_by(|a, b| {
            let a_name = a.file_name().unwrap_or_default().to_string_lossy();
            let b_name = b.file_name().unwrap_or_default().to_string_lossy();
            episode_sort_key(&a_name)
                .partial_cmp(&episode_sort_key(&b_name))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        for file in files {
            if !is_video(&file) {
                continue;
            }

            let fname = file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if let Ok(meta) = file.metadata() {
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs_f64())
                    .unwrap_or(0.0);

                total += size;
                latest = latest.max(mtime);
                episodes.push(LibraryEpisode {
                    file: fname,
                    path: file.to_string_lossy().to_string(),
                    size,
                    size_text: human_size(size as f64),
                    modified: mtime,
                    modified_text: format_mtime(mtime),
                });
            }
        }

        if !episodes.is_empty() {
            folders.push(LibraryFolder {
                name,
                path: dir.to_string_lossy().to_string(),
                count: episodes.len(),
                total,
                total_text: human_size(total as f64),
                modified: latest,
                episodes,
            });
        }
    }

    folders.sort_by(|a, b| {
        b.modified
            .partial_cmp(&a.modified)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    folders
}

fn canonical_root() -> Result<PathBuf, String> {
    let root = download_root();
    root.canonicalize()
        .map_err(|e| format!("Cannot resolve download folder: {}", e))
}

fn safe_path(candidate: &str, root: &Path) -> Result<PathBuf, String> {
    let path = PathBuf::from(candidate);
    let resolved = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };

    let canonical = resolved
        .canonicalize()
        .map_err(|e| format!("Cannot resolve path: {}", e))?;
    if canonical != root && !canonical.starts_with(root) {
        return Err("Refusing to touch a path outside the download folder.".to_string());
    }
    Ok(canonical)
}

fn require_top_level_folder(path: &Path, root: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| "Invalid path.".to_string())?;
    if parent == root {
        return Err("Refusing to touch a file in the download root.".to_string());
    }
    if parent.parent() != Some(root) {
        return Err("Only direct anime folders in the library can be changed.".to_string());
    }
    let name = parent
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    if name.starts_with('.') || RESERVED_DIRS.contains(&name.as_str()) {
        return Err("Refusing to touch an application folder.".to_string());
    }
    Ok(())
}

pub fn delete_file(path_str: &str) -> Result<serde_json::Value, String> {
    let root = canonical_root()?;
    let path = safe_path(path_str, &root)?;
    if !path.is_file() {
        return Err("File not found or is a directory.".to_string());
    }
    if !is_video(&path) {
        return Err("Only downloaded video files can be deleted.".to_string());
    }
    require_top_level_folder(&path, &root)?;
    fs::remove_file(&path).map_err(|e| format!("Failed to delete file: {}", e))?;
    Ok(serde_json::json!({
        "deleted": path.to_string_lossy().to_string(),
        "remaining": scan()
    }))
}

pub fn delete_folder(path_str: &str) -> Result<serde_json::Value, String> {
    let root = canonical_root()?;
    let path = safe_path(path_str, &root)?;
    if path == root {
        return Err("Refusing to delete download root folder.".to_string());
    }
    if !path.is_dir() {
        return Err("Folder not found or is a file.".to_string());
    }
    if path.parent() != Some(root.as_path()) {
        return Err("Only direct anime folders in the library can be deleted.".to_string());
    }
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    if name.starts_with('.') || RESERVED_DIRS.contains(&name.as_str()) {
        return Err("Refusing to delete an application folder.".to_string());
    }
    let has_video = fs::read_dir(&path)
        .map_err(|e| format!("Cannot inspect folder: {}", e))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .any(|file| file.is_file() && is_video(&file));
    if !has_video {
        return Err("Refusing to delete a folder without downloaded videos.".to_string());
    }

    fs::remove_dir_all(&path).map_err(|e| format!("Failed to delete folder: {}", e))?;
    Ok(serde_json::json!({
        "deleted": path.to_string_lossy().to_string(),
        "remaining": scan()
    }))
}

pub fn open_path(path_str: &str) -> Result<serde_json::Value, String> {
    let root = canonical_root()?;
    let path = safe_path(path_str, &root)?;
    if !path.exists() {
        return Err("Path does not exist.".to_string());
    }
    let p_str = path.to_string_lossy().to_string();

    let candidates: [(&str, &[&str]); 4] = [
        ("xdg-open", &[p_str.as_str()]),
        ("kioclient5", &["exec", p_str.as_str()]),
        ("kioclient", &["exec", p_str.as_str()]),
        ("open", &[p_str.as_str()]),
    ];

    for (command, args) in candidates {
        if std::process::Command::new(command)
            .args(args)
            .spawn()
            .is_ok()
        {
            return Ok(serde_json::json!({ "opened": p_str }));
        }
    }

    Err("No tool available to open path (xdg-open or kioclient).".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_episodes_naturally() {
        let mut names = vec![
            "10.mp4",
            "2 (Dub).mp4",
            "2.mp4",
            "1.mp4",
            "1 (Dub).mp4",
            "special.mp4",
        ];
        names.sort_by(|a, b| {
            episode_sort_key(a)
                .partial_cmp(&episode_sort_key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        assert_eq!(
            names,
            vec![
                "1.mp4",
                "1 (Dub).mp4",
                "2.mp4",
                "2 (Dub).mp4",
                "10.mp4",
                "special.mp4"
            ]
        );
    }

    #[test]
    fn formats_human_sizes() {
        assert_eq!(human_size(512.0), "512 B");
        assert_eq!(human_size(1536.0), "1.5 KB");
        assert_eq!(human_size(2.0 * 1024.0 * 1024.0), "2.0 MB");
    }

    #[test]
    fn rejects_reserved_and_nested_paths() {
        let root = Path::new("/tmp/anime-library-root");
        let reserved = root.join("src").join("1.mp4");
        assert!(require_top_level_folder(&reserved, root).is_err());

        let nested = root.join("Show").join("Season 1").join("1.mp4");
        assert!(require_top_level_folder(&nested, root).is_err());

        let valid = root.join("Show").join("1.mp4");
        assert!(require_top_level_folder(&valid, root).is_ok());
    }
}
