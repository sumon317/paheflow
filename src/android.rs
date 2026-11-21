use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use jni::objects::{GlobalRef, JClass, JString, JValue};
use jni::sys::{jboolean, jint, jstring, JNI_FALSE, JNI_TRUE};
use jni::{JNIEnv, JavaVM};

use crate::client::HttpClient;
use crate::config::{read_config, write_config};
use crate::runner::Runner;
use crate::server;

static RUNNING: AtomicBool = AtomicBool::new(false);
static BOUND_PORT: AtomicU16 = AtomicU16::new(0);
static SERVER_TOKEN: OnceLock<Mutex<String>> = OnceLock::new();
static START_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static RUNNER: OnceLock<Mutex<Option<Arc<Runner>>>> = OnceLock::new();
static SHUTDOWN_NOTIFY: OnceLock<Mutex<Option<Arc<tokio::sync::Notify>>>> = OnceLock::new();
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();
static BRIDGE_CLASS: OnceLock<GlobalRef> = OnceLock::new();

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn parse_bridge_response(raw_json: &str) -> Result<String, String> {
    let val: serde_json::Value = serde_json::from_str(raw_json)
        .map_err(|e| format!("Invalid WebView bridge response: {}", e))?;
    let ok = val.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let status = val.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
    let body = val
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let error = val
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let cf_challenge = val
        .get("cfChallenge")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if ok {
        return Ok(body);
    }
    if cf_challenge || status == 403 {
        return Err(if error.is_empty() {
            "Cloudflare is challenging this request (HTTP 403 challenge). Tap 'Solve Cloudflare' or update cf_clearance in Settings.".to_string()
        } else {
            error
        });
    }
    if status == 429 {
        return Err(if error.is_empty() {
            "animepahe.pw rate-limited requests (HTTP 429 Too Many Requests).".to_string()
        } else {
            error
        });
    }
    Err(if error.is_empty() {
        format!("HTTP request failed with status code {}", status)
    } else {
        error
    })
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn android_webview_fetch_blocking(
    url: &str,
    referer: &str,
    cf_cookie: &str,
    user_agent: &str,
) -> Result<String, String> {
    let vm = JAVA_VM
        .get()
        .ok_or_else(|| "Android JavaVM is not initialized".to_string())?;
    let cls_ref = BRIDGE_CLASS
        .get()
        .ok_or_else(|| "AndroidHttpBridge class is not cached".to_string())?;

    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("JNI attach failed: {}", e))?;

    let local_cls = env
        .new_local_ref(cls_ref.as_obj())
        .map_err(|e| format!("Failed to create local ref for AndroidHttpBridge: {}", e))?;
    let cls = JClass::from(local_cls);

    let j_url = env
        .new_string(url)
        .map_err(|e| format!("JNI string alloc failed: {}", e))?;
    let j_ref = env
        .new_string(referer)
        .map_err(|e| format!("JNI string alloc failed: {}", e))?;
    let j_cf = env
        .new_string(cf_cookie)
        .map_err(|e| format!("JNI string alloc failed: {}", e))?;
    let j_ua = env
        .new_string(user_agent)
        .map_err(|e| format!("JNI string alloc failed: {}", e))?;

    let res = env.call_static_method(
        cls,
        "fetch",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[
            JValue::Object(&j_url),
            JValue::Object(&j_ref),
            JValue::Object(&j_cf),
            JValue::Object(&j_ua),
        ],
    );

    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
        return Err("AndroidHttpBridge.fetch threw a Java exception".to_string());
    }

    let val = res.map_err(|e| format!("AndroidHttpBridge.fetch call failed: {}", e))?;
    let obj = val
        .l()
        .map_err(|e| format!("AndroidHttpBridge.fetch return conversion failed: {}", e))?;
    let j_str = JString::from(obj);
    let json_str: String = env
        .get_string(&j_str)
        .map_err(|e| format!("Failed to read AndroidHttpBridge response string: {}", e))?
        .into();

    parse_bridge_response(&json_str)
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn android_webview_fetch(
    url: &str,
    referer: Option<&str>,
    cf_cookie: &str,
    user_agent: &str,
) -> Result<String, String> {
    let url = url.to_string();
    let referer = referer.unwrap_or("").to_string();
    let cf_cookie = cf_cookie.to_string();
    let user_agent = user_agent.to_string();

    tokio::task::spawn_blocking(move || {
        android_webview_fetch_blocking(&url, &referer, &cf_cookie, &user_agent)
    })
    .await
    .map_err(|e| format!("WebView HTTP bridge task panicked: {}", e))?
}

