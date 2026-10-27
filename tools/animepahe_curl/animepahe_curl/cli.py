"""A small curl-compatible CLI backed by curl_cffi.

The vendored downloader and the local GUI backend only need a narrow subset of
curl's options.  Keeping a tiny shim in the project means users do not have to
build and pin a separate curl-impersonate binary just to talk to Cloudflare.
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass, field
from urllib.parse import urlsplit

from curl_cffi import __version__ as CURL_CFFI_VERSION
from curl_cffi import requests
from curl_cffi.requests.exceptions import Timeout

from . import __version__

DEFAULT_IMPERSONATE = "chrome"
DEFAULT_CONNECT_TIMEOUT = 10.0
DEFAULT_MAX_TIME = 300.0

HELP = """\
Usage: animepahe-curl [options] URL

A small curl-compatible client with browser TLS/HTTP impersonation.

Options:
  -s, --silent                Do not print progress (accepted for compatibility)
  -S, --show-error            Print errors even with --silent
  -L, --location              Follow redirects
      --compressed            Request compressed responses (accepted)
      --fail                  Exit with an error on HTTP 4xx/5xx responses
      --connect-timeout SEC   Connection timeout (default: 10)
      --max-time SEC          Read/request timeout (default: 300)
  -b, --cookie COOKIE         Cookie header value, e.g. "name=value"
  -A, --user-agent UA         User-Agent header
  -H, --header 'Name: value'  Add a request header (repeatable)
  -X, --request METHOD        HTTP method (default: GET, or POST with data)
      --data-raw DATA         Request body (POST unless -X is given)
  -V, --version               Show version information
  -h, --help                  Show this help
