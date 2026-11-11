"""HTTP API, authentication, Host validation and SSE tests (no real network)."""

from __future__ import annotations

import http.client
import json
import socket
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from app import backend, runner, server
from app.folder_picker import FolderPickerError

TEST_TOKEN = "test-token-123"
SECRET_COOKIE = "SUPER_SECRET_COOKIE_VALUE"
FAKE_CONFIG = {
    "cf": SECRET_COOKIE,
    "ua": "Test User Agent",
    "concurrent_fragments": 7,
}

FIRST_STUB = """#!/usr/bin/env bash
echo "[INFO] FIRST_JOB_READY"
echo "https://example.test/first.m3u8"
echo "[INFO] first job done"
"""

SECOND_STUB = """#!/usr/bin/env bash
echo "[INFO] SECOND_JOB_MARKER"
echo "https://example.test/second.m3u8"
echo "[INFO] second job done"
"""

HIGH_VOLUME_STUB = """#!/usr/bin/env python3
for index in range(8000):
    print(f"[download] {index % 100:3d}.0% of 1.00GiB at 5.00MiB/s ETA 00:10", flush=True)
print("[INFO] high volume job done", flush=True)
"""


class ServerHarness(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.first_stub = self.write_stub("first.sh", FIRST_STUB)
        self.second_stub = self.write_stub("second.sh", SECOND_STUB)

        self.patches = [
            mock.patch.object(server, "TOKEN", TEST_TOKEN),
            mock.patch.object(server, "read_config", return_value=dict(FAKE_CONFIG)),
            mock.patch.object(server, "dependencies", return_value=[]),
            mock.patch.object(
                server.backend,
                "catalog",
                return_value=[{"slug": "demo", "title": "Demo Anime"}],
            ),
            mock.patch.object(backend, "ensure_entry", return_value=None),
            mock.patch.object(backend, "title_for_slug", return_value="Test Anime"),
        ]
        for patcher in self.patches:
            patcher.start()

        self.runner = runner.Runner()
        self.runner_patch = mock.patch.object(server, "RUNNER", self.runner)
        self.runner_patch.start()

        self.server = server.GuiHTTPServer(("127.0.0.1", 0), server.Handler)
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.runner.stop()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        self.runner_patch.stop()
        for patcher in reversed(self.patches):
            patcher.stop()
        self.tmp.cleanup()

    def write_stub(self, name: str, body: str) -> Path:
        path = self.dir / name
        path.write_text(body, encoding="utf-8")
        path.chmod(0o755)
        return path

    # ------------------------------------------------------------ HTTP helpers

    def request(
        self,
        method: str,
        path: str,
        *,
        body: dict | None = None,
        token: str | None = TEST_TOKEN,
        host: str | None = None,
        headers: dict[str, str] | None = None,
    ) -> tuple[int, bytes, dict[str, str]]:
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        conn.putrequest(method, path, skip_host=True)
        conn.putheader("Host", host if host is not None else f"127.0.0.1:{self.port}")
        if token is not None:
            conn.putheader("X-App-Token", token)
        raw = None
        if body is not None:
            raw = json.dumps(body).encode("utf-8")
            conn.putheader("Content-Type", "application/json")
            conn.putheader("Content-Length", str(len(raw)))
        for key, value in (headers or {}).items():
            conn.putheader(key, value)
        conn.endheaders(raw)
        response = conn.getresponse()
        payload = response.read()
        result = (response.status, payload, {k.lower(): v for k, v in response.getheaders()})
        conn.close()
        return result

    def json_request(self, *args, **kwargs):
        status, payload, headers = self.request(*args, **kwargs)
        return status, json.loads(payload.decode("utf-8")), headers

    def start_job(self, stub: Path) -> None:
        with mock.patch.object(runner, "SCRIPT_PATH", stub):
            status, body, _ = self.json_request(
                "POST",
                "/api/run",
                body={
                    "slug": "sse-test",
                    "title": "SSE Test",
                    "episodes": "1",
                    "concurrent_fragments": 8,
                },
            )
        self.assertEqual(status, 200, body)

    def wait_for_idle(self, timeout: float = 15.0) -> None:
        deadline = time.time() + timeout
        while self.runner.snapshot()["running"] and time.time() < deadline:
            time.sleep(0.05)
        self.assertFalse(self.runner.snapshot()["running"], "job did not finish in time")
        time.sleep(0.1)

    # -------------------------------------------------------------- SSE helpers

    def open_sse(self, *, token: str | None = TEST_TOKEN, headers: dict[str, str] | None = None):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=1.0)
        conn.putrequest("GET", "/api/events", skip_host=True)
        conn.putheader("Host", f"127.0.0.1:{self.port}")
        if token is not None:
            conn.putheader("X-App-Token", token)
        for key, value in (headers or {}).items():
            conn.putheader(key, value)
        conn.endheaders()
        response = conn.getresponse()
        return conn, response

    @staticmethod
    def read_sse_response(response, deadline: float, stop_when) -> list[dict]:
        """Read complete SSE frames until stop_when(event) or the deadline."""
        events: list[dict] = []
        current = {"event": None, "data": [], "id": None}
        while time.time() < deadline:
            try:
                raw = response.fp.readline()
            except (socket.timeout, TimeoutError, OSError):
                break
            if not raw:
                break
            line = raw.decode("utf-8", "replace").rstrip("\r\n")
            if line.startswith("event:"):
                current["event"] = line[6:].strip()
            elif line.startswith("data:"):
                current["data"].append(line[5:].strip())
            elif line.startswith("id:"):
                current["id"] = line[3:].strip()
            elif line == "":
                if current["data"]:
                    try:
                        payload = json.loads("\n".join(current["data"]))
                    except json.JSONDecodeError:
                        payload = None
                    if payload is not None:
                        event = {
                            "event": current["event"] or "message",
                            "data": payload,
                            "id": current["id"],
                        }
                        events.append(event)
                        if stop_when(event):
                            return events
                current = {"event": None, "data": [], "id": None}
        return events

    def read_until(self, response, predicate, timeout: float = 15.0) -> list[dict]:
        return self.read_sse_response(response, time.time() + timeout, predicate)

    # ------------------------------------------------------------------- tests

    def test_all_api_endpoints_require_a_token(self):
        paths = [
            "/api/state",
            "/api/config",
            "/api/health",
            "/api/deps",
            "/api/catalog",
            "/api/search?q=x",
            "/api/latest",
            "/api/episodes?slug=x",
            "/api/episode-options?slug=x&episode=1",
            "/api/status",
            "/api/logs",
            "/api/events",
            "/api/library",
        ]
        for path in paths:
            with self.subTest(path=path):
                status, body, _ = self.request("GET", path, token=None)
                self.assertEqual(status, 403, body)
                self.assertIn(b"token", body.lower())

        for path in [
            "/api/config",
            "/api/run",
            "/api/stop",
            "/api/clear-logs",
            "/api/library/delete",
            "/api/pick-folder",
        ]:
            with self.subTest(path=path):
                status, body, _ = self.request("POST", path, body={}, token=None)
                self.assertEqual(status, 403, body)

    def test_valid_token_can_reach_the_api(self):
        status, payload, _ = self.json_request("GET", "/api/health")
        self.assertEqual(status, 200)
        self.assertTrue(payload["cfConfigured"])
        status, payload, _ = self.json_request("GET", "/api/status")
        self.assertEqual(status, 200)
        self.assertIn("running", payload)

    def test_config_responses_never_echo_the_cookie(self):
        for path in ("/api/config", "/api/state", "/api/health"):
            with self.subTest(path=path):
                status, payload, _ = self.json_request("GET", path)
                self.assertEqual(status, 200)
                text = json.dumps(payload)
                self.assertNotIn(SECRET_COOKIE, text)
        status, payload, _ = self.json_request("GET", "/api/config")
        self.assertNotIn("cf", payload)
        self.assertTrue(payload["cfConfigured"])
        self.assertEqual(payload["cfLength"], len(SECRET_COOKIE))
        self.assertEqual(payload["downloadDir"], str(server.BASE_DIR))
        self.assertEqual(payload["defaultDownloadDir"], str(server.BASE_DIR))

        fake_saved = {
            "cf": SECRET_COOKIE,
            "ua": "Test User Agent",
            "concurrent_fragments": 7,
        }
        with mock.patch.object(server, "write_config", return_value=fake_saved):
            status, payload, _ = self.json_request("POST", "/api/config", body={"ua": "x"})
        self.assertEqual(status, 200)
        self.assertNotIn(SECRET_COOKIE, json.dumps(payload))

    def test_non_localhost_host_header_is_rejected(self):
        status, body, _ = self.request("GET", "/", host="evil.example")
        self.assertEqual(status, 403, body)
        status, body, _ = self.request("GET", "/api/state", host="evil.example:8765")
        self.assertEqual(status, 403, body)
        status, body, _ = self.request("GET", "/", host=f"localhost:{self.port}")
        self.assertEqual(status, 200, body)

    def test_non_ascii_token_is_rejected_without_dropping_the_connection(self):
        status, body, _ = self.request("GET", "/api/health", token="tökén")
        self.assertEqual(status, 403, body)
        self.assertIn(b"token", body.lower())

    def test_static_directory_traversal_returns_404(self):
        for path in ("/static/../app/config.py", "/static/%2e%2e/app/config.py"):
            with self.subTest(path=path):
                status, body, _ = self.request("GET", path)
                self.assertEqual(status, 404, body)

    def test_state_and_status_do_not_include_logs(self):
        with mock.patch.object(runner, "SCRIPT_PATH", self.first_stub):
            self.start_job(self.first_stub)
        self.wait_for_idle()

        status, state, _ = self.json_request("GET", "/api/state")
        self.assertEqual(status, 200)
        self.assertNotIn("logs", state["job"])
        self.assertGreater(state["job"]["logSeq"], 0)

        status, job, _ = self.json_request("GET", "/api/status")
        self.assertEqual(status, 200)
        self.assertNotIn("logs", job)

        status, payload, _ = self.json_request("GET", "/api/logs?after=0")
        self.assertEqual(status, 200)
        self.assertTrue(payload["logs"])
        self.assertTrue(any("FIRST_JOB_READY" in entry["text"] for entry in payload["logs"]))

        last_seq = payload["logs"][-1]["seq"]
        status, payload, _ = self.json_request("GET", f"/api/logs?after={last_seq}")
        self.assertEqual(status, 200)
        self.assertEqual(payload["logs"], [])

    def test_sse_streams_log_events_and_survives_multiple_jobs(self):
        conn, response = self.open_sse()
        self.addCleanup(conn.close)
        try:
            self.assertEqual(response.status, 200)
            self.assertIn("text/event-stream", response.getheader("Content-Type", ""))

            self.start_job(self.first_stub)
            first_events = self.read_until(
                response, lambda event: event["event"] == "done", timeout=15
            )
            log_events = [event for event in first_events if event["event"] == "log"]
            self.assertTrue(log_events, "the SSE stream never delivered log events")
            self.assertTrue(any("FIRST_JOB_READY" in event["data"]["text"] for event in log_events))
            for event in log_events:
                self.assertIsNotNone(event["id"], "log events must carry an SSE id")
                self.assertEqual(int(event["id"]), event["data"]["seq"])
            first_last_seq = max(event["data"]["seq"] for event in log_events)

            # The stream must remain open after the first done event.
            self.start_job(self.second_stub)
            second_events = self.read_until(
                response,
                lambda event: event["event"] == "log"
                and "SECOND_JOB_MARKER" in event["data"]["text"],
                timeout=15,
            )
            marker = [event for event in second_events if event["event"] == "log"][-1]
            self.assertGreater(marker["data"]["seq"], first_last_seq)
            self.read_until(response, lambda event: event["event"] == "done", timeout=15)
        finally:
            response.close()
            conn.close()

    def test_high_volume_sse_does_not_starve_other_requests(self):
        high_volume = self.write_stub("high-volume.py", HIGH_VOLUME_STUB)
        conn, response = self.open_sse()
        self.addCleanup(conn.close)
        try:
            self.start_job(high_volume)

            # While the event stream has a backlog it cannot write to this
            # unread socket, unrelated API requests must still be answered.
            latencies = []
            for _ in range(8):
                started = time.perf_counter()
                status, payload, _ = self.json_request("GET", "/api/health")
                latencies.append(time.perf_counter() - started)
                self.assertEqual(status, 200, payload)
                time.sleep(0.05)
            self.assertLess(max(latencies), 3.0, f"slow responses: {latencies}")

            events = self.read_until(
                response, lambda event: event["event"] == "done", timeout=20
            )
            log_events = [event for event in events if event["event"] == "log"]
            self.assertTrue(log_events, "high-volume output was never streamed")
            self.assertLessEqual(
                len(self.runner.logs_since(0)),
                2000,
                "stored logs must stay bounded",
            )
        finally:
            response.close()
            conn.close()

    def test_sse_last_event_id_prevents_replaying_old_logs(self):
        with mock.patch.object(runner, "SCRIPT_PATH", self.first_stub):
            self.start_job(self.first_stub)
        self.wait_for_idle()
        last_seq = self.runner.snapshot(include_logs=False)["logSeq"]

        conn, response = self.open_sse(headers={"Last-Event-ID": str(last_seq)})
        try:
            events = self.read_sse_response(
                response, time.time() + 1.5, lambda event: False
            )
        finally:
            response.close()
            conn.close()
        log_events = [event for event in events if event["event"] == "log"]
        self.assertEqual(log_events, [], "old log entries must not be replayed")

    def test_pick_folder_returns_a_selection(self):
        with mock.patch.object(server, "pick_folder", return_value="/tmp/Anime Library") as pick:
            status, payload, _ = self.json_request(
                "POST", "/api/pick-folder", body={"current": "/tmp"}
            )
        self.assertEqual(status, 200, payload)
        self.assertEqual(payload["path"], "/tmp/Anime Library")
        self.assertFalse(payload.get("cancelled", False))
        pick.assert_called_once_with("/tmp")

    def test_pick_folder_reports_cancellation(self):
        with mock.patch.object(server, "pick_folder", return_value=None):
            status, payload, _ = self.json_request(
                "POST", "/api/pick-folder", body={}
            )
        self.assertEqual(status, 200, payload)
        self.assertTrue(payload["cancelled"])
        self.assertNotIn("path", payload)

    def test_pick_folder_without_a_dialog_reports_501(self):
        with mock.patch.object(
            server, "pick_folder", side_effect=FolderPickerError("Install zenity or kdialog.")
        ):
            status, payload, _ = self.json_request(
                "POST", "/api/pick-folder", body={}
            )
        self.assertEqual(status, 501, payload)
        self.assertIn("zenity", payload["error"])

    def test_invalid_download_folder_reports_400(self):
        with mock.patch.object(
            server,
            "write_config",
            side_effect=ValueError("Download folder cannot be the filesystem root."),
        ):
            status, payload, _ = self.json_request(
                "POST", "/api/config", body={"download_dir": "/"}
            )
        self.assertEqual(status, 400, payload)
        self.assertIn("filesystem root", payload["error"])

    def test_episode_error_falls_back_to_the_saved_list(self):
        cached = [{"episode": "1", "session": "s1", "audio": "jpn"}]
        with mock.patch.object(
            server.backend,
            "release",
            side_effect=backend.BackendError("Cloudflare refused this request (block page)."),
        ), mock.patch.object(server.backend, "load_release_cache", return_value=cached):
            status, payload, _ = self.json_request("GET", "/api/episodes?slug=demo")

        self.assertEqual(status, 200, payload)
        self.assertEqual(payload["items"], cached)
        self.assertIs(payload["stale"], True)
        self.assertIn("block page", payload["warning"])

    def test_episode_error_without_a_cache_is_reported(self):
        with mock.patch.object(
            server.backend,
            "release",
            side_effect=backend.BackendError("animepahe.pw is unavailable."),
        ), mock.patch.object(server.backend, "load_release_cache", return_value=[]):
            status, payload, _ = self.json_request("GET", "/api/episodes?slug=demo")

        self.assertEqual(status, 502, payload)
        self.assertIn("unavailable", payload["error"])

    def test_unknown_api_with_token_is_404(self):
        status, payload, _ = self.json_request("GET", "/api/nope")
        self.assertEqual(status, 404)
        self.assertIn("Not found", payload["error"])