fn get_runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("Failed to initialize Android Tokio runtime")
    })
}

#[no_mangle]
pub extern "system" fn Java_com_animepahe_app_NativeEngine_startServer(
    mut env: JNIEnv,
    _class: JClass,
    preferred_port: jint,
    res_dir: JString,
    data_dir: JString,
    ext_download_dir: JString,
    cache_dir: JString,
) -> jint {
    if let Ok(vm) = env.get_java_vm() {
        let _ = JAVA_VM.set(vm);
    }
    if BRIDGE_CLASS.get().is_none() {
        match env.find_class("com/animepahe/app/AndroidHttpBridge") {
            Ok(cls) => {
                if let Ok(global) = env.new_global_ref(cls) {
                    let _ = BRIDGE_CLASS.set(global);
                }
            }
            Err(e) => {
                eprintln!(
                    "[NativeEngine] Could not cache AndroidHttpBridge class: {:?}",
                    e
                );
                let _ = env.exception_clear();
            }
        }
    }

    if RUNNING.load(Ordering::SeqCst) {
        return BOUND_PORT.load(Ordering::SeqCst) as jint;
    }

    let _start_guard = START_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    if RUNNING.load(Ordering::SeqCst) {
        return BOUND_PORT.load(Ordering::SeqCst) as jint;
    }

    let res_dir_str: String = match env.get_string(&res_dir) {
        Ok(s) => s.into(),
        Err(_) => return -1,
    };
    let data_dir_str: String = match env.get_string(&data_dir) {
        Ok(s) => s.into(),
        Err(_) => return -1,
    };
    let ext_download_dir_str: String = env
        .get_string(&ext_download_dir)
        .map(Into::into)
        .unwrap_or_default();
    let cache_dir_str: String = env
        .get_string(&cache_dir)
        .map(Into::into)
        .unwrap_or_default();

    if !res_dir_str.is_empty() {
        std::env::set_var("ANIMEPAHE_GUI_RESOURCES", &res_dir_str);
    }
    if !data_dir_str.is_empty() {
        std::env::set_var("ANIMEPAHE_GUI_DATA_DIR", &data_dir_str);
    }
    if !ext_download_dir_str.is_empty() {
        std::env::set_var("ANIMEPAHE_APP_EXT_DOWNLOAD_DIR", &ext_download_dir_str);
    }
    if !cache_dir_str.is_empty() {
        std::env::set_var("ANIMEPAHE_GUI_CACHE_DIR", &cache_dir_str);
    }

    let target_port = if preferred_port > 0 {
        preferred_port as u16
    } else {
        0
    };
    let rt = get_runtime();

    let (tx, rx) = std::sync::mpsc::channel();

    rt.spawn(async move {
        let addr = SocketAddr::from(([127, 0, 0, 1], target_port));
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "[NativeEngine] Failed to bind TcpListener on {}: {}",
                    addr, e
                );
                let _ = tx.send(Err(format!("Bind failed: {}", e)));
                return;
            }
        };

        let actual_port = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(target_port);
        BOUND_PORT.store(actual_port, Ordering::SeqCst);
        RUNNING.store(true, Ordering::SeqCst);

        let client = HttpClient::new();
        let runner = Arc::new(Runner::new());
        {
            let lock = RUNNER.get_or_init(|| Mutex::new(None));
            if let Ok(mut guard) = lock.lock() {
                *guard = Some(runner.clone());
            }
        }

        let pump_runner = runner.clone();
        let pump_client = client.clone();
        tokio::spawn(async move {
            pump_runner.run_queue_pump(pump_client).await;
        });

        let token = server::generate_token();
        {
            let lock = SERVER_TOKEN.get_or_init(|| Mutex::new(String::new()));
            if let Ok(mut guard) = lock.lock() {
                *guard = token.clone();
            }
        }

        let state = Arc::new(server::AppState {
            client,
            runner,
            stream_manager: Arc::new(crate::stream::StreamManager::new()),
            token,
        });

        let shutdown_notify = Arc::new(tokio::sync::Notify::new());
        {
            let lock = SHUTDOWN_NOTIFY.get_or_init(|| Mutex::new(None));
            if let Ok(mut guard) = lock.lock() {
                *guard = Some(shutdown_notify.clone());
            }
        }

        let app = server::create_router(state);
        let _ = tx.send(Ok(actual_port));

        println!(
            "[NativeEngine] Server listening on http://127.0.0.1:{}",
            actual_port
        );
        let serve_fut = axum::serve(listener, app).with_graceful_shutdown(async move {
            shutdown_notify.notified().await;
        });
        if let Err(e) = serve_fut.await {
            eprintln!("[NativeEngine] Axum server error: {}", e);
        }
        RUNNING.store(false, Ordering::SeqCst);
        BOUND_PORT.store(0, Ordering::SeqCst);
    });

    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(port)) => port as jint,
        Ok(Err(_)) | Err(_) => -1,
    }
}

