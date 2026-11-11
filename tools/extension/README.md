# AnimePahe GUI Auto-Sync Extension

A lightweight, zero-dependency browser extension and userscript that monitors Cloudflare's `cf_clearance` cookie on AnimePahe and automatically syncs it and your browser's User-Agent to your local native Rust GUI backend.

## Session token

Every GUI API request needs the per-launch `X-App-Token`. The extension reads
the token from the GUI page's `<meta name="app-token">` tag, caches it, and
sends it with `/api/sync-credentials`. If the GUI restarts and issues a new
token, the extension refreshes it and retries once; no manual setup is needed.
When the GUI is opened on a non-default port, that loopback origin is stored as
the sync target automatically.

The token is only sent to the loopback GUI. It is never transmitted to
AnimePahe or any third party.

---

## 🚀 Option 1: Native Auto-Detection (Zero Configuration!)

You don't even need an extension! The AnimePahe GUI now features **native browser profile inspection**:
1. Simply browse to `https://animepahe.pw` in Firefox (or your browser) once to pass the Cloudflare check.
2. Launch the GUI (`cargo run` or `./animepahe-gui`).
3. The app automatically scans your local Firefox profile for `cf_clearance`, matches your User-Agent, and configures itself on launch.
4. You can also click **"✨ Auto-Detect from Browser"** in the GUI Settings at any time to instantly pull fresh credentials.

---

## 🦊 Option 2: Permanent Firefox Extension (Never Removed on Restart)

By default, Firefox's `about:debugging` "Load Temporary Add-on" removes extensions on restart. To make the extension **permanent forever** in Firefox across all restarts:

### Automatic 1-Click Install:
Run the policy installer script from your terminal:
```bash
./tools/extension/install-firefox-policy.sh
```
This registers an official Firefox Enterprise Policy in `/etc/firefox/policies/policies.json` pointing to `animepahe-autosync.xpi`. Once installed, Firefox keeps the extension permanently active.

---

## 🦁 Option 3: Chromium Browsers (Brave, Chrome, Edge, Vivaldi)

Unpacked extensions in Chromium-based browsers **never** get removed on restart when loaded via Developer Mode:

1. Open your browser and navigate to:
   * **Brave:** `brave://extensions`
   * **Chrome:** `chrome://extensions`
2. Enable **Developer mode** (toggle in the top-right corner).
3. Click the **"Load unpacked"** button.
4. Select this directory:
   ```text
   tools/extension
   ```
*(Note: If prompted, use `manifest.chrome.json` or unzip `animepahe-autosync-chrome.zip`)*.

---

## 📜 Option 4: Violentmonkey / Tampermonkey Userscript

If you use Violentmonkey or Tampermonkey:
1. Open [`animepahe-autosync.user.js`](./animepahe-autosync.user.js).
2. Click "Install" in your userscript manager.
3. The userscript stays active permanently across all browser restarts with no developer warnings.
