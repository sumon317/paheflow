"""Open a native folder chooser for the local GUI."""

from __future__ import annotations

import os
import shutil
import subprocess
import threading
from pathlib import Path

TITLE = "Choose where to save downloaded anime"
_PICK_LOCK = threading.Lock()


class FolderPickerError(RuntimeError):
    """Raised when no system folder dialog can be opened."""


def _display_available() -> bool:
    """Return True when a graphical session can show a dialog."""
    return bool(os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY"))


def _start_directory(initial: str) -> Path:
    """Pick a sensible existing folder for the dialog to open in."""
    if initial:
        path = Path(initial).expanduser()
        if not path.is_absolute():
            path = Path.cwd() / path
        try:
            resolved = path.resolve()
        except OSError:
            resolved = path
        if resolved.is_dir():
            return resolved
        if resolved.parent.is_dir():
            return resolved.parent
    return Path.home()


def _selected_path(value: str | None) -> str | None:
    if value is None:
        return None
    text = value.strip()
    if not text:
        return None
    try:
        return str(Path(text).expanduser().resolve())
    except OSError:
        return text


def _run_dialog(command: list[str]) -> str | None:
    try:
        result = subprocess.run(
            command,
            capture_output=True,
            text=True,
            check=False,
            timeout=None,
        )
    except OSError as exc:
        raise FolderPickerError(f"Could not open folder picker: {exc}") from exc
    if result.returncode == 1:
        return None
    if result.returncode != 0:
        message = (result.stderr or result.stdout or "Folder picker failed.").strip()
        raise FolderPickerError(message)
    return _selected_path(result.stdout)


def _tkinter_pick(start: Path) -> str | None:
    """Fallback dialog when neither zenity nor kdialog is installed."""
    if not _display_available():
        raise FolderPickerError("No graphical session is available for a folder picker.")
    try:
        import tkinter as tk
        from tkinter import filedialog
    except ImportError as exc:  # pragma: no cover - depends on the Python build
        raise FolderPickerError("Tkinter is not installed.") from exc

    try:
        root = tk.Tk()
        root.withdraw()
        root.attributes("-topmost", True)
        value = filedialog.askdirectory(
            parent=root,
            initialdir=str(start),
            title=TITLE,
            mustexist=True,
        )
        root.destroy()
    except Exception as exc:  # pragma: no cover - depends on the desktop session
        raise FolderPickerError(f"Could not open folder picker: {exc}") from exc
    return _selected_path(value)


def pick_folder(initial: str = "") -> str | None:
    """Return an absolute folder path, or ``None`` when the user cancels."""
    start = _start_directory(initial)
    errors: list[str] = []

    with _PICK_LOCK:
        zenity = shutil.which("zenity")
        if zenity:
            try:
                return _run_dialog(
                    [
                        zenity,
                        "--file-selection",
                        "--directory",
                        f"--title={TITLE}",
                        f"--filename={start}{os.sep}",
                    ]
                )
            except FolderPickerError as exc:
                errors.append(str(exc))

        kdialog = shutil.which("kdialog")
        if kdialog:
            try:
                return _run_dialog(
                    [kdialog, "--getexistingdirectory", str(start), "--title", TITLE]
                )
            except FolderPickerError as exc:
                errors.append(str(exc))

        if _display_available():
            try:
                return _tkinter_pick(start)
            except FolderPickerError as exc:
                errors.append(str(exc))

    detail = f" ({errors[-1]})" if errors else ""
    raise FolderPickerError(
        "No folder picker is available. Install zenity or kdialog, or type the path manually."
        + detail
    )
