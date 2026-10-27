"""Offline tests for the subprocess runner (no network, no real catalog writes)."""

import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from app import backend, runner

STUB = """#!/usr/bin/env bash
echo "[INFO] Downloading Episode 1..."
echo "FRAGMENTS=${YTDLP_CONCURRENT_FRAGMENTS:-unset}"
echo "OUTPUT_DIR=${ANIMEPAHE_OUTPUT_DIR:-unset}"
echo "[download] Destination: /tmp/x/1.mp4"
printf '[download]  50.0%% of ~50.00MiB at 1.00MiB/s ETA 00:05\\r'
printf '[download] 100.0%% of ~50.00MiB at 1.00MiB/s ETA 00:00\\n'
echo "+ curl -b cf_clearance=SECRET123 https://example.test"
echo "[WARNING] Episode 2 not found!"
echo "https://example.test/master.m3u8"
echo "[INFO] Job finished"
"""

SLOW_STUB = """#!/usr/bin/env bash
echo "[INFO] Downloading Episode 1..."
sleep 5
"""

BATCH_FAIL_STUB = """#!/usr/bin/env bash
echo "[INFO] Downloading Episode 1..."
echo "[WARNING] Episode 1 download attempt 1 failed with exit code 1."
echo "[WARNING] Episode 1 failed after 3 attempts."
echo "[INFO] Downloading Episode 2..."
printf '[download] 100.0%% of ~10.00MiB at 1.00MiB/s ETA 00:00\\n'
echo "[INFO] Job finished"
exit 1
"""


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.stub = Path(self.tmp.name) / "stub.sh"
        self.stub.write_text(STUB, encoding="utf-8")
        self.stub.chmod(0o755)

        self.slow_stub = Path(self.tmp.name) / "slow.sh"
        self.slow_stub.write_text(SLOW_STUB, encoding="utf-8")
        self.slow_stub.chmod(0o755)

        self.download_dir = Path(self.tmp.name) / "downloads"
        self.config = {
            "cf": "",
            "ua": "Test UA",
            "concurrent_fragments": 8,
            "download_dir": str(self.download_dir),
        }
        self.patches = [
            mock.patch.object(runner, "SCRIPT_PATH", self.stub),
            mock.patch.object(backend, "ensure_entry", return_value=None),
            mock.patch.object(backend, "title_for_slug", return_value="Test Anime"),
            mock.patch.object(runner, "read_config", return_value=dict(self.config)),
        ]
        for patcher in self.patches:
            patcher.start()
        self.runner = runner.Runner()

    def tearDown(self):
        self.runner.stop()
        for patcher in reversed(self.patches):
            patcher.stop()
        self.tmp.cleanup()

    def run_job(self, slug="slug", fragments=8, **extra):
        spec = {
            "slug": slug,
            "title": "Test Anime",
            "episodes": "1",
            "concurrent_fragments": fragments,
        }
        spec.update(extra)
        self.runner.start(spec)
        deadline = time.time() + 15
        while self.runner.snapshot()["running"] and time.time() < deadline:
            time.sleep(0.05)
        self.assertFalse(self.runner.snapshot()["running"], "job did not finish in time")
        time.sleep(0.05)
        return self.runner.snapshot()

    def test_parses_progress_episodes_playlists_and_redacts_secrets(self):
        snap = self.run_job()
        self.assertEqual(snap["exitCode"], 0)
        self.assertIn("1", snap["episodes"]["completed"])
        self.assertIn("2", snap["episodes"]["failed"])
        self.assertTrue(any("master.m3u8" in url for url in snap["playlists"]))
        self.assertIn("logs", snap)

        text = "\n".join(entry["text"] for entry in snap["logs"])
        self.assertNotIn("SECRET123", text)
        self.assertIn("<redacted>", text)

    def test_log_sequence_stays_monotonic_across_jobs(self):
        self.run_job("first")
        first_seq = self.runner.snapshot()["logSeq"]
        self.assertGreater(first_seq, 0)

        self.run_job("second")
        new_logs = self.runner.logs_since(first_seq)
        self.assertTrue(new_logs, "logs from the second job should follow the first job's sequence")
        self.assertTrue(all(entry["seq"] > first_seq for entry in new_logs))

    def test_clear_logs_does_not_rewind_sequence(self):
        self.run_job("first")
        first_seq = self.runner.snapshot()["logSeq"]

        self.runner.clear_logs()
        self.assertEqual(self.runner.snapshot()["logSeq"], first_seq)
        self.assertEqual(self.runner.snapshot()["logs"], [])

        self.run_job("second")
        self.assertTrue(self.runner.logs_since(first_seq))

    def test_invalid_fragment_count_is_clamped(self):
        snap = self.run_job(fragments="not-a-number")
        text = "\n".join(entry["text"] for entry in snap["logs"])
        self.assertIn("FRAGMENTS=8", text)

    def test_selected_resolution_and_audio_are_forwarded(self):
        snap = self.run_job(resolution="720", audio="eng")
        args = snap["args"]
        self.assertEqual(args[args.index("-r") + 1], "720")
        self.assertEqual(args[args.index("-o") + 1], "eng")
        self.assertIn("-r 720", snap["command"])
        self.assertIn("-o eng", snap["command"])

    def test_replace_flag_is_forwarded(self):
        snap = self.run_job(replace=True)
        self.assertIn("-R", snap["args"])
        self.assertIn("-R", snap["command"])

        default = self.run_job()
        self.assertNotIn("-R", default["args"])
        self.assertNotIn("-R", default["command"])

    def test_destination_is_created_and_passed_to_backend(self):
        snap = self.run_job()
        text = "\n".join(entry["text"] for entry in snap["logs"])
        self.assertIn(f"OUTPUT_DIR={self.download_dir}", text)
        self.assertTrue(self.download_dir.is_dir())

    def test_unusable_destination_rejects_job(self):
        not_a_directory = Path(self.tmp.name) / "not-a-directory"
        not_a_directory.write_text("file", encoding="utf-8")
        broken = dict(self.config, download_dir=str(not_a_directory))
        with mock.patch.object(runner, "read_config", return_value=broken):
            with self.assertRaises(runner.RunnerError):
                self.runner.start(
                    {
                        "slug": "slug",
                        "title": "Test Anime",
                        "episodes": "1",
                        "concurrent_fragments": 8,
                    }
                )
        self.assertFalse(self.runner.snapshot()["running"])

    def test_snapshot_can_omit_logs(self):
        self.run_job()
        snap = self.runner.snapshot(include_logs=False)
        self.assertNotIn("logs", snap)
        self.assertIn("logSeq", snap)

    def test_high_volume_logs_stay_bounded(self):
        for index in range(25_000):
            self.runner._handle_line(
                f"[download] {index % 100:3d}.0% of 1.00GiB at 5.00MiB/s ETA 00:10"
            )

        stored = self.runner.snapshot()["logs"]
        self.assertLessEqual(len(stored), 500, "snapshots only return the newest window")
        self.assertEqual(self.runner.snapshot()["logSeq"], 25_000)
        self.assertEqual(stored[-1]["seq"], 25_000)
        self.assertGreater(stored[0]["seq"], 24_000)

    def test_download_failure_marks_episode_but_retry_warning_does_not(self):
        self.runner._handle_line("[INFO] Downloading Episode 2...")
        self.runner._handle_line(
            "[WARNING] Episode 2 download attempt 1 failed with exit code 1."
        )
        self.runner._handle_line("[WARNING] Episode 2 failed after 3 attempts.")

        episodes = self.runner.snapshot()["episodes"]
        self.assertEqual(episodes["failed"], ["2"])

        self.runner._handle_line("[INFO] Downloading Episode 3...")
        self.runner._handle_line(
            "[WARNING] Episode 3 download attempt 1 failed with exit code 1."
        )
        self.assertNotIn("3", self.runner.snapshot()["episodes"]["failed"])

    def test_existing_episode_is_marked_completed(self):
        self.runner._handle_line("[INFO] Downloading Episode 1...")
        self.runner._handle_line(
            "[WARNING] Episode 2 already exists; skipping (use -R to replace)."
        )

        episodes = self.runner.snapshot()["episodes"]
        self.assertIn("2", episodes["completed"])
        self.assertNotIn("2", episodes["failed"])
        self.assertEqual(episodes["current"], "2")

    def test_failed_episode_does_not_hide_later_completion(self):
        stub = Path(self.tmp.name) / "batch-fail.sh"
        stub.write_text(BATCH_FAIL_STUB, encoding="utf-8")
        stub.chmod(0o755)

        with mock.patch.object(runner, "SCRIPT_PATH", stub):
            snap = self.run_job(episodes="1,2")

        self.assertEqual(snap["exitCode"], 1)
        self.assertEqual(snap["episodes"]["failed"], ["1"])
        self.assertEqual(snap["episodes"]["completed"], ["2"])

    def test_stop_marks_job_as_stopped(self):
        with mock.patch.object(runner, "SCRIPT_PATH", self.slow_stub):
            self.runner.start(
                {
                    "slug": "slow",
                    "title": "Slow",
                    "episodes": "1",
                    "concurrent_fragments": 8,
                }
            )
            time.sleep(0.3)
            self.assertTrue(self.runner.stop())

            deadline = time.time() + 10
            while self.runner.snapshot()["running"] and time.time() < deadline:
                time.sleep(0.05)
            snap = self.runner.snapshot()
            self.assertFalse(snap["running"])
            self.assertTrue(snap["stopped"])


if __name__ == "__main__":
    unittest.main()