class DependencyTests(unittest.TestCase):
    def test_dependency_list_requires_the_helper_and_ffmpeg(self):
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(server, "BASE_DIR", Path(tmp)), mock.patch.object(
                server.shutil,
                "which",
                side_effect=lambda name: {
                    "jq": "/usr/bin/jq",
                    "ffmpeg": "/usr/bin/ffmpeg",
                    "yt-dlp": "/usr/bin/yt-dlp",
                }.get(name),
            ):
                deps = {item["name"]: item for item in server.dependencies()}

        self.assertTrue(deps["ffmpeg"]["required"])
        self.assertTrue(deps["ffmpeg"]["found"])
        self.assertTrue(deps["Cloudflare client"]["required"])
        self.assertFalse(deps["Cloudflare client"]["found"])
        self.assertFalse(deps["fzf"]["required"])

    def test_project_local_animepahe_curl_counts_as_found(self):
        with tempfile.TemporaryDirectory() as tmp:
            local = Path(tmp) / ".uv" / "bin"
            local.mkdir(parents=True)
            helper = local / "animepahe-curl"
            helper.write_text("#!/usr/bin/env bash\n", encoding="utf-8")
            helper.chmod(0o755)

            with mock.patch.object(server, "BASE_DIR", Path(tmp)), mock.patch.object(
                server.shutil, "which", return_value=None
            ):
                deps = {item["name"]: item for item in server.dependencies()}

        self.assertTrue(deps["Cloudflare client"]["found"])
        self.assertEqual(deps["Cloudflare client"]["path"], str(helper))


