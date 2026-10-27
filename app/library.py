"""Inspection and maintenance of downloaded episode files."""

from __future__ import annotations

import shutil
import subprocess
import time
from pathlib import Path
from typing import Any

from .config import BASE_DIR, ConfigError, read_config

VIDEO_SUFFIXES = {".mp4", ".mkv", ".webm", ".m4v"}

# These top-level directories belong to the application, not to the library.
# Deleting or treating them as downloaded anime folders would break the GUI.
RESERVED_DIRS = {
    "app",
    "static",
    "tests",
    "cache",
    ".git",
    ".zcode",
    "__pycache__",
}


class LibraryError(RuntimeError):
    """Raised when a library operation is not allowed or fails."""


def download_root() -> Path:
    """Return the configured folder that holds downloaded anime folders."""
    try:
        configured = read_config().get("download_dir") or BASE_DIR
    except (ConfigError, OSError, ValueError):
        configured = BASE_DIR
    path = Path(str(configured)).expanduser()
    if not path.is_absolute():
        path = BASE_DIR / path
    try:
        resolved = path.resolve()
    except OSError:
        return BASE_DIR.resolve()
    if resolved == Path(resolved.anchor):
        return BASE_DIR.resolve()
    return resolved


def _safe_path(candidate: str | Path, root: Path | None = None) -> Path:
    """Resolve a path and make sure it stays inside the download folder."""
    base = (root or download_root()).resolve()
    path = Path(candidate)
    if not path.is_absolute():
        path = base / path
    try:
        resolved = path.resolve()
    except OSError as exc:
        raise LibraryError(f"Cannot resolve path: {exc}") from exc
    if resolved != base and base not in resolved.parents:
        raise LibraryError("Refusing to touch a path outside the download folder.")
    return resolved


def scan() -> list[dict[str, Any]]:
    """Return every downloaded anime folder with its episode files."""
    entries: list[dict[str, Any]] = []
    base = download_root()
    if not base.is_dir():
        return entries
    try:
        children = sorted(base.iterdir())
    except OSError:
        return entries

    for folder in children:
        if not folder.is_dir() or folder.name.startswith("."):
            continue
        if folder.name in RESERVED_DIRS:
            continue
        try:
            files = sorted(folder.iterdir())
        except OSError:
            continue
        videos = [f for f in files if f.is_file() and f.suffix.lower() in VIDEO_SUFFIXES]
        if not videos:
            continue

        episodes = []
        total = 0
        for video in videos:
            try:
                stat = video.stat()
            except OSError:
                continue
            total += stat.st_size
            episodes.append(
                {
                    "file": video.name,
                    "path": str(video),
                    "size": stat.st_size,
                    "sizeText": human_size(stat.st_size),
                    "modified": stat.st_mtime,
                    "modifiedText": time.strftime("%Y-%m-%d %H:%M", time.localtime(stat.st_mtime)),
                }
            )
        episodes.sort(key=lambda e: episode_sort_key(e["file"]))
        entries.append(
            {
                "name": folder.name,
                "path": str(folder),
                "episodes": episodes,
                "count": len(episodes),
                "total": total,
                "totalText": human_size(total),
                "modified": max((e["modified"] for e in episodes), default=folder.stat().st_mtime),
            }
        )

    entries.sort(key=lambda e: e["modified"], reverse=True)
    return entries


def episode_sort_key(filename: str) -> tuple[int, float, str]:
    """Sort "12.mp4" after "2.mp4" instead of lexicographically."""
    stem = Path(filename).stem
    try:
        return (0, float(stem), stem)
    except ValueError:
        return (1, 0.0, stem)


def _check_anime_folder(path: Path, root: Path | None = None) -> None:
    """Require the path's parent to be a real, top-level anime folder."""
    base = (root or download_root()).resolve()
    if path.parent == base:
        raise LibraryError("Refusing to touch files in the project root.")
    if path.parent.parent != base:
        raise LibraryError("Only direct anime folders in the library can be changed.")
    name = path.parent.name
    if name.startswith(".") or name in RESERVED_DIRS:
        raise LibraryError("Refusing to touch an application folder.")


def delete_file(relative_path: str) -> dict[str, Any]:
    """Delete one downloaded file from a top-level anime folder."""
    root = download_root()
    path = _safe_path(relative_path, root)
    if not path.exists():
        raise LibraryError("File does not exist.")
    if path.is_dir():
        raise LibraryError("Refusing to delete a directory.")
    if path.suffix.lower() not in VIDEO_SUFFIXES:
        raise LibraryError("Only downloaded video files can be deleted.")
    _check_anime_folder(path, root)
    size = path.stat().st_size
    try:
        path.unlink()
    except OSError as exc:
        raise LibraryError(f"Cannot delete file: {exc}") from exc
    return {"deleted": path.name, "size": size}


def delete_folder(relative_path: str) -> dict[str, Any]:
    """Delete a whole top-level anime folder including its episodes."""
    base = download_root()
    path = _safe_path(relative_path, base)
    if path == base:
        raise LibraryError("Refusing to delete the download folder itself.")
    if path.parent != base:
        raise LibraryError("Only direct anime folders in the library can be deleted.")
    if path.name.startswith(".") or path.name in RESERVED_DIRS:
        raise LibraryError("Refusing to delete an application folder.")
    if not path.is_dir():
        raise LibraryError("Folder does not exist.")
    try:
        videos = [
            f for f in path.iterdir() if f.is_file() and f.suffix.lower() in VIDEO_SUFFIXES
        ]
    except OSError as exc:
        raise LibraryError(f"Cannot inspect folder: {exc}") from exc
    if not videos:
        raise LibraryError("Refusing to delete a folder without downloaded videos.")
    try:
        shutil.rmtree(path)
    except OSError as exc:
        raise LibraryError(f"Cannot delete folder: {exc}") from exc
    return {"deleted": path.name, "count": len(videos)}


def open_path(relative_path: str) -> dict[str, Any]:
    """Open a file or folder with the desktop's default handler."""
    path = _safe_path(relative_path, download_root())
    if not path.exists():
        raise LibraryError("Path does not exist.")
    opener = shutil.which("xdg-open") or shutil.which("open")
    if not opener:
        raise LibraryError("No file opener available on this system.")
    try:
        subprocess.Popen(
            [opener, str(path)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError as exc:
        raise LibraryError(f"Cannot open path: {exc}") from exc
    return {"opened": str(path)}


def human_size(size: float) -> str:
    """Format a byte count for display."""
    value = float(size)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if value < 1024 or unit == "TB":
            if unit == "B":
                return f"{int(value)} {unit}"
            return f"{value:.1f} {unit}"
        value /= 1024
    return f"{value:.1f} TB"
