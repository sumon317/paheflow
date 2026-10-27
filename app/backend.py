"""Client for the animepahe.pw API plus the local slug catalog (anime.list)."""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import threading
import time
import urllib.parse
from pathlib import Path
from typing import Any

from .config import BASE_DIR, read_config

HOST = "https://animepahe.pw"
API_URL = f"{HOST}/api"
ANIME_LIST_PATH = BASE_DIR / "anime.list"
CACHE_DIR = BASE_DIR / "cache"

REQUEST_TIMEOUT = 30
# animepahe.pw sits behind Cloudflare, which starts answering "error code: 1015"
# (rate limited) once several requests arrive back to back.
MIN_REQUEST_INTERVAL = 0.4
MAX_ATTEMPTS = 5
RATE_LIMIT_MARKER = "error code: 1015"

_REQUEST_LOCK = threading.Lock()
_LAST_REQUEST = 0.0

LIST_LINE_RE = re.compile(r"^\[([^\]]+)\]\s*(.*?)\s*$")
ANCHOR_RE = re.compile(r'<a\s+href="/anime/([0-9a-fA-F-]+)"[^>]*title="([^"]*)"')
BUTTON_RE = re.compile(r"<button\b[^>]*>", re.IGNORECASE)
ATTR_RE = re.compile(r'data-(src|resolution|audio|av1)\s*=\s*"([^"]*)"', re.IGNORECASE)


class BackendError(RuntimeError):
    """Raised when a request to animepahe.pw fails."""


def _curl_binary() -> str:
    """Return the best curl-compatible binary for Cloudflare requests.

    A real ``curl-impersonate`` is preferred.  The project-local
    ``animepahe-curl`` shim (curl_cffi) is the supported fallback, so users no
    longer need to build curl-impersonate themselves.  Plain curl is only a
    last resort to keep error handling graceful when setup is incomplete.
    """
    local_bin = BASE_DIR / ".uv" / "bin"

    def local(name: str) -> str | None:
        candidate = local_bin / name
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
        return None

    for name in ("curl-impersonate", "animepahe-curl"):
        path = shutil.which(name) or local(name)
        if path:
            return path
    return shutil.which("curl") or "curl"


def _throttle() -> None:
    """Keep a minimum gap between requests to stay below the rate limit."""
    global _LAST_REQUEST
    with _REQUEST_LOCK:
        wait = MIN_REQUEST_INTERVAL - (time.monotonic() - _LAST_REQUEST)
        if wait > 0:
            time.sleep(wait)
        _LAST_REQUEST = time.monotonic()


def _fetch(url: str, *, referer: str | None = None) -> str:
    """Fetch a URL through curl-impersonate/animepahe-curl with the cookie.

    Retries with a growing delay when Cloudflare rate-limits the request.
    """
    cfg = read_config()
    cmd = [
        _curl_binary(),
        "-sS",
        "-L",
        "--compressed",
        "--max-time",
        str(REQUEST_TIMEOUT),
        url,
        "-b",
        f"cf_clearance={cfg['cf']}",
        "-A",
        cfg["ua"],
    ]
    if referer:
        cmd[1:1] = ["-H", f"Referer: {referer}"]

    last_error = "Request to animepahe.pw failed."
    for attempt in range(MAX_ATTEMPTS):
        _throttle()
        try:
            result = subprocess.run(
                cmd, capture_output=True, text=True, timeout=REQUEST_TIMEOUT + 15
            )
        except subprocess.TimeoutExpired:
            last_error = "Request to animepahe.pw timed out."
            time.sleep(1.0 * (attempt + 1))
            continue
        if result.returncode != 0:
            last_error = result.stderr.strip() or "HTTP client failed."
            time.sleep(1.0 * (attempt + 1))
            continue
        body = result.stdout
        if RATE_LIMIT_MARKER in body:
            last_error = (
                "animepahe.pw rate-limited the request (Cloudflare error 1015). "
                "Try again in a moment."
            )
            time.sleep(1.5 * (attempt + 1))
            continue
        return body

    raise BackendError(last_error)


def _fetch_json(url: str) -> dict[str, Any]:
    import json

    body = _fetch(url)
    try:
        data = json.loads(body)
    except json.JSONDecodeError as exc:
        head = body[:2000].lower()
        if "just a moment" in head or "cf-browser-verification" in head:
            raise BackendError(
                "Cloudflare is challenging this request (HTTP 403 challenge page). "
                "Wait a minute and retry — if it keeps happening, copy a fresh "
                "cf_clearance cookie and User-Agent into Settings."
            ) from exc
        if "<html" in head:
            raise BackendError(
                "animepahe.pw returned an HTML page instead of data — the cf_clearance "
                "cookie is probably expired. Update it in Settings."
            ) from exc
        raise BackendError(
            "animepahe.pw returned an unexpected response — the cf_clearance cookie is "
            "probably expired. Update it in Settings."
        ) from exc
    if not isinstance(data, dict):
        raise BackendError("Unexpected response shape from animepahe.pw")
    return data


