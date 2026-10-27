"""Offline tests for the animepahe API client's option probing."""

import tempfile
import unittest
from pathlib import Path
from unittest import mock

from app import backend


class AvailableOptionsTests(unittest.TestCase):
    def test_returns_only_real_resolution_audio_pairs(self):
        episodes = [{"episode": "1", "session": "session-1"}]
        sources = [
            {"resolution": "1080", "audio": "jpn", "url": "https://cdn/jpn1080"},
            {"resolution": "1080", "audio": "eng", "url": "https://cdn/eng1080"},
            {"resolution": "720", "audio": "eng", "url": "https://cdn/eng720"},
            # Duplicate pair: must appear only once.
            {"resolution": "1080", "audio": "jpn", "url": "https://cdn/jpn1080b"},
            # A source without a resolution cannot form a selectable pair.
            {"resolution": "", "audio": "eng", "url": "https://cdn/unknown"},
        ]

        with mock.patch.object(backend, "release", return_value=episodes), mock.patch.object(
            backend, "play_sources", return_value=sources
        ):
            options = backend.available_options("slug", "1")

        self.assertEqual(options["resolutions"], ["1080", "720"])
        self.assertEqual(options["audios"], ["jpn", "eng"])
        self.assertEqual(
            options["combinations"],
            [
                {"resolution": "1080", "audio": "eng"},
                {"resolution": "1080", "audio": "jpn"},
                {"resolution": "720", "audio": "eng"},
            ],
        )
        self.assertEqual(options["sourceCount"], len(sources))

    def test_empty_probe_still_returns_stable_shape(self):
        episodes = [{"episode": "2", "session": "session-2"}]

        with mock.patch.object(backend, "release", return_value=episodes), mock.patch.object(
            backend, "play_sources", return_value=[]
        ):
            options = backend.available_options("slug", "2")

        self.assertEqual(options["resolutions"], [])
        self.assertEqual(options["audios"], [])
        self.assertEqual(options["combinations"], [])
        self.assertEqual(options["sourceCount"], 0)

    def test_missing_episode_raises(self):
        with mock.patch.object(backend, "release", return_value=[]):
            with self.assertRaises(backend.BackendError):
                backend.available_options("slug", "404")


class CurlBinaryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        patcher = mock.patch.object(backend, "BASE_DIR", self.base)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_real_curl_impersonate_is_preferred(self):
        with mock.patch.object(
            backend.shutil,
            "which",
            side_effect=lambda name: {
                "curl-impersonate": "/usr/bin/curl-impersonate",
                "animepahe-curl": "/usr/bin/animepahe-curl",
                "curl": "/usr/bin/curl",
            }.get(name),
        ):
            self.assertEqual(backend._curl_binary(), "/usr/bin/curl-impersonate")

    def test_animepahe_curl_is_the_fallback(self):
        with mock.patch.object(
            backend.shutil,
            "which",
            side_effect=lambda name: {
                "animepahe-curl": "/usr/bin/animepahe-curl",
                "curl": "/usr/bin/curl",
            }.get(name),
        ):
            self.assertEqual(backend._curl_binary(), "/usr/bin/animepahe-curl")

    def test_project_local_helper_is_used_without_path_entry(self):
        local = self.base / ".uv" / "bin"
        local.mkdir(parents=True)
        helper = local / "animepahe-curl"
        helper.write_text("#!/usr/bin/env bash\n", encoding="utf-8")
        helper.chmod(0o755)

        with mock.patch.object(backend.shutil, "which", return_value=None):
            self.assertEqual(backend._curl_binary(), str(helper))

    def test_plain_curl_is_only_a_last_resort(self):
        with mock.patch.object(
            backend.shutil,
            "which",
            side_effect=lambda name: "/usr/bin/curl" if name == "curl" else None,
        ):
            self.assertEqual(backend._curl_binary(), "/usr/bin/curl")


if __name__ == "__main__":
    unittest.main()
