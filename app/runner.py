"""Runs the vendored animepahe-dl.sh backend and streams its output."""

from __future__ import annotations

import codecs
import os
import re
import signal
import subprocess
import threading
import time
from typing import Any

from .config import BASE_DIR, clamp_fragments, ensure_download_dir, read_config
from . import backend

SCRIPT_PATH = BASE_DIR / "animepahe-dl.sh"

ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
SECRET_RE = re.compile(
    r"(?i)\b(cf_clearance|cf_clearance_token|x-app-token)(\s*=\s*|:\s*)([^\s\"'&;]+)"
)
LEVEL_RE = re.compile(r"^\[(INFO|WARNING|ERROR|DEBUG)\]\s*(.*)$")
DEST_RE = re.compile(r"^\[download\]\s+Destination:\s+(.*)$")
PROGRESS_RE = re.compile(
    r"^\[download\]\s+(?P<pct>\d+(?:\.\d+)?)%\s+of\s+(?P<size>[^\s]+)"
    r"(?:\s+at\s+(?P<speed>[^\s]+))?(?:\s+ETA\s+(?P<eta>[^\s]+))?"
)
M3U8_RE = re.compile(r"https?://[^\s\"'<>]+\.m3u8[^\s\"'<>]*")
EPISODE_INFO_RE = re.compile(r"^Downloading Episode\s+(.+?)\.{3}")
EPISODE_WARN_RE = re.compile(r"Episode\s+(\S+)\s+not found")
EPISODE_FAIL_RE = re.compile(r"^Episode\s+(\S+)\s+failed\b")
EPISODE_EXISTS_RE = re.compile(r"^Episode\s+(\S+)\s+already exists\b")

LEVEL_STYLE = {
    "info": "info",
    "warn": "warn",
    "error": "error",
    "debug": "debug",
    "progress": "progress",
    "plain": "plain",
}


class RunnerError(RuntimeError):
    """Raised when a job cannot be started."""


