"""Local HTTP server that serves the GUI and the JSON API."""

from __future__ import annotations

import json
import mimetypes
import os
import secrets
import shutil
import subprocess
import sys
import threading
import time
import urllib.parse
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

from . import backend, library
from .config import BASE_DIR, ConfigError, read_config, write_config
from .folder_picker import FolderPickerError, pick_folder
from .runner import Runner, RunnerError

STATIC_DIR = BASE_DIR / "static"
HOST = "127.0.0.1"
DEFAULT_PORT = 8765
ALLOWED_HOSTS = {"127.0.0.1", "localhost", "::1"}
SSE_HEARTBEAT_SECONDS = 15.0

TOKEN = secrets.token_urlsafe(24)

RUNNER = Runner()


def public_config(cfg: dict[str, Any]) -> dict[str, Any]:
    """Return config data that is safe to expose to the browser.

    The cookie itself never leaves the server: the UI only needs to know
    whether one is stored and how long it is.
    """
    cf = str(cfg.get("cf", "") or "")
    return {
        "ua": str(cfg.get("ua", "") or ""),
        "concurrent_fragments": cfg.get("concurrent_fragments", 8),
        "downloadDir": str(cfg.get("download_dir") or BASE_DIR),
        "defaultDownloadDir": str(BASE_DIR),
        "cfConfigured": bool(cf),
        "cfLength": len(cf),
    }


def _dependency_path(binaries: list[str]) -> str | None:
    """Find a helper on PATH, preferring the project-local .uv/bin install."""
    local_bin = BASE_DIR / ".uv" / "bin"
    for binary in binaries:
        candidate = local_bin / binary
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    for binary in binaries:
        path = shutil.which(binary)
        if path:
            return path
    return None


def dependencies() -> list[dict[str, Any]]:
    """Report which helper programs the backend needs are installed."""
    checks = [
        (
            "Cloudflare client",
            "Cloudflare bypass (curl-impersonate or animepahe-curl)",
            ["curl-impersonate", "animepahe-curl"],
            True,
        ),
        ("jq", "JSON parsing in the bash backend", ["jq"], True),
        ("yt-dlp", "Episode downloading", ["yt-dlp"], True),
        ("ffmpeg", "Merging video and audio streams", ["ffmpeg"], True),
        (
            "fzf",
            "Interactive picker for terminal use (optional)",
            ["fzf"],
            False,
        ),
        ("mpv", "Optional: play a playlist link", ["mpv"], False),
    ]
    result = []
    for name, purpose, binaries, required in checks:
        path = _dependency_path(binaries)
        result.append(
            {
                "name": name,
                "purpose": purpose,
                "required": required,
                "found": bool(path),
                "path": path or "",
            }
        )
    return result


def health() -> dict[str, Any]:
    """Summarise configuration readiness."""
    try:
        cfg = read_config()
        error = ""
    except ConfigError as exc:
        cfg = {"cf": "", "ua": "", "concurrent_fragments": 8}
        error = str(exc)
    return {
        "cfConfigured": bool(cfg.get("cf")),
        "uaConfigured": bool(cfg.get("ua")),
        "cfLength": len(cfg.get("cf", "")),
        "error": error,
    }


