use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

use crate::client::HttpClient;
use crate::config::{read_config, write_config};
use crate::runner::Runner;
use crate::server;

static RUNNING: AtomicBool = AtomicBool::new(false);
static BOUND_PORT: AtomicU16 = AtomicU16::new(0);
static SERVER_TOKEN: OnceLock<Mutex<String>> = OnceLock::new();
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

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
) -> jint {
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

    if !res_dir_str.is_empty() {
        std::env::set_var("ANIMEPAHE_GUI_RESOURCES", &res_dir_str);
    }
    if !data_dir_str.is_empty() {
        std::env::set_var("ANIMEPAHE_GUI_DATA_DIR", &data_dir_str);
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
            token,
        });

        let app = server::create_router(state);
        let _ = tx.send(Ok(actual_port));

        println!(
            "[NativeEngine] Server listening on http://127.0.0.1:{}",
            actual_port
        );
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("[NativeEngine] Axum server error: {}", e);
        }
        RUNNING.store(false, Ordering::SeqCst);
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