#[no_mangle]
pub extern "system" fn Java_com_animepahe_app_NativeEngine_getServerPort(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    BOUND_PORT.load(Ordering::SeqCst) as jint
}

#[no_mangle]
pub extern "system" fn Java_com_animepahe_app_NativeEngine_isServerRunning(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    if RUNNING.load(Ordering::SeqCst) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[no_mangle]
pub extern "system" fn Java_com_animepahe_app_NativeEngine_getServerToken(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let token = if let Some(lock) = SERVER_TOKEN.get() {
        lock.lock().map(|g| g.clone()).unwrap_or_default()
    } else {
        String::new()
    };

    match env.new_string(token) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_animepahe_app_NativeEngine_notifyCloudflareSolved(
    mut env: JNIEnv,
    _class: JClass,
    cf_cookie: JString,
    user_agent: JString,
) -> jboolean {
    let cf_str: String = match env.get_string(&cf_cookie) {
        Ok(s) => s.into(),
        Err(_) => return JNI_FALSE,
    };
    let ua_str: String = match env.get_string(&user_agent) {
        Ok(s) => s.into(),
        Err(_) => return JNI_FALSE,
    };

    let mut cfg = read_config();
    if !cf_str.is_empty() {
        cfg.cf = cf_str;
    }
    if !ua_str.is_empty() {
        cfg.ua = ua_str;
    }

    if let Err(e) = write_config(cfg) {
        eprintln!("[NativeEngine] Failed to save config after CF solve: {}", e);
        return JNI_FALSE;
    }

    println!("[NativeEngine] Cloudflare credentials successfully updated");
    JNI_TRUE
}

#[no_mangle]
pub extern "system" fn Java_com_animepahe_app_NativeEngine_shutdownEngine(
    _env: JNIEnv,
    _class: JClass,
) {
    RUNNING.store(false, Ordering::SeqCst);
    BOUND_PORT.store(0, Ordering::SeqCst);

    if let Some(notify) = SHUTDOWN_NOTIFY
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|mut g| g.take())
    {
        notify.notify_waiters();
    }

    let runner = RUNNER
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|mut g| g.take());

    if let Some(runner) = runner {
        let rt = get_runtime();
        rt.block_on(async {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let _ = runner.stop().await;
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            })
            .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bridge_response;

    #[test]
    fn parse_bridge_response_handles_ok_cf_rate_limit_and_errors() {
        let ok =
            r#"{"ok":true,"status":200,"body":"<html>ok</html>","error":"","cfChallenge":false}"#;
        assert_eq!(parse_bridge_response(ok).unwrap(), "<html>ok</html>");

        let cf = r#"{"ok":false,"status":403,"body":"","error":"","cfChallenge":true}"#;
        let cf_err = parse_bridge_response(cf).unwrap_err();
        assert!(crate::client::is_cloudflare_error(&cf_err));

        let rl = r#"{"ok":false,"status":429,"body":"","error":"","cfChallenge":false}"#;
        assert!(parse_bridge_response(rl).unwrap_err().contains("429"));

        let bad = r#"{"ok":false,"status":404,"body":"","error":"","cfChallenge":false}"#;
        assert!(parse_bridge_response(bad).unwrap_err().contains("404"));
    }
}