"""


class UsageError(ValueError):
    """Raised when the command line cannot be understood."""


@dataclass
class Options:
    url: str = ""
    method: str = ""
    headers: list[tuple[str, str]] = field(default_factory=list)
    cookie: str = ""
    user_agent: str = ""
    data: bytes | None = None
    follow: bool = False
    fail: bool = False
    silent: bool = False
    show_error: bool = False
    compressed: bool = False
    connect_timeout: float = DEFAULT_CONNECT_TIMEOUT
    max_time: float = DEFAULT_MAX_TIME
    show_version: bool = False
    show_help: bool = False


_VALUE_SHORT = {
    "b": "cookie",
    "A": "user_agent",
    "H": "header",
    "X": "method",
    "d": "data",
}
_VALUE_LONG = {
    "--cookie": "cookie",
    "--user-agent": "user_agent",
    "--header": "header",
    "--request": "method",
    "--data": "data",
    "--data-raw": "data_raw",
    "--connect-timeout": "connect_timeout",
    "--max-time": "max_time",
}
_TIMEOUTS = {"connect_timeout", "max_time"}


def _split_header(value: str) -> tuple[str, str]:
    name, separator, body = value.partition(":")
    if not separator or not name.strip():
        raise UsageError(f"invalid header (expected 'Name: value'): {value!r}")
    return name.strip(), body.strip()


def _parse_timeout(value: str, option: str) -> float:
    try:
        number = float(value)
    except ValueError as exc:
        raise UsageError(f"{option} expects a number, got {value!r}") from exc
    if number <= 0:
        raise UsageError(f"{option} must be greater than zero")
    return number


def _set_value(opts: Options, name: str, value: str) -> None:
    if name == "header":
        opts.headers.append(_split_header(value))
    elif name == "data_raw":
        opts.data = value.encode("utf-8")
    elif name == "data":
        opts.data = value.encode("utf-8")
    elif name in _TIMEOUTS:
        setattr(opts, name, _parse_timeout(value, name))
    else:
        setattr(opts, name, value)


def _next_value(argv: list[str], index: int, option: str) -> tuple[str, int]:
    if index + 1 >= len(argv):
        raise UsageError(f"{option} expects a value")
    return argv[index + 1], index + 2


def parse_args(argv: list[str]) -> Options:
    opts = Options()
    index = 0
    while index < len(argv):
        arg = argv[index]
        if arg == "--":
            for value in argv[index + 1 :]:
                if opts.url:
                    raise UsageError("only one URL can be requested")
                opts.url = value
            break
        if arg.startswith("--"):
            name, has_equals, attached = arg.partition("=")
            if name in ("--help",):
                opts.show_help = True
                index += 1
                continue
            if name in ("--version",):
                opts.show_version = True
                index += 1
                continue
            if name in ("--silent",):
                opts.silent = True
                index += 1
                continue
            if name in ("--show-error",):
                opts.show_error = True
                index += 1
                continue
            if name in ("--location",):
                opts.follow = True
                index += 1
                continue
            if name in ("--compressed",):
                opts.compressed = True
                index += 1
                continue
            if name in ("--fail",):
                opts.fail = True
                index += 1
                continue
            target = _VALUE_LONG.get(name)
            if target is None:
                raise UsageError(f"unsupported option: {name}")
            if has_equals:
                value = attached
                index += 1
            else:
                value, index = _next_value(argv, index, name)
            _set_value(opts, target, value)
            continue
        if arg.startswith("-") and arg != "-":
            position = 1
            while position < len(arg):
                short = arg[position]
                if short == "s":
                    opts.silent = True
                elif short == "S":
                    opts.show_error = True
                elif short == "L":
                    opts.follow = True
                elif short == "V":
                    opts.show_version = True
                elif short == "h":
                    opts.show_help = True
                elif short in _VALUE_SHORT:
                    target = _VALUE_SHORT[short]
                    attached = arg[position + 1 :]
                    if attached:
                        value = attached
                        index += 1
                    else:
                        value, index = _next_value(argv, index, f"-{short}")
                    _set_value(opts, target, value)
                    break
                else:
                    raise UsageError(f"unsupported option: -{short}")
                position += 1
            else:
                index += 1
            continue
        if opts.url:
            raise UsageError("only one URL can be requested")
        opts.url = arg
        index += 1

    if not opts.show_help and not opts.show_version and not opts.url:
        raise UsageError("a URL is required")
    return opts


def _error(opts: Options | None, message: str) -> None:
    if opts is None or not opts.silent or opts.show_error:
        print(f"animepahe-curl: {message}", file=sys.stderr)


def _cookie_pairs(value: str) -> list[tuple[str, str]]:
    pairs: list[tuple[str, str]] = []
    for part in value.split(";"):
        part = part.strip()
        if not part:
            continue
        name, separator, body = part.partition("=")
        if not separator or not name.strip():
            raise UsageError(
                "cookie files are not supported; pass cookies as 'name=value'"
            )
        pairs.append((name.strip(), body.strip()))
    return pairs


def _perform(opts: Options) -> int:
    headers = list(opts.headers)
    header_names = {name.lower() for name, _ in headers}
    if opts.user_agent and "user-agent" not in header_names:
        headers.append(("User-Agent", opts.user_agent))

    host = urlsplit(opts.url).hostname or ""
    if not host:
        raise UsageError(f"invalid URL: {opts.url!r}")

    impersonate = os.environ.get("ANIMEPAHE_CURL_IMPERSONATE", DEFAULT_IMPERSONATE)
    session = requests.Session(impersonate=impersonate)
    try:
        if opts.cookie and "cookie" not in header_names:
            # Scope the cookie to the requested host so it is not forwarded to
            # unrelated redirect targets such as a CDN or glot.io.
            for name, value in _cookie_pairs(opts.cookie):
                session.cookies.set(name, value, domain=host, path="/")

        method = opts.method.upper() if opts.method else (
            "POST" if opts.data is not None else "GET"
        )
        timeout = opts.max_time if opts.max_time else opts.connect_timeout
        response = session.request(
            method,
            opts.url,
            headers=headers or None,
            data=opts.data,
            timeout=timeout,
            allow_redirects=opts.follow,
            stream=False,
        )
    finally:
        session.close()

    if opts.fail and response.status_code >= 400:
        _error(opts, f"HTTP error {response.status_code}")
        return 22
    sys.stdout.buffer.write(response.content)
    sys.stdout.buffer.flush()
    return 0


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    try:
        opts = parse_args(argv)
    except UsageError as exc:
        _error(None, str(exc))
        return 2

    if opts.show_help:
        print(HELP, end="")
        return 0
    if opts.show_version:
        print(
            f"animepahe-curl {__version__} "
            f"(curl_cffi {CURL_CFFI_VERSION}, impersonate: {DEFAULT_IMPERSONATE})"
        )
        return 0

    try:
        return _perform(opts)
    except UsageError as exc:
        _error(opts, str(exc))
        return 2
    except Timeout:
        _error(opts, "request timed out")
        return 28
    except requests.RequestsError as exc:
        _error(opts, str(exc) or exc.__class__.__name__)
        return 7
    except BrokenPipeError:
        return 23
    except KeyboardInterrupt:
        return 130
    except OSError as exc:
        _error(opts, str(exc))
        return 7


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
