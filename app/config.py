"""Persistent configuration handling (config.json)."""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

BASE_DIR = Path(__file__).resolve().parent.parent
CONFIG_PATH = BASE_DIR / "config.json"

DEFAULT_USER_AGENT = (
    "Mozilla/5.0 (X11; Linux x86_64; rv:158.0) Gecko/20100101 Firefox/158.0"
)

MIN_FRAGMENTS = 1
MAX_FRAGMENTS = 64
DEFAULT_FRAGMENTS = 8


class ConfigError(RuntimeError):
    """Raised when config.json cannot be read or written."""


def clamp_fragments(value: Any) -> int:
    """Coerce a concurrent-fragment count into the supported range."""
    try:
        number = int(value)
    except (TypeError, ValueError):
        return DEFAULT_FRAGMENTS
    return max(MIN_FRAGMENTS, min(MAX_FRAGMENTS, number))


def normalize_download_dir(value: Any) -> str:
    """Return an absolute download folder path, rejecting the filesystem root."""
    text = str(value or "").strip()
    if not text:
        return str(BASE_DIR.resolve())
    path = Path(text).expanduser()
    if not path.is_absolute():
        path = BASE_DIR / path
    try:
        resolved = path.resolve()
    except OSError as exc:
        raise ValueError(f"Cannot resolve download folder: {exc}") from exc
    if resolved == Path(resolved.anchor):
        raise ValueError("The download folder cannot be the filesystem root.")
    return str(resolved)


def ensure_download_dir(value: str) -> Path:
    """Create the download folder if needed and make sure it is a writable directory."""
    path = Path(value)
    try:
        if path.exists() and not path.is_dir():
            raise ValueError("The download folder must be a directory.")
        path.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        raise ValueError(f"Cannot create the download folder: {exc}") from exc
    if not os.access(path, os.W_OK):
        raise ValueError(f"The download folder is not writable: {path}")
    return path


def read_config() -> dict[str, Any]:
    """Return the current configuration, falling back to defaults."""
    try:
        raw = CONFIG_PATH.read_text(encoding="utf-8")
    except FileNotFoundError:
        return {
            "cf": "",
            "ua": DEFAULT_USER_AGENT,
            "concurrent_fragments": DEFAULT_FRAGMENTS,
            "download_dir": str(BASE_DIR.resolve()),
        }
    except OSError as exc:
        raise ConfigError(f"Cannot read {CONFIG_PATH.name}: {exc}") from exc

    try:
        data = json.loads(raw or "{}")
    except json.JSONDecodeError as exc:
        raise ConfigError(f"{CONFIG_PATH.name} is not valid JSON: {exc}") from exc
    if not isinstance(data, dict):
        raise ConfigError(f"{CONFIG_PATH.name} must contain a JSON object")

    try:
        download_dir = normalize_download_dir(data.get("download_dir", ""))
    except (OSError, ValueError):
        # A hand-edited config must not make the whole UI unusable. Fall back
        # to the project directory and let the Settings page show the default.
        download_dir = str(BASE_DIR.resolve())

    return {
        "cf": str(data.get("cf", "") or "").strip(),
        "ua": str(data.get("ua", "") or DEFAULT_USER_AGENT).strip(),
        "concurrent_fragments": clamp_fragments(
            data.get("concurrent_fragments", DEFAULT_FRAGMENTS)
        ),
        "download_dir": download_dir,
    }


def _write_private(path: Path, text: str) -> None:
    """Write a file that only the current user can read."""
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(text)
    except Exception:
        try:
            os.close(fd)
        except OSError:
            pass
        raise
    os.chmod(path, 0o600)


def write_config(changes: dict[str, Any]) -> dict[str, Any]:
    """Persist the configuration atomically, keeping a .bak of the previous file.

    A missing ``cf`` key (or an explicit ``None``) keeps the stored cookie, so
    the browser never has to receive the secret just to save another setting.
    An empty string still clears the cookie.
    """
    current = read_config()

    cf = current["cf"]
    if "cf" in changes and changes["cf"] is not None:
        cf = str(changes["cf"] or "").strip()

    ua = current["ua"]
    if "ua" in changes and changes["ua"] is not None:
        ua = str(changes["ua"] or current["ua"]).strip()

    download_dir = current["download_dir"]
    if "download_dir" in changes and changes["download_dir"] is not None:
        download_dir = normalize_download_dir(changes["download_dir"])
        ensure_download_dir(download_dir)

    payload = {
        "cf": cf,
        "ua": ua,
        "concurrent_fragments": clamp_fragments(
            changes.get("concurrent_fragments", current["concurrent_fragments"])
        ),
        "download_dir": download_dir,
    }

    text = json.dumps(payload, indent=2) + "\n"
    tmp = CONFIG_PATH.with_suffix(".json.tmp")
    backup = CONFIG_PATH.with_suffix(".json.bak")
    try:
        _write_private(tmp, text)
        if CONFIG_PATH.exists():
            _write_private(backup, CONFIG_PATH.read_text(encoding="utf-8"))
        os.replace(tmp, CONFIG_PATH)
        os.chmod(CONFIG_PATH, 0o600)
    except OSError as exc:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        raise ConfigError(f"Cannot write {CONFIG_PATH.name}: {exc}") from exc
    return payload