class ConnectionAndRedactionTests(unittest.TestCase):
    def test_connection_reports_success(self):
        with mock.patch.object(
            server.backend, "search", return_value=[{"title": "x"}]
        ) as search:
            result = server.test_connection()

        self.assertTrue(result["ok"])
        self.assertIn("1 search result", result["message"])
        search.assert_called_once_with("one piece")

    def test_connection_reports_backend_errors(self):
        with mock.patch.object(
            server.backend,
            "search",
            side_effect=backend.BackendError("Cloudflare challenge"),
        ):
            result = server.test_connection()

        self.assertFalse(result["ok"])
        self.assertEqual(result["message"], "Cloudflare challenge")
        self.assertEqual(result["elapsed"], 0.0)

    def test_public_config_never_returns_the_cookie(self):
        public = server.public_config(
            {
                "cf": SECRET_COOKIE,
                "ua": "Test User Agent",
                "concurrent_fragments": 7,
                "download_dir": "/tmp/downloads",
            }
        )

        self.assertNotIn("cf", public)
        self.assertTrue(public["cfConfigured"])
        self.assertEqual(public["cfLength"], len(SECRET_COOKIE))
        self.assertNotIn(SECRET_COOKIE, json.dumps(public))


if __name__ == "__main__":
    unittest.main()
