use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_FRAGMENTS: usize = 8;
pub const MIN_FRAGMENTS: usize = 1;
pub const MAX_FRAGMENTS: usize = 64;

const RESOURCES_ENV: &str = "ANIMEPAHE_GUI_RESOURCES";
const DATA_ENV: &str = "ANIMEPAHE_GUI_DATA_DIR";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub cf: String,
    #[serde(default)]
    pub ua: String,
    #[serde(default = "default_fragments")]
    pub concurrent_fragments: usize,
    #[serde(default)]
    pub download_dir: Option<String>,
}

fn default_fragments() -> usize {
    DEFAULT_FRAGMENTS
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            cf: String::new(),
            ua: String::new(),
            concurrent_fragments: DEFAULT_FRAGMENTS,
            download_dir: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicConfig {
    pub ua: String,
    pub concurrent_fragments: usize,
    pub download_dir: String,
    pub default_download_dir: String,
    pub cf_configured: bool,
    pub cf_length: usize,
}

/// Directory that contains the read-only application assets (`static/`, `anime.list`).
///
/// The project checkout is preferred when it is the current directory. Otherwise
/// walk up from the executable location so a binary run from `target/release`
/// still finds the project resources. A packaged installation can override the
/// location with `ANIMEPAHE_GUI_RESOURCES`.
pub fn resource_dir() -> PathBuf {
    if let Some(value) = std::env::var_os(RESOURCES_ENV) {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        if cwd.join("static").join("index.html").is_file() {
            return cwd;
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut current = exe.parent().map(Path::to_path_buf);
        while let Some(dir) = current {
            if dir.join("static").join("index.html").is_file() {
                return dir;
            }
            current = dir.parent().map(Path::to_path_buf);
        }
    }

    let xdg_data = default_data_dir();
    if xdg_data.join("static").join("index.html").is_file() {
        return xdg_data;
    }

    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn default_data_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let path = PathBuf::from(xdg);
        if !path.as_os_str().is_empty() {
            return path.join("animepahe-gui");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home);
        if !path.as_os_str().is_empty() {
            return path.join(".local").join("share").join("animepahe-gui");
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Writable directory for `config.json`, `cache/`, `anime.list`, and the
/// default download folder. A project checkout keeps everything next to the
/// source tree; an installed binary uses the XDG data directory.
pub fn data_dir() -> PathBuf {
    if let Some(value) = std::env::var_os(DATA_ENV) {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        let project_like = cwd.join("static").join("index.html").is_file()
            || cwd.join("anime.list").is_file()
            || cwd.join("config.json").is_file();
        if project_like {
            return cwd;
        }
    }

    default_data_dir()
}

/// Directory for temporary download fragments, assembled streams, and cache.
pub fn cache_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("ANIMEPAHE_GUI_CACHE_DIR") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    data_dir().join("cache")
}

/// Backwards-compatible name for the writable application directory.
pub fn project_base_dir() -> PathBuf {
    data_dir()
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn clamp_fragments(val: usize) -> usize {
    val.clamp(MIN_FRAGMENTS, MAX_FRAGMENTS)
}

pub fn read_config() -> AppConfig {
    let path = config_path();
    if !path.exists() {
        return AppConfig::default();
    }
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => AppConfig::default(),
    }
}

pub fn write_config(mut cfg: AppConfig) -> Result<AppConfig, String> {
    cfg.concurrent_fragments = clamp_fragments(cfg.concurrent_fragments);
    let path = config_path();
    let parent = path
        .parent()
        .ok_or_else(|| "Invalid config path".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("Cannot create config directory: {}", e))?;

    let bak_path = path.with_extension("json.bak");
    let tmp_path = path.with_extension("json.tmp");
    let json_bytes = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;

    // Back up the previous config before replacing it.
    if path.exists() {
        let _ = fs::copy(&path, &bak_path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&bak_path, fs::Permissions::from_mode(0o600));
        }
    }

    // Write to a temporary file first, then rename so a crash or full disk
    // cannot leave a truncated config behind.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;

        let _ = fs::remove_file(&tmp_path);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|e| e.to_string())?;
        file.write_all(json_bytes.as_bytes())
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }

    #[cfg(not(unix))]
    fs::write(&tmp_path, &json_bytes).map_err(|e| e.to_string())?;

    if let Err(e) = fs::rename(&tmp_path, &path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("Cannot replace config.json: {}", e));
    }

    Ok(cfg)
}

/// Default download folder.
///
/// On Android, defaults to `/storage/emulated/0/Download/PaheFlow`.
/// On Linux desktop, defaults to `~/Downloads/PaheFlow` or `$XDG_DOWNLOAD_DIR/PaheFlow`.
pub fn default_download_dir() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        let parent = Path::new("/storage/emulated/0/Download");
        let android_dl = parent.join("PaheFlow");
        if parent.exists() || Path::new("/storage/emulated/0").exists() {
            let _ = std::fs::create_dir_all(&android_dl);
            return android_dl;
        }
    }

    if let Some(dl) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        let p = PathBuf::from(dl);
        if !p.as_os_str().is_empty() {
            let target = p.join("PaheFlow");
            let _ = std::fs::create_dir_all(&target);
            return target;
        }
    }

    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join("Downloads");
        if p.exists() || p.parent().map(|h| h.exists()).unwrap_or(false) {
            let target = p.join("PaheFlow");
            let _ = std::fs::create_dir_all(&target);
            return target;
        }
    }

    project_base_dir()
}

pub fn public_config(cfg: &AppConfig) -> PublicConfig {
    let default_dir = default_download_dir().to_string_lossy().to_string();
    let base_internal = project_base_dir().to_string_lossy().to_string();
    let old_android_root = "/storage/emulated/0/Download";
    let old_animepahe = "/storage/emulated/0/Download/AnimePahe";

    let download_dir = cfg
        .download_dir
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| {
            !s.is_empty() && *s != base_internal && *s != old_android_root && *s != old_animepahe
        })
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_dir.clone());

    PublicConfig {
        ua: cfg.ua.clone(),
        concurrent_fragments: clamp_fragments(cfg.concurrent_fragments),
        download_dir,
        default_download_dir: default_dir,
        cf_configured: !cfg.cf.trim().is_empty(),
        cf_length: cfg.cf.trim().len(),
    }
}

pub fn resolve_download_dir(cfg: &AppConfig) -> PathBuf {
    let default_dir = default_download_dir();
    let base_internal = project_base_dir().to_string_lossy().to_string();
    let old_android_root = "/storage/emulated/0/Download";
    let old_animepahe = "/storage/emulated/0/Download/AnimePahe";

    if let Some(ref d) = cfg.download_dir {
        let trimmed = d.trim();
        if !trimmed.is_empty()
            && trimmed != base_internal
            && trimmed != old_android_root
            && trimmed != old_animepahe
        {
            let expanded = if let Some(stripped) = trimmed.strip_prefix("~/") {
                if let Ok(home) = std::env::var("HOME") {
                    Path::new(&home).join(stripped)
                } else {
                    PathBuf::from(trimmed)
                }
            } else {
                PathBuf::from(trimmed)
            };
            if expanded.is_absolute() {
                return expanded;
            } else {
                return default_dir.join(expanded);
            }
        }
    }
    default_dir
}
