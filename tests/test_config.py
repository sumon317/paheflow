import json
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from app import config


class ConfigTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.base = Path(self.tmp.name)
        self.path = self.base / "config.json"
        self.patcher = mock.patch.object(config, "CONFIG_PATH", self.path)
        self.patcher.start()
        self.base_patcher = mock.patch.object(config, "BASE_DIR", self.base)
        self.base_patcher.start()

    def tearDown(self):
        self.base_patcher.stop()
        self.patcher.stop()
        self.tmp.cleanup()

    def seed(self, **values):
        payload = {"cf": "cookie", "ua": "UA", "concurrent_fragments": 7}
        payload.update(values)
        self.path.write_text(json.dumps(payload), encoding="utf-8")

    def test_missing_file_returns_defaults(self):
        cfg = config.read_config()
        self.assertEqual(cfg["cf"], "")
        self.assertEqual(cfg["concurrent_fragments"], config.DEFAULT_FRAGMENTS)
        self.assertEqual(Path(cfg["download_dir"]), self.base.resolve())
        self.assertTrue(cfg["ua"])

    def test_fragments_are_clamped(self):
        self.assertEqual(config.clamp_fragments(None), config.DEFAULT_FRAGMENTS)
        self.assertEqual(config.clamp_fragments("nope"), config.DEFAULT_FRAGMENTS)
        self.assertEqual(config.clamp_fragments(-5), config.MIN_FRAGMENTS)
        self.assertEqual(config.clamp_fragments(999), config.MAX_FRAGMENTS)

    def test_omitted_cookie_is_preserved(self):
        self.seed()
        result = config.write_config({"ua": "New UA"})
        self.assertEqual(result["cf"], "cookie")
        self.assertEqual(config.read_config()["cf"], "cookie")

    def test_none_cookie_is_preserved(self):
        self.seed()
        result = config.write_config({"cf": None, "ua": "New UA"})
        self.assertEqual(result["cf"], "cookie", "None must mean 'keep the stored cookie'")
        self.assertEqual(config.read_config()["cf"], "cookie")

    def test_empty_cookie_clears_it(self):
        self.seed()
        config.write_config({"cf": ""})
        self.assertEqual(config.read_config()["cf"], "")

    def test_cookie_can_be_replaced(self):
        self.seed()
        config.write_config({"cf": "new-cookie"})
        self.assertEqual(config.read_config()["cf"], "new-cookie")

    def test_download_dir_is_created_and_saved(self):
        destination = self.base / "media" / "anime"
        result = config.write_config({"download_dir": str(destination)})
        self.assertEqual(Path(result["download_dir"]), destination.resolve())
        self.assertTrue(destination.is_dir(), "the destination must be created")
        self.assertEqual(
            Path(config.read_config()["download_dir"]), destination.resolve()
        )

    def test_relative_download_dir_is_resolved_against_project(self):
        result = config.write_config({"download_dir": "downloads"})
        self.assertEqual(
            Path(result["download_dir"]), (self.base / "downloads").resolve()
        )

    def test_omitted_download_dir_is_preserved(self):
        destination = self.base / "media"
        config.write_config({"download_dir": str(destination)})
        config.write_config({"ua": "New UA"})
        self.assertEqual(
            Path(config.read_config()["download_dir"]), destination.resolve()
        )

    def test_empty_download_dir_restores_default(self):
        config.write_config({"download_dir": str(self.base / "elsewhere")})
        config.write_config({"download_dir": ""})
        self.assertEqual(
            Path(config.read_config()["download_dir"]), self.base.resolve()
        )

    def test_filesystem_root_is_rejected_as_download_dir(self):
        root = Path(self.base.anchor)
        with self.assertRaises(ValueError):
            config.write_config({"download_dir": str(root)})

    def test_file_is_rejected_as_download_dir(self):
        not_a_directory = self.base / "config.json"
        not_a_directory.write_text("{}", encoding="utf-8")
        with self.assertRaises(ValueError):
            config.write_config({"download_dir": str(not_a_directory)})

    @unittest.skipUnless(os.name == "posix", "POSIX permissions only")
    def test_config_and_backup_are_private(self):
        self.seed()
        config.write_config({"ua": "New UA"})
        self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o600)
        backup = self.path.with_suffix(".json.bak")
        self.assertTrue(backup.exists())
        self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o600)


if __name__ == "__main__":
    unittest.main()