def search(query: str) -> list[dict[str, Any]]:
    """Search animepahe.pw by name."""
    query = query.strip()
    if not query:
        return []
    url = f"{API_URL}?m=search&q={urllib.parse.quote(query)}"
    data = _fetch_json(url)
    results = []
    for item in data.get("data") or []:
        if not isinstance(item, dict):
            continue
        results.append(
            {
                "slug": item.get("session", ""),
                "title": item.get("title", ""),
                "type": item.get("type", ""),
                "episodes": item.get("episodes", ""),
                "status": item.get("status", ""),
                "score": item.get("score", ""),
                "year": item.get("year", ""),
                "poster": item.get("poster", ""),
                "cover": item.get("cover", ""),
            }
        )
    return results


def release_page(slug: str, page: int = 1) -> tuple[list[dict[str, Any]], int]:
    """Return one page of episodes plus the total number of pages."""
    slug = slug.strip()
    if not slug:
        return [], 1
    url = (
        f"{API_URL}?m=release&id={urllib.parse.quote(slug)}"
        f"&sort=episode_asc&page={max(1, int(page))}"
    )
    data = _fetch_json(url)
    try:
        last_page = int(data.get("last_page") or 1)
    except (TypeError, ValueError):
        last_page = 1
    return [_episode_item(item) for item in (data.get("data") or [])], last_page


def _episode_item(item: dict[str, Any]) -> dict[str, Any]:
    """Normalise one release entry."""
    return {
        "episode": item.get("episode", ""),
        "audio": item.get("audio", ""),
        "created_at": item.get("created_at", ""),
        "duration": item.get("duration", ""),
        "session": item.get("session", ""),
        "episode2": item.get("episode2", ""),
    }


_RELEASE_CACHE: dict[str, tuple[float, list[dict[str, Any]]]] = {}
_RELEASE_TTL = 900.0


def _cache_file(slug: str) -> Path:
    safe = "".join(ch for ch in slug if ch.isalnum() or ch in "-_")
    return CACHE_DIR / f"{safe}.json"


def save_release_cache(slug: str, episodes: list[dict[str, Any]]) -> None:
    """Persist an episode list so it stays available when the site is blocked."""
    try:
        CACHE_DIR.mkdir(exist_ok=True)
        payload = {"slug": slug, "savedAt": time.time(), "items": episodes}
        _cache_file(slug).write_text(json.dumps(payload), encoding="utf-8")
    except OSError:
        pass


def load_release_cache(slug: str) -> list[dict[str, Any]]:
    """Read a previously cached episode list (empty when there is none)."""
    path = _cache_file(slug)
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return []
    items = payload.get("items")
    return items if isinstance(items, list) else []


def release(slug: str, *, use_cache: bool = True) -> list[dict[str, Any]]:
    """Return the full episode list for a slug (all API pages)."""
    slug = slug.strip()
    if not slug:
        return []

    if use_cache:
        cached = _RELEASE_CACHE.get(slug)
        if cached and (time.monotonic() - cached[0]) < _RELEASE_TTL:
            return list(cached[1])

    episodes: list[dict[str, Any]] = []
    seen: set[str] = set()
    page = 1
    last_page = 1
    while page <= last_page:
        items, last_page = release_page(slug, page)
        for item in items:
            session = item.get("session", "")
            if session and session in seen:
                continue
            if session:
                seen.add(session)
            episodes.append(item)
        page += 1

    def sort_key(item: dict[str, Any]):
        try:
            return (0, float(item.get("episode") or 0))
        except (TypeError, ValueError):
            return (1, 0)

    episodes.sort(key=sort_key)
    _RELEASE_CACHE[slug] = (time.monotonic(), episodes)
    save_release_cache(slug, episodes)
    return list(episodes)


def play_sources(slug: str, session: str) -> list[dict[str, Any]]:
    """Return the playable sources of one episode, parsed from its play page."""
    url = f"{HOST}/play/{urllib.parse.quote(slug)}/{urllib.parse.quote(session)}"
    html = _fetch(url)
    sources: list[dict[str, Any]] = []
    for tag in BUTTON_RE.findall(html):
        attrs = {k.lower(): v for k, v in ATTR_RE.findall(tag)}
        src = attrs.get("src")
        if not src:
            continue
        if attrs.get("av1") == "1":
            # The bash backend ignores AV1 sources; keep the GUI consistent.
            continue
        sources.append(
            {
                "url": src,
                "resolution": attrs.get("resolution", ""),
                "audio": attrs.get("audio", ""),
                "av1": attrs.get("av1", "0"),
                "kwik": "kwik" in src,
            }
        )
    return sources