class Runner:
    """Owns the lifecycle of a single animepahe-dl.sh invocation."""

    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._proc: subprocess.Popen | None = None
        self._thread: threading.Thread | None = None
        self._seq = 0
        self._logs: list[dict[str, Any]] = []
        self._state: dict[str, Any] = self._empty_state()
        self._stopping = False

    # ------------------------------------------------------------------ state

    @staticmethod
    def _empty_state() -> dict[str, Any]:
        return {
            "running": False,
            "exitCode": None,
            "startedAt": None,
            "finishedAt": None,
            "args": [],
            "command": "",
            "anime": {"slug": "", "title": "", "name": ""},
            "episodes": {
                "requested": [],
                "current": None,
                "completed": [],
                "failed": [],
            },
            "progress": {
                "percent": None,
                "size": None,
                "speed": None,
                "eta": None,
                "file": None,
            },
            "playlists": [],
            "listOnly": False,
            "stopped": False,
        }

    def snapshot(self, *, include_logs: bool = True) -> dict[str, Any]:
        """Return a copy of the current job state.

        HTTP state endpoints pass ``include_logs=False`` because the event
        stream (or /api/logs) already delivers the entries; repeating them on
        every state poll makes the payload needlessly large.
        """
        with self._lock:
            state = json_copy(self._state)
            if include_logs:
                state["logs"] = list(self._logs[-500:])
            state["logSeq"] = self._seq
            return state

    def logs_since(self, seq: int) -> list[dict[str, Any]]:
        """Return log entries newer than the given sequence number."""
        with self._lock:
            return [entry for entry in self._logs if entry["seq"] > seq]

    # ---------------------------------------------------------------- control

    def start(self, spec: dict[str, Any]) -> dict[str, Any]:
        """Start a download job. Raises RunnerError if one is already running."""
        with self._lock:
            if self._state["running"]:
                raise RunnerError("A job is already running — stop it first.")
            if not SCRIPT_PATH.exists():
                raise RunnerError(f"Backend script missing: {SCRIPT_PATH}")

            slug = str(spec.get("slug", "")).strip()
            title = str(spec.get("title", "")).strip()
            episodes = str(spec.get("episodes", "")).strip() or "*"
            resolution = str(spec.get("resolution", "")).strip()
            audio = str(spec.get("audio", "")).strip()
            replace = bool(spec.get("replace"))
            list_only = bool(spec.get("listOnly"))
            debug = bool(spec.get("debug"))

            if not slug:
                raise RunnerError("No anime selected.")

            # Read settings from config.json once, falling back to safe defaults.
            try:
                config = read_config()
            except Exception:
                config = {}
            fragment_value = spec.get("concurrent_fragments")
            if fragment_value is None or fragment_value == "":
                fragment_value = config.get("concurrent_fragments", 8)
            concurrent_fragments = clamp_fragments(fragment_value)

            # Create the destination up front so a bad path is rejected before
            # the backend process starts.
            try:
                output_dir = ensure_download_dir(
                    str(config.get("download_dir") or BASE_DIR)
                )
            except ValueError as exc:
                raise RunnerError(str(exc)) from exc

            # The bash backend looks the slug up in anime.list to build the
            # output folder name, so make sure it is present.
            backend.ensure_entry(slug, title or backend.title_for_slug(slug))
            resolved_title = title or backend.title_for_slug(slug)

            args: list[str] = ["-s", slug, "-e", episodes]
            if resolution:
                args += ["-r", resolution]
            if audio:
                args += ["-o", audio]
            if replace:
                args.append("-R")
            if list_only:
                args.append("-l")
            if debug:
                args.append("-d")

            self._state = self._empty_state()
            self._state.update(
                {
                    "running": True,
                    "startedAt": time.time(),
                    "args": args,
                    "command": " ".join(["./animepahe-dl.sh", *args]),
                    "anime": {"slug": slug, "title": resolved_title, "name": resolved_title},
                    "episodes": {
                        "requested": split_episodes(episodes),
                        "current": None,
                        "completed": [],
                        "failed": [],
                    },
                    "listOnly": list_only,
                    "stopped": False,
                }
            )
            self._logs = []
            # Never rewind _seq: SSE clients use Last-Event-ID and must not
            # skip the first lines of a new job because the counter restarted.
            self._stopping = False

            env = dict(os.environ)
            env["PYTHONUNBUFFERED"] = "1"
            env["YTDLP_CONCURRENT_FRAGMENTS"] = str(concurrent_fragments)
            env["ANIMEPAHE_OUTPUT_DIR"] = str(output_dir.resolve())
            tool_bin = BASE_DIR / ".uv" / "bin"
            if tool_bin.is_dir():
                env["PATH"] = str(tool_bin) + os.pathsep + env.get("PATH", "")

            try:
                proc = subprocess.Popen(
                    [str(SCRIPT_PATH), *args],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    stdin=subprocess.DEVNULL,
                    cwd=str(BASE_DIR),
                    env=env,
                    bufsize=0,
                    start_new_session=True,
                )
            except OSError as exc:
                self._state["running"] = False
                raise RunnerError(f"Cannot start backend: {exc}") from exc

            self._proc = proc
            self._thread = threading.Thread(target=self._read_loop, daemon=True)
            self._thread.start()
            return self.snapshot()

    def stop(self) -> bool:
        """Terminate the running job (and its process group)."""
        with self._lock:
            proc = self._proc
            running = self._state["running"]
        if proc is None:
            return False
        if proc.poll() is not None:
            return False
        with self._lock:
            self._stopping = True
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except (ProcessLookupError, PermissionError, OSError):
            proc.terminate()
        deadline = time.time() + 6
        while time.time() < deadline and proc.poll() is None:
            time.sleep(0.1)
        if proc.poll() is None:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                proc.kill()
        return running

    # ------------------------------------------------------------- log stream

    def _read_loop(self) -> None:
        """Read the child's output, splitting on newlines and carriage returns.

        yt-dlp updates its progress bar with \r, so a plain readline() would
        hide every update until the episode finishes.
        """
        proc = self._proc
        if proc is None or proc.stdout is None:
            return
        decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        buffer = ""
        try:
            fd = proc.stdout.fileno()
            while True:
                chunk = os.read(fd, 65536)
                if not chunk:
                    break
                buffer += decoder.decode(chunk)
                buffer = self._flush_buffer(buffer)
            buffer += decoder.decode(b"", final=True)
            buffer = self._flush_buffer(buffer)
            if buffer.strip():
                self._handle_line(buffer.rstrip("\r\n"))
        except (OSError, ValueError):
            pass
        finally:
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
            self._finalize(proc.returncode)

    def _flush_buffer(self, buffer: str) -> str:
        """Emit complete lines, keeping the trailing partial line in the buffer."""
        while True:
            newline = buffer.find("\n")
            carriage = buffer.find("\r")
            if newline < 0 and carriage < 0:
                return buffer
            if newline < 0 or (0 <= carriage < newline):
                line = buffer[:carriage]
                buffer = buffer[carriage + 1 :]
            else:
                line = buffer[:newline]
                buffer = buffer[newline + 1 :]
            self._handle_line(line.rstrip())

    def _handle_line(self, raw: str) -> None:
        """Classify one output line, update the job state and store the entry."""
        if raw is None:
            return
        text = redact_secrets(ANSI_RE.sub("", raw).rstrip())
        if not text.strip():
            return

        level = "plain"
        body = text

        match = LEVEL_RE.match(text)
        if match:
            level = match.group(1).lower()
            level = "warn" if level == "warning" else level
            body = match.group(2)
        elif text.startswith("+ "):
            level = "debug"
        elif PROGRESS_RE.match(text):
            level = "progress"
        elif DEST_RE.match(text):
            level = "progress"
        elif "[download]" in text or "[Merger]" in text or "[Fixup]" in text:
            level = "progress"

        self._update_state(level, body or text)

        with self._lock:
            self._seq += 1
            entry = {
                "seq": self._seq,
                "ts": time.time(),
                "level": LEVEL_STYLE.get(level, "plain"),
                "text": body or text,
            }
            self._logs.append(entry)
            if len(self._logs) > 2000:
                del self._logs[:1000]

    def _update_state(self, level: str, text: str) -> None:
        """Fold one output line into the progress counters."""
        with self._lock:
            state = self._state
            progress = state["progress"]
            episodes = state["episodes"]

            playlist = M3U8_RE.search(text)
            if playlist:
                url = playlist.group(0)
                if url not in state["playlists"]:
                    state["playlists"].append(url)
                    if state["listOnly"] and episodes["requested"]:
                        index = len(state["playlists"]) - 1
                        if index < len(episodes["requested"]):
                            episodes["current"] = episodes["requested"][index]

            info = EPISODE_INFO_RE.match(text)
            if info:
                episodes["current"] = info.group(1).strip()
                progress["percent"] = None
                progress["speed"] = None
                progress["eta"] = None
                progress["file"] = None
                return

            missing = EPISODE_WARN_RE.search(text)
            if missing:
                value = missing.group(1)
                if value and value not in episodes["failed"]:
                    episodes["failed"].append(value)
                return

            failed = EPISODE_FAIL_RE.search(text)
            if failed:
                value = failed.group(1)
                if value and value not in episodes["failed"]:
                    episodes["failed"].append(value)
                return

            exists = EPISODE_EXISTS_RE.search(text)
            if exists:
                value = exists.group(1)
                if value:
                    episodes["current"] = value
                    if value not in episodes["completed"]:
                        episodes["completed"].append(value)
                progress["percent"] = None
                progress["speed"] = None
                progress["eta"] = None
                return

            dest = DEST_RE.match(text)
            if dest:
                progress["file"] = dest.group(1).strip()
                return

            match = PROGRESS_RE.match(text)
            if match:
                progress["percent"] = float(match.group("pct"))
                progress["size"] = match.group("size")
                progress["speed"] = match.group("speed")
                progress["eta"] = match.group("eta")
                if match.group("pct").startswith("100"):
                    current = episodes.get("current")
                    if current and current not in episodes["completed"]:
                        episodes["completed"].append(current)
                    progress["percent"] = None
                return

            if "Merging formats" in text or "Merging" in text:
                progress["percent"] = 100

    def _finalize(self, exit_code: int | None) -> None:
        """Mark the job as finished."""
        with self._lock:
            self._state["running"] = False
            self._state["exitCode"] = exit_code
            self._state["finishedAt"] = time.time()
            self._state["progress"]["percent"] = None
            self._state["stopped"] = bool(self._stopping)
            self._proc = None
        self._handle_line(
            f"[INFO] Job finished with exit code {exit_code}"
            if exit_code == 0
            else f"[ERROR] Job failed with exit code {exit_code}"
        )

    def clear_logs(self) -> None:
        """Drop the stored log entries (does not touch a running job).

        The sequence counter stays where it is so a connected SSE client can
        still receive the next job's entries.
        """
        with self._lock:
            self._logs = []


