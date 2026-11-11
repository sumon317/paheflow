use crate::config::{read_config, write_config, AppConfig};
use std::process::Command;

pub struct DetectedCookie {
    pub cf: String,
    pub ua: String,
    pub source: String,
}

pub fn detect_from_firefox() -> Option<DetectedCookie> {
    if which::which("python3").is_err() {
        return None;
    }

    let script = r#"
import glob, os, sqlite3, shutil, json

def find_cookie():
    candidates = [
        os.path.expanduser('~/.config/mozilla/firefox'),
        os.path.expanduser('~/.mozilla/firefox'),
        os.path.expanduser('~/.var/app/org.mozilla.firefox/.mozilla/firefox'),
        os.path.expanduser('~/snap/firefox/common/.mozilla/firefox'),
        os.path.expanduser('~/.librewolf'),
        os.path.expanduser('~/.var/app/io.gitlab.librewolf-community/.librewolf'),
        os.path.expanduser('~/.waterfox'),
        os.path.expanduser('~/.floorp'),
        os.path.expanduser('~/.zen'),
        os.path.expanduser('~/.var/app/app.zen_browser.zen/.zen')
    ]
    newest = None
    for base in candidates:
        if not os.path.isdir(base):
            continue
        for db in glob.glob(os.path.join(base, '*default*', 'cookies.sqlite')):
            pid = os.getpid()
            bname = os.path.basename(os.path.dirname(db))
            tmp = f'/tmp/ff_ck_{pid}_{bname}.sqlite'
            tmp_wal = tmp + '-wal'
            tmp_shm = tmp + '-shm'
            try:
                shutil.copy2(db, tmp)
                if os.path.exists(db + '-wal'):
                    shutil.copy2(db + '-wal', tmp_wal)
                if os.path.exists(db + '-shm'):
                    shutil.copy2(db + '-shm', tmp_shm)

                conn = sqlite3.connect(tmp)
                cur = conn.cursor()
                cur.execute("SELECT value, lastAccessed, expiry FROM moz_cookies WHERE name='cf_clearance' AND host LIKE '%animepahe%' ORDER BY lastAccessed DESC LIMIT 1")
                row = cur.fetchone()
                conn.close()
                if row:
                    val, last_acc, expiry = row
                    if not newest or last_acc > newest['last_accessed']:
                        newest = {
                            'cf': val,
                            'last_accessed': last_acc,
                            'source': db
                        }
            except Exception:
                pass
            finally:
                for f in (tmp, tmp_wal, tmp_shm):
                    if os.path.exists(f):
                        try: os.remove(f)
                        except: pass
    if newest:
        ua = 'Mozilla/5.0 (X11; Linux x86_64; rv:159.0) Gecko/20100101 Firefox/159.0'
        try:
            import subprocess
            out = subprocess.check_output(['firefox', '--version'], stderr=subprocess.DEVNULL).decode()
            ver = out.strip().split()[-1]
            if ver and ver[0].isdigit():
                # Firefox freezes minor versions in the User-Agent string to <major>.0
                major = ver.split('.')[0]
                ua = f'Mozilla/5.0 (X11; Linux x86_64; rv:{major}.0) Gecko/20100101 Firefox/{major}.0'
        except Exception:
            pass
        newest['ua'] = ua
    return newest

res = find_cookie()
if res:
    print(json.dumps(res))
"#;

    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;

    let cf = json.get("cf")?.as_str()?.to_string();
    let ua = json.get("ua")?.as_str()?.to_string();
    let source = json.get("source")?.as_str()?.to_string();

    Some(DetectedCookie { cf, ua, source })
}

pub fn try_auto_import() -> Option<AppConfig> {
    let mut cfg = read_config();
    if let Some(detected) = detect_from_firefox() {
        if !detected.cf.is_empty() && (cfg.cf.is_empty() || cfg.cf != detected.cf) {
            println!(
                "[Auto-Config] Successfully loaded Cloudflare credentials from Firefox profile: {}",
                detected.source
            );
            cfg.cf = detected.cf;
            cfg.ua = detected.ua;
            if let Ok(saved) = write_config(cfg) {
                return Some(saved);
            }
        }
    }
    None
}