def available_options(slug: str, episode: str) -> dict[str, Any]:
    """Probe which resolutions / audio tracks exist for one episode."""
    episodes = release(slug)
    target = _match_episode(episodes, episode)
    if target is None or not target.get("session"):
        raise BackendError(f"Episode {episode} not found for this anime")

    sources = play_sources(slug, target["session"])
    resolutions = sorted(
        {s["resolution"] for s in sources if s.get("resolution")},
        key=lambda r: int(r) if r.isdigit() else 0,
        reverse=True,
    )
    audios: list[str] = []
    for source in sources:
        value = source.get("audio") or ""
        if value and value not in audios:
            audios.append(value)

    combinations: list[dict[str, str]] = []
    seen_pairs: set[tuple[str, str]] = set()
    for source in sources:
        resolution = str(source.get("resolution") or "")
        audio = str(source.get("audio") or "")
        if not resolution:
            continue
        pair = (resolution, audio)
        if pair in seen_pairs:
            continue
        seen_pairs.add(pair)
        combinations.append({"resolution": resolution, "audio": audio})

    def combination_key(pair: dict[str, str]) -> tuple[int, str]:
        try:
            resolution = int(pair["resolution"])
        except (TypeError, ValueError):
            resolution = 0
        return (-resolution, pair["audio"])

    combinations.sort(key=combination_key)
    return {
        "episode": target.get("episode", ""),
        "session": target.get("session", ""),
        "resolutions": resolutions,
        "audios": audios,
        "combinations": combinations,
        "sourceCount": len(sources),
    }


def _match_episode(
    episodes: list[dict[str, Any]], episode: str
) -> dict[str, Any] | None:
    """Find one episode by number inside a release list."""
    try:
        wanted = float(episode)
    except (TypeError, ValueError):
        return None
    for item in episodes:
        try:
            if float(item.get("episode") or 0) == wanted:
                return item
        except (TypeError, ValueError):
            continue
    return None


def catalog() -> list[dict[str, str]]:
    """Parse the local slug catalog (anime.list)."""
    entries: list[dict[str, str]] = []
    seen: set[str] = set()
    try:
        raw = ANIME_LIST_PATH.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return entries
    for line in raw.splitlines():
        match = LIST_LINE_RE.match(line.strip())
        if not match:
            continue
        slug = match.group(1).strip()
        title = match.group(2).strip()
        if not slug or slug in seen:
            continue
        seen.add(slug)
        entries.append({"slug": slug, "title": title})
    return entries


def refresh_catalog() -> list[dict[str, str]]:
    """Re-download the anime listing from the site and rewrite anime.list."""
    html = _fetch(f"{HOST}/anime")
    entries: list[dict[str, str]] = []
    seen: set[str] = set()
    for slug, title in ANCHOR_RE.findall(html):
        slug = slug.strip()
        title = title.strip()
        if not slug or slug in seen:
            continue
        seen.add(slug)
        entries.append({"slug": slug, "title": title})
    if not entries:
        raise BackendError(
            "Could not read the anime listing — the cf_clearance cookie may be expired."
        )
    try:
        ANIME_LIST_PATH.write_text(
            "".join(f"[{e['slug']}] {e['title']}   \n" for e in entries), encoding="utf-8"
        )
    except OSError as exc:
        raise BackendError(f"Cannot write anime.list: {exc}") from exc
    return entries


def ensure_entry(slug: str, title: str) -> None:
    """Make sure a slug is present in anime.list.

    The bash backend resolves the output folder name by looking the slug up in
    anime.list, so a slug that is missing there makes the run fail.
    """
    slug = slug.strip()
    if not slug:
        return
    try:
        raw = ANIME_LIST_PATH.read_text(encoding="utf-8", errors="replace")
    except OSError:
        raw = ""
    for line in raw.splitlines():
        match = LIST_LINE_RE.match(line.strip())
        if match and match.group(1).strip() == slug:
            return
    title = (title or slug).strip()
    try:
        with ANIME_LIST_PATH.open("a", encoding="utf-8") as handle:
            handle.write(f"[{slug}] {title}   \n")
    except OSError as exc:
        raise BackendError(f"Cannot update anime.list: {exc}") from exc


def title_for_slug(slug: str) -> str:
    """Look up a title in the local catalog."""
    slug = slug.strip()
    for entry in catalog():
        if entry["slug"] == slug:
            return entry["title"]
    return ""
