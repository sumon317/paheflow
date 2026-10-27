"""Tests for the curl_cffi-based curl shim used against Cloudflare."""

import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
HELPER_ROOT = REPO / "tools" / "animepahe_curl"

try:
    import curl_cffi  # noqa: F401
except ImportError:  # pragma: no cover - only before requirements.sh has run
    curl_cffi = None


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # noqa: A003 - keep test output clean
        pass

    def _read_body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def _record(self, body=b""):
        self.server.capture = {
            "method": self.command,
            "path": self.path,
            "headers": {key: value for key, value in self.headers.items()},
            "body": body,
        }

    def _send(self, body=b"", status=200, headers=None):
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            try:
                self.wfile.write(body)
            except BrokenPipeError:
                pass

    def do_GET(self):  # noqa: N802 - http.server API
        self._record()
        if self.path == "/redirect":
            self._send(b"", status=302, headers={"Location": "/final"})
        elif self.path == "/slow":
            time.sleep(2)
            self._send(b"slow")
        elif self.path == "/missing":
            self._send(b"not found", status=404)
        else:
            self._send(json.dumps({"ok": True, "path": self.path}).encode())

    def do_POST(self):  # noqa: N802 - http.server API
        body = self._read_body()
        self._record(body)
        self._send(json.dumps({"ok": True, "body": body.decode("utf-8")}).encode())


class _Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), _Handler)
        self.capture = {}


@unittest.skipUnless(curl_cffi is not None, "curl_cffi is not installed")
class CurlHelperTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.server = _Server()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"

    def run_helper(self, *args, timeout=15):
        env = dict(os.environ)
        env["PYTHONPATH"] = os.pathsep.join(
            [str(HELPER_ROOT), env.get("PYTHONPATH", "")]
        ).rstrip(os.pathsep)
        return subprocess.run(
            [sys.executable, "-m", "animepahe_curl", *args],
            capture_output=True,
            timeout=timeout,
            env=env,
        )

    def test_version_does_not_need_a_url(self):
        result = self.run_helper("--version")
        self.assertEqual(result.returncode, 0, result.stderr)
        text = result.stdout.decode()
        self.assertIn("animepahe-curl", text)
        self.assertIn("curl_cffi", text)

    def test_get_sends_cookie_ua_referer_and_follows_redirect(self):
        result = self.run_helper(
            "-sS",
            "-L",
            "--compressed",
            "--connect-timeout",
            "5",
            "--max-time",
            "5",
            "-b",
            "cf_clearance=abc=def",
            "-A",
            "Helper UA",
            "-H",
            "Referer: https://example.test/",
            f"{self.base}/redirect",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["path"], "/final")
        headers = self.server.capture["headers"]
        self.assertEqual(headers.get("User-Agent"), "Helper UA")
        self.assertEqual(headers.get("Referer"), "https://example.test/")
        self.assertEqual(headers.get("Cookie"), "cf_clearance=abc=def")

    def test_without_location_the_redirect_is_not_followed(self):
        result = self.run_helper("-sS", f"{self.base}/redirect")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.server.capture["path"], "/redirect")
        self.assertEqual(result.stdout, b"")

    def test_post_sends_the_raw_json_body(self):
        payload = '{"action":"run","data":{"x":1}}'
        result = self.run_helper(
            "-sS",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "--data-raw",
            payload,
            f"{self.base}/glot",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.server.capture["method"], "POST")
        self.assertEqual(self.server.capture["body"], payload.encode())
        self.assertEqual(
            self.server.capture["headers"].get("Content-Type"), "application/json"
        )

    def test_fail_option_reports_http_errors(self):
        result = self.run_helper("-sS", "--fail", f"{self.base}/missing")
        self.assertEqual(result.returncode, 22)
        self.assertIn(b"HTTP error", result.stderr)

    def test_timeout_is_bounded_and_reported(self):
        result = self.run_helper("-sS", "--max-time", "0.2", f"{self.base}/slow")
        self.assertEqual(result.returncode, 28, result.stderr)
        self.assertIn(b"timed out", result.stderr)

    def test_unknown_option_is_rejected_instead_of_ignored(self):
        result = self.run_helper("-sS", "--definitely-not-curl", f"{self.base}/")
        self.assertEqual(result.returncode, 2)
        self.assertIn(b"unsupported option", result.stderr)

    def test_url_is_required(self):
        result = self.run_helper("-sS")
        self.assertEqual(result.returncode, 2)
        self.assertIn(b"URL is required", result.stderr)


if __name__ == "__main__":
    unittest.main()