class GuiHTTPServer(ThreadingHTTPServer):
    """Threaded server that ignores clients hanging up mid-response."""

    daemon_threads = True
    allow_reuse_address = True

    def handle_error(self, request: Any, client_address: Any) -> None:
        error = sys.exc_info()[1]
        if isinstance(error, (BrokenPipeError, ConnectionResetError)):
            return
        super().handle_error(request, client_address)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "AnimePaheGUI/1.0"

    # ------------------------------------------------------------- utilities

    def log_message(self, fmt: str, *args: Any) -> None:  # noqa: A003 - silence access log
        pass

    def _send_bytes(self, body: bytes, content_type: str, status: int = 200) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Referrer-Policy", "no-referrer")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def check_host(self) -> bool:
        """Reject DNS-rebinding requests addressed to another hostname."""
        host = self.headers.get("Host") or ""
        if not host:
            return True
        try:
            hostname = urllib.parse.urlsplit("//" + host).hostname
        except ValueError:
            hostname = None
        if not hostname or hostname.rstrip(".").lower() not in ALLOWED_HOSTS:
            self.send_error_json("Invalid Host header.", 403)
            return False
        return True

    def send_json(self, payload: Any, status: int = 200) -> None:
        body = json.dumps(payload, default=str).encode("utf-8")
        self._send_bytes(body, "application/json; charset=utf-8", status)

    def send_error_json(self, message: str, status: int = 400) -> None:
        self.send_json({"error": str(message)}, status)

    def read_body(self) -> dict[str, Any]:
        length = int(self.headers.get("Content-Length") or 0)
        if not length:
            return {}
        raw = self.rfile.read(length)
        try:
            data = json.loads(raw.decode("utf-8") or "{}")
        except (json.JSONDecodeError, UnicodeDecodeError) as exc:
            raise ValueError(f"Invalid JSON body: {exc}") from exc
        if not isinstance(data, dict):
            raise ValueError("Request body must be a JSON object.")
        return data

    def check_token(self) -> bool:
        """Guard every API endpoint against other local processes and the web."""
        supplied = self.headers.get("X-App-Token") or ""
        query = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
        if not supplied and query.get("token"):
            supplied = query["token"][0]
        # compare_digest() with str only accepts ASCII; encode so a malformed
        # header is rejected instead of raising TypeError and dropping the
        # connection.
        matches = secrets.compare_digest(
            str(supplied).encode("utf-8", "replace"), TOKEN.encode("utf-8")
        )
        if not matches:
            self.send_error_json("Invalid or missing session token.", 403)
            return False
        return True

    # ------------------------------------------------------------------ GET

    def do_GET(self) -> None:  # noqa: N802 - http.server API
        if not self.check_host():
            return
        parsed = urllib.parse.urlsplit(self.path)
        path = parsed.path
        query = urllib.parse.parse_qs(parsed.query)

        if (path == "/api" or path.startswith("/api/")) and not self.check_token():
            return

        try:
            if path in ("/", "/index.html"):
                self.serve_index()
                return
            if path.startswith("/static/"):
                self.serve_static(path[len("/static/") :])
                return
            if path == "/api/state":
                self.send_json(
                    {
                        "config": public_config(read_config()),
                        "health": health(),
                        "dependencies": dependencies(),
                        "catalog": {"items": backend.catalog()},
                        "job": RUNNER.snapshot(include_logs=False),
                    }
                )
                return
            if path == "/api/config":
                self.send_json(public_config(read_config()))
                return
            if path == "/api/health":
                self.send_json(health())
                return
            if path == "/api/deps":
                self.send_json(dependencies())
                return
            if path == "/api/catalog":
                refresh = query.get("refresh", ["0"])[0] in ("1", "true", "yes")
                items = backend.refresh_catalog() if refresh else backend.catalog()
                self.send_json({"items": items, "count": len(items), "refreshed": refresh})
                return
            if path == "/api/search":
                term = query.get("q", [""])[0]
                self.send_json({"items": backend.search(term)})
                return
            if path == "/api/episodes":
                slug = query.get("slug", [""])[0]
                page = query.get("page", [""])[0]
                try:
                    if page:
                        items, last_page = backend.release_page(slug, int(page or 1))
                        self.send_json(
                            {"items": items, "lastPage": last_page, "page": int(page or 1)}
                        )
                    else:
                        self.send_json({"items": backend.release(slug)})
                except backend.BackendError as exc:
                    cached = backend.load_release_cache(slug)
                    if cached:
                        # The site is unreachable but we have this list on disk.
                        self.send_json(
                            {"items": cached, "stale": True, "warning": str(exc)}
                        )
                    else:
                        raise
                return
            if path == "/api/episode-options":
                slug = query.get("slug", [""])[0]
                episode = query.get("episode", [""])[0]
                self.send_json(backend.available_options(slug, episode))
                return
            if path == "/api/status":
                self.send_json(RUNNER.snapshot(include_logs=False))
                return
            if path == "/api/logs":
                after = int(query.get("after", ["0"])[0] or 0)
                self.send_json({"logs": RUNNER.logs_since(after)})
                return
            if path == "/api/events":
                self.serve_events()
                return
            if path == "/api/library":
                self.send_json({"items": library.scan()})
                return
        except backend.BackendError as exc:
            self.send_error_json(str(exc), 502)
            return
        except ConfigError as exc:
            self.send_error_json(str(exc), 500)
            return
        except ValueError as exc:
            self.send_error_json(str(exc), 400)
            return
        except Exception as exc:  # pragma: no cover - defensive
            self.send_error_json(f"Unexpected server error: {exc}", 500)
            return

        self.send_error_json("Not found.", 404)

    def do_HEAD(self) -> None:  # noqa: N802 - http.server API
        self.do_GET()

    # ----------------------------------------------------------------- POST

    def do_POST(self) -> None:  # noqa: N802 - http.server API
        if not self.check_host():
            return
        parsed = urllib.parse.urlsplit(self.path)
        path = parsed.path

        if not self.check_token():
            return

        try:
            data = self.read_body()
        except ValueError as exc:
            self.send_error_json(str(exc), 400)
            return

        try:
            if path == "/api/config":
                self.send_json(public_config(write_config(data)))
                return
            if path == "/api/run":
                job = RUNNER.start(data)
                self.send_json({"ok": True, "job": job})
                return
            if path == "/api/stop":
                stopped = RUNNER.stop()
                self.send_json({"ok": True, "stopped": stopped})
                return
            if path == "/api/clear-logs":
                RUNNER.clear_logs()
                self.send_json({"ok": True})
                return
            if path == "/api/library/delete":
                self.send_json({"ok": True, **library.delete_file(data.get("path", ""))})
                return
            if path == "/api/library/delete-folder":
                self.send_json({"ok": True, **library.delete_folder(data.get("path", ""))})
                return
            if path == "/api/open":
                self.send_json({"ok": True, **library.open_path(data.get("path", ""))})
                return
            if path == "/api/test-connection":
                self.send_json(test_connection())
                return
            if path == "/api/pick-folder":
                current = str(data.get("current") or "").strip()
                if not current:
                    current = public_config(read_config())["downloadDir"]
                selected = pick_folder(current)
                if selected is None:
                    self.send_json({"ok": True, "cancelled": True})
                else:
                    self.send_json({"ok": True, "path": selected})
                return
        except FolderPickerError as exc:
            self.send_error_json(str(exc), 501)
            return
        except ValueError as exc:
            self.send_error_json(str(exc), 400)
            return
        except RunnerError as exc:
            self.send_error_json(str(exc), 409)
            return
        except library.LibraryError as exc:
            self.send_error_json(str(exc), 400)
            return
        except ConfigError as exc:
            self.send_error_json(str(exc), 500)
            return
        except backend.BackendError as exc:
            self.send_error_json(str(exc), 502)
            return
        except Exception as exc:  # pragma: no cover - defensive
            self.send_error_json(f"Unexpected server error: {exc}", 500)
            return

        self.send_error_json("Not found.", 404)

    # -------------------------------------------------------------- serving

    def serve_index(self) -> None:
        try:
            html = (STATIC_DIR / "index.html").read_text(encoding="utf-8")
        except OSError:
            self.send_error_json("index.html is missing from static/.", 500)
            return
        html = html.replace("__APP_TOKEN__", TOKEN)
        self._send_bytes(html.encode("utf-8"), "text/html; charset=utf-8")

    def serve_static(self, relative: str) -> None:
        candidate = (STATIC_DIR / relative).resolve()
        try:
            static_root = STATIC_DIR.resolve()
        except OSError:
            self.send_error_json("Not found.", 404)
            return
        if static_root != candidate and static_root not in candidate.parents:
            self.send_error_json("Not found.", 404)
            return
        if not candidate.is_file():
            self.send_error_json("Not found.", 404)
            return
        content_type = mimetypes.guess_type(candidate.name)[0] or "application/octet-stream"
        if content_type.startswith("text/") or content_type in (
            "application/javascript",
            "application/json",
        ):
            content_type += "; charset=utf-8"
        self._send_bytes(candidate.read_bytes(), content_type)

    def serve_events(self) -> None:
        """Stream log entries and job state as Server-Sent Events.

        The connection intentionally stays open after a job finishes: the same
        browser tab starts the next job without reconnecting (and therefore
        without losing the first few log lines or missing a done event).
        """
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.send_header("X-Accel-Buffering", "no")
        self.end_headers()

        query = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
        last_seq = 0
        for candidate in (
            self.headers.get("Last-Event-ID"),
            query.get("after", [""])[0],
        ):
            if not candidate:
                continue
            try:
                last_seq = max(0, int(candidate))
                break
            except (TypeError, ValueError):
                continue

        last_snapshot: str | None = None
        last_write = time.monotonic()
        try:
            self.wfile.write(b"retry: 3000\n\n")
            self.wfile.flush()
            while True:
                wrote = False
                entries = RUNNER.logs_since(last_seq)
                if entries:
                    # A slow client can leave a large backlog. Send it in one
                    # write instead of one syscall/flush per log line.
                    frames = []
                    for entry in entries:
                        last_seq = entry["seq"]
                        frames.append(self._sse_frame("log", entry, entry["seq"]))
                    self._write_sse(*frames)
                    wrote = True
                snapshot = RUNNER.snapshot(include_logs=False)
                marker = json.dumps(
                    {
                        "startedAt": snapshot["startedAt"],
                        "finishedAt": snapshot["finishedAt"],
                        "running": snapshot["running"],
                        "stopped": snapshot["stopped"],
                        "exitCode": snapshot["exitCode"],
                        "progress": snapshot["progress"],
                        "episodes": snapshot["episodes"],
                        "playlists": snapshot["playlists"],
                    },
                    default=str,
                    sort_keys=True,
                )
                if marker != last_snapshot:
                    last_snapshot = marker
                    frames = [self._sse_frame("state", snapshot)]
                    if not snapshot["running"] and snapshot["exitCode"] is not None:
                        frames.append(self._sse_frame("done", snapshot))
                    self._write_sse(*frames)
                    wrote = True
                if wrote:
                    last_write = time.monotonic()
                elif time.monotonic() - last_write >= SSE_HEARTBEAT_SECONDS:
                    self.wfile.write(b": keepalive\n\n")
                    self.wfile.flush()
                    last_write = time.monotonic()
                time.sleep(0.3)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def _sse_frame(self, event: str, payload: Any, event_id: int | None = None) -> bytes:
        data = json.dumps(payload, default=str)
        lines = []
        if event_id is not None:
            lines.append(f"id: {event_id}")
        lines.append(f"event: {event}")
        lines.append(f"data: {data}")
        return ("\n".join(lines) + "\n\n").encode("utf-8")

    def _write_sse(self, *frames: bytes) -> None:
        if frames:
            self.wfile.write(b"".join(frames))
            self.wfile.flush()


def test_connection() -> dict[str, Any]:
    """Check that the configured credentials can talk to animepahe.pw."""
    started = time.time()
    try:
        items = backend.search("one piece")
    except backend.BackendError as exc:
        return {"ok": False, "message": str(exc), "elapsed": round(time.time() - started, 2)}
    return {
        "ok": True,
        "message": f"Connection OK — {len(items)} search result(s).",
        "elapsed": round(time.time() - started, 2),
    }


def main() -> None:
    """Start the local web server."""
    port = int(os.environ.get("ANIMEPAHE_GUI_PORT") or DEFAULT_PORT)
    server = GuiHTTPServer((HOST, port), Handler)
    url = f"http://{HOST}:{port}"

    print(f"AnimePahe GUI running at {url}")
    print("Press Ctrl+C to stop.")
    if os.environ.get("ANIMEPAHE_GUI_NO_BROWSER") != "1":
        threading.Thread(target=lambda: (time.sleep(0.8), webbrowser.open(url)), daemon=True).start()

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nStopping…")
    finally:
        RUNNER.stop()
        server.server_close()


if __name__ == "__main__":
    sys.exit(main())
