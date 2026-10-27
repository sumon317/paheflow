# AnimePahe GUI

A local web GUI for the [animepahe-dl](https://github.com/KevCui/animepahe-dl) bash backend. Search anime, pick episodes, tweak options, watch progress, and manage your downloaded library — all from the browser.

## Features

| Area | What it does |
|------|--------------|
| **Browse** | Search animepahe.pw or filter the cached local slug list (6 000+ entries). |
| **Episodes** | Checklist with filter / select-all / range / latest buttons. Probe available resolutions & audio tracks per episode. |
| **Options** | Resolution, audio language (filtered to combinations that actually exist), concurrent fragments (stepper 1–64), “playlist link only” (`-l`), “replace existing” (`-R`), debug mode (`-d`). Live command preview. |
| **Progress** | Live log stream (SSE), progress bar, per-episode status chips, playlist links when `-l` is used. The log panel keeps the newest 1,000 lines so long batches stay responsive. |
| **Library** | Scan downloaded folders, open files/folders, delete single episodes or whole series. Follows the configured destination. Large collections render series headers first and load episode rows on demand. |
| **Settings** | First-time setup panel, download destination with a native folder picker, replace or clear the stored `cf_clearance`, “Use this browser’s User-Agent”, concurrent fragments, Test connection, and a dependency checklist. |
| **Responsive** | Works on desktop and mobile; activity panel can be toggled. |

The stored `cf_clearance` cookie is never sent back to the browser. Settings only shows whether a cookie is saved and how long it is; leaving the field blank keeps the current value, and **Clear** deletes it.

### Download destination

By default, anime folders are saved in the project directory. Use **Settings → Destination folder → Browse…** to pick another folder. The choice is saved immediately as `download_dir` in `config.json` and becomes the default for every later download; **Default** switches back to the project folder. A path can also be typed and applied with **Save configuration**. The folder is created if it does not exist, and the Library scans the same folder. Existing downloads are never moved automatically.

When the GUI starts a job it passes the destination to the backend as `ANIMEPAHE_OUTPUT_DIR`. Running `animepahe-dl.sh` directly also honours that variable, then `download_dir` from `config.json`, then the project directory.

### Quality and existing files

**Video resolution** and **Audio language** are filtered against the probe result for the current episode, so the GUI only offers pairs that the source actually provides. If a later episode lacks the chosen resolution, the backend keeps the chosen audio track and falls back to the highest resolution available in that track. If the chosen audio track itself is missing, it warns and uses the highest available source.

Existing episodes are **skipped by default** with an explicit warning. Turn on **Replace existing** (or pass `-R` to the shell) to download a fresh copy. Replacements are written to `<episode>.mp4.new`; the original file is replaced only after the download succeeds, so a failed or interrupted replacement never destroys a working episode.

Every network step is bounded by a total timeout: page/API reads use 30 seconds and the stream-resolver POST uses 60 seconds. A real `curl-impersonate` also enforces a 10-second connect timeout; the bundled `animepahe-curl` helper applies the total timeout to the whole request. Link resolution is retried before an episode is marked failed, and the rest of the batch continues.

Fragment downloads are retried with a short sleep. If a fragment is still unavailable, the episode fails instead of being saved with silent gaps. When the CDN rate-limits a large burst, retries automatically lower the fragment concurrency (down to 1), and every attempt starts from a clean temporary file so partial state can never be stitched into the final episode.

### High-volume downloads

Downloading hundreds of episodes produces far more log output than the browser can usefully display. The runner keeps at most 2,000 log entries and returns the newest 500 in snapshots. The progress panel renders at most 1,000 lines, batches incoming SSE events once per animation frame, and discards the oldest lines. **Copy logs** copies the retained window, not the complete job history.

The Library shows one header per anime folder. Small libraries (up to 5 folders / 200 files) open automatically; larger ones load an episode list only when **Show episodes** is clicked. This keeps both long batches and large collections from creating tens of thousands of DOM nodes.

## Requirements and one-command setup

The supported platforms are **Linux** (dnf, apt, pacman, or zypper), **macOS** (Homebrew), and **WSL2**. Native Windows and public server hosting are not supported.

From the project folder, run:

```bash
./requirements.sh
```

The installer:

1. Detects the package manager and installs any missing system packages (`jq`, `ffmpeg`, and optionally `fzf` and `mpv`).
2. Asks for administrator access only when it actually needs to install system packages. Every package-manager and download output is shown.
3. Installs `uv` into `.uv/` if it is not already available.
4. Uses `uv tool install` to install `yt-dlp[default,curl-cffi]` and the bundled `animepahe-curl` helper into `.uv/bin`.
5. Installs a project-local Python 3.12 with uv when the system Python is older than 3.11.
6. Verifies the complete dependency set.

Nothing is written to your shell profile. `requirements.sh` never reads or modifies `config.json` or cookies.

Useful modes:

```bash
./requirements.sh --check        # report only; changes nothing
./requirements.sh --yes          # no confirmation prompt (interactive sudo may still be needed)
./requirements.sh --no-optional  # skip fzf and mpv
./requirements.sh --yes --no-optional  # unattended, after system packages are present
```

| Tool | Purpose | Provided by |
|------|---------|-------------|
| Python 3.11+ | Runs the web server | System, or uv-managed Python 3.12 |
| `jq` | JSON parsing in the bash backend | System package |
| `ffmpeg` | Merging video/audio streams | System package |
| `yt-dlp` + `curl_cffi` | Episode downloading | `requirements.sh` → `.uv/bin` |
| `animepahe-curl` | Browser-impersonating HTTP client | `requirements.sh` → `.uv/bin` |
| `curl-impersonate` | Alternative Cloudflare client | Optional; preferred when already installed |
| `fzf` | Interactive picker for direct terminal use | Optional system package |
| `mpv` | Play playlist links from the GUI | Optional system package |

On macOS, install [Homebrew](https://brew.sh) first if `brew` is not available. On Fedora, the installer uses `ffmpeg-free` from the official repositories and falls back to `ffmpeg` if needed. On openSUSE, ffmpeg may require the Packman repository.

## Quick start

```bash
# Get the project folder (git clone or download), then:
cd animepahe_gui

./requirements.sh    # first time only
./animepahe-gui      # starts http://127.0.0.1:8765
```

The launcher prepends `.uv/bin` to `PATH`, checks all required tools, and prints every missing dependency at once instead of failing one by one. It binds the web server to `127.0.0.1` only.

### First-time setup

The first time the page loads without a stored cookie, it opens the **Settings** tab and shows the first-time setup panel:

1. Open **animepahe.pw** in the browser you normally use and pass the Cloudflare check.
2. Open DevTools → Application → Cookies → `animepahe.pw`, copy the `cf_clearance` value, and paste it into the cookie field.
3. Click **Use this browser’s User-Agent** so the User-Agent matches the browser that issued the cookie.
4. Click **Save & test connection**. You should see “Connection OK”.

The same User-Agent button is available in the User-Agent card later. A `config.json` is created with mode `0600` on the first save. `config.example.json` is a safe template with an empty cookie.

### Updating

```bash
./requirements.sh --yes
```

`yt-dlp` is updated in place, and the local `animepahe-curl` helper is rebuilt from the current project source. Your settings and downloads are left alone.

## Project layout

```
animepahe_gui/
├── requirements.sh       # cross-distro dependency installer
├── animepahe-dl.sh       # vendored backend (patched for highest-resolution default)
├── animepahe-gui         # launcher script
├── config.example.json   # safe template; real config.json is ignored by git
├── config.json           # cf_clearance, User-Agent, concurrent_fragments, download_dir (mode 0600)
├── anime.list            # cached slug → title catalogue (auto-refreshed)
├── tools/
│   └── animepahe_curl/   # curl_cffi-backed curl shim installed by requirements.sh
├── app/
│   ├── __init__.py
│   ├── config.py         # config.json read / write
│   ├── folder_picker.py  # native folder dialog (zenity, kdialog, Tkinter)
│   ├── backend.py        # animepahe.pw API client + local catalogue
│   ├── runner.py         # subprocess management + log parsing
│   ├── library.py        # downloaded file inspection
│   └── server.py         # ThreadingHTTPServer + JSON API + SSE
├── static/
│   ├── index.html        # single-page UI
│   ├── styles.css        # dark theme
│   └── app.js            # vanilla ES6 frontend
├── tests/                # stdlib unittest suite (no network required)
└── .uv/                  # installed tools and optional managed Python (ignored by git)
```

## API (for scripting)

Every `/api/*` request needs the per-launch session token. The token is embedded in the served page, so a local script can read it from there:

```bash
TOKEN="$(curl -s http://127.0.0.1:8765/ | sed -n 's/.*name="app-token" content="\([^"]*\)".*/\1/p')"
curl -s -H "X-App-Token: $TOKEN" http://127.0.0.1:8765/api/health
```

Requests without a valid token receive `403`; `/api/events` also accepts `?token=...` because `EventSource` cannot set headers. The server rejects any `Host` header other than `127.0.0.1`, `localhost`, or `::1`, which blocks DNS-rebinding attacks.

```
GET  /api/state        # config (redacted), health, deps, catalog, job
GET  /api/config       # redacted config: ua, concurrent_fragments, downloadDir, defaultDownloadDir, cfConfigured, cfLength
POST /api/config       # update config; omit "cf" to keep the stored cookie, send "" to clear it;
                       # send "download_dir": "/path" to change the destination, or "" to reset it
POST /api/pick-folder  # open a native folder chooser; body {"current": "/starting/path"}
GET  /api/catalog      # local slug list (?refresh=1 to re-download)
GET  /api/search?q=... # site search
GET  /api/episodes?slug=...&page=...  # paginated episode list
GET  /api/episode-options?slug=...&episode=...  # resolutions, audios, valid resolution/audio combinations
POST /api/run          # start a download job
POST /api/stop         # stop the running job
GET  /api/logs?after=N # log entries after sequence N
GET  /api/events       # SSE stream (see below)
GET  /api/library      # downloaded files
POST /api/library/delete          # delete one file
POST /api/library/delete-folder   # delete a whole anime folder
POST /api/open         # open file/folder with xdg-open
POST /api/test-connection
```

The SSE stream stays open across jobs. It emits named `log` events with SSE `id` fields, plus `state` and `done` events carrying JSON snapshots. A reconnecting client may send `Last-Event-ID` (or `?after=N`) so old log lines are not replayed. `state` and `status` payloads intentionally omit the log array; fetch `/api/logs` or subscribe to the stream instead.

## Security notes

- The server binds to `127.0.0.1` only. Do not expose it through a reverse proxy or bind it to a public interface.
- The session token changes on every launch. It protects every API route, including read-only ones, from other local processes and from cross-site requests.
- `config.json` and `config.json.bak` are written atomically with mode `0600`. The backup contains the previous cookie, so treat it as a secret too.
- `config.json`, `config.json.bak`, `cache/`, and `.uv/` are ignored by git. `.gitattributes` also excludes them from `git archive` distributions. `config.example.json` contains no secrets. When sharing the project, use `git archive`/a fresh clone rather than zipping a working folder that contains your `config.json`.
- Download logs are redacted before they reach the browser or `/api/logs` (`cf_clearance=...` becomes `<redacted>`).
- Library deletes are limited to video files directly inside a top-level anime folder within the configured download destination. Empty folders, nested paths, the destination root, and application folders (`app/`, `static/`, `tests/`, `cache/`) are refused. The destination itself cannot be the filesystem root.

## Keyboard shortcuts (UI)

| Key | Action |
|-----|--------|
| `/` | Focus the search box (when not typing in a field) |
| `Enter` in search | Run search |
| `Esc` in search | Clear the query and show the local catalog |

## Testing

The automated suite uses Python's standard library and never touches the real `anime.list`, `config.json`, or the network:

```bash
python3 -m unittest discover -s tests -v
```

It covers config permission, cookie preservation, and download-destination rules; library delete guards and destination scoping; shell startup, helper fallback, optional `fzf`, catalog, and destination precedence; folder-picker fallbacks; runner log sequencing, bounds, redaction, and destination propagation; HTTP token/Host/SSE behaviour against ephemeral servers; the `animepahe-curl` option parser and its HTTP behaviour against a local test server; and dependency reporting.

Verify an installed checkout without changing anything:

```bash
./requirements.sh --check
```

`tests/test_playlist_chain.sh` is a separate manual smoke test for the live kwik → glot.io → m3u8 → yt-dlp chain. It requires network access and a valid cf_clearance cookie in the browser-generated stream URL, so discovery does not run it.

## Troubleshooting

- **“error code: 1015”** in logs → Cloudflare rate-limited the API. The GUI retries automatically; if it persists, wait a minute or refresh the catalogue less often.
- **“cf_clearance expired”** or a Cloudflare “Just a moment…” page → copy a fresh cookie from the same browser session, click **Use this browser’s User-Agent**, then save. The cookie is tied to the browser’s User-Agent and often to the network address.
- **A required tool is missing** → run `./requirements.sh`, then start the GUI again. The launcher lists every missing tool; the dependency card in Settings shows resolved paths.
- **`animepahe-curl` fails to start** → run `./requirements.sh --yes` to rebuild the helper and its `curl_cffi` dependency.
- **“Invalid or missing session token”** → The server restarted and issued a new token. Reload the page.
- **No playlists / downloads** → Check `yt-dlp --list-impersonate-targets`; at least one target must show `curl_cffi`. `./requirements.sh --check` verifies this.
- **A CDN times out** → The downloader retries each episode up to three times with a fresh stream link and a 30-second socket timeout. Link resolution and playlist extraction are retried as well. If one episode still fails, the rest of the batch continues; the job exits non-zero and the failed episode is marked in the progress chips.
- **An episode is not replaced** → Existing files are skipped unless **Replace existing** is enabled. Replacements download to `<episode>.mp4.new` and only swap in after a successful download.
- **Port already in use** → Set `ANIMEPAHE_GUI_PORT=8766` before launching.

## License

The GUI code is MIT-licensed. The vendored `animepahe-dl.sh` remains under its original **WTFPL** license.