def redact_secrets(text: str) -> str:
    """Remove credential values from backend output before it is logged."""
    return SECRET_RE.sub(lambda match: f"{match.group(1)}{match.group(2)}<redacted>", text)


def json_copy(value: Any) -> Any:
    """Deep-copy plain JSON data structures."""
    if isinstance(value, dict):
        return {key: json_copy(item) for key, item in value.items()}
    if isinstance(value, list):
        return [json_copy(item) for item in value]
    return value


def split_episodes(spec: str) -> list[str]:
    """Expand an episode spec ("1,3,5-7" or "*") into a list of episode ids."""
    spec = spec.strip()
    if not spec or spec == "*":
        return []
    result: list[str] = []
    for chunk in spec.split(","):
        chunk = chunk.strip()
        if not chunk:
            continue
        if "-" in chunk:
            start, _, end = chunk.partition("-")
            try:
                first = int(start)
                last = int(end)
            except ValueError:
                continue
            if first > last:
                first, last = last, first
            result.extend(str(n) for n in range(first, last + 1))
        else:
            result.append(chunk)
    return result


def compress_episodes(values: list[str]) -> str:
    """Compress a list of episode numbers into ranges: [1,2,3,5] -> "1-3,5"."""
    numbers: list[int] = []
    for value in values:
        try:
            numbers.append(int(value))
        except (TypeError, ValueError):
            continue
    if not numbers:
        return ""
    numbers = sorted(set(numbers))
    parts: list[str] = []
    start = previous = numbers[0]
    for number in numbers[1:]:
        if number == previous + 1:
            previous = number
            continue
        parts.append(str(start) if start == previous else f"{start}-{previous}")
        start = previous = number
    parts.append(str(start) if start == previous else f"{start}-{previous}")
    return ",".join(parts)