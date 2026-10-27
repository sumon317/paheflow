import tempfile
import unittest
from pathlib import Path
from unittest import mock

from app import library


class LibraryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.base = Path(self.tmp.name)
        self.patcher = mock.patch.object(library, "BASE_DIR", self.base)
        self.patcher.start()
        self.root_patcher = mock.patch.object(
            library, "download_root", return_value=self.base.resolve(), create=True
        )
        self.root_patcher.start()

        (self.base / "Anime").mkdir()
        (self.base / "Anime" / "1.mp4").write_bytes(b"video-1")
        (self.base / "Anime" / "2.mkv").write_bytes(b"video-2")

        (self.base / "app").mkdir()
        (self.base / "app" / "server.py").write_text("code", encoding="utf-8")

        (self.base / "static").mkdir()
        (self.base / "static" / "clip.mp4").write_bytes(b"not-an-episode")

        (self.base / "Empty").mkdir()
        (self.base / "Empty" / "notes.txt").write_text("nothing", encoding="utf-8")

        (self.base / "Anime" / "Sub").mkdir()
        (self.base / "Anime" / "Sub" / "nested.mp4").write_bytes(b"nested")

    def tearDown(self):
        self.root_patcher.stop()
        self.patcher.stop()
        self.tmp.cleanup()

    def test_safe_path_blocks_escape(self):
        with self.assertRaises(library.LibraryError):
            library._safe_path("../outside.mp4")
        with self.assertRaises(library.LibraryError):
            library._safe_path("/etc/passwd")

    def test_scan_ignores_reserved_and_non_anime_directories(self):
        names = {entry["name"] for entry in library.scan()}
        self.assertEqual(names, {"Anime"})

    def test_delete_file_refuses_outside_anime_folder(self):
        with self.assertRaises(library.LibraryError):
            library.delete_file("static/clip.mp4")
        self.assertTrue((self.base / "static" / "clip.mp4").exists())

    def test_delete_file_refuses_nested_folder(self):
        with self.assertRaises(library.LibraryError):
            library.delete_file("Anime/Sub/nested.mp4")
        self.assertTrue((self.base / "Anime" / "Sub" / "nested.mp4").exists())

    def test_delete_file_allows_direct_episode(self):
        result = library.delete_file("Anime/1.mp4")
        self.assertEqual(result["deleted"], "1.mp4")
        self.assertFalse((self.base / "Anime" / "1.mp4").exists())

    def test_delete_folder_refuses_reserved(self):
        with self.assertRaises(library.LibraryError):
            library.delete_folder("app")
        self.assertTrue((self.base / "app" / "server.py").exists())

    def test_delete_folder_refuses_folder_without_videos(self):
        with self.assertRaises(library.LibraryError):
            library.delete_folder("Empty")
        self.assertTrue((self.base / "Empty").exists())

    def test_delete_folder_refuses_nested_folder(self):
        with self.assertRaises(library.LibraryError):
            library.delete_folder("Anime/Sub")
        self.assertTrue((self.base / "Anime" / "Sub").exists())

    def test_delete_folder_allows_direct_anime_folder(self):
        result = library.delete_folder("Anime")
        self.assertEqual(result["deleted"], "Anime")
        self.assertFalse((self.base / "Anime").exists())

    def test_scan_and_delete_use_configured_destination(self):
        destination = self.base / "elsewhere" / "media"
        (destination / "External Show").mkdir(parents=True)
        (destination / "External Show" / "3.mp4").write_bytes(b"external")

        with mock.patch.object(
            library, "download_root", return_value=destination.resolve()
        ):
            names = [entry["name"] for entry in library.scan()]
            self.assertEqual(names, ["External Show"])
            result = library.delete_file(
                str(destination / "External Show" / "3.mp4")
            )

        self.assertEqual(result["deleted"], "3.mp4")
        self.assertFalse((destination / "External Show" / "3.mp4").exists())

    def test_configured_destination_does_not_allow_other_paths(self):
        destination = self.base / "elsewhere" / "media"
        destination.mkdir(parents=True)
        outside = self.base / "outside.mp4"
        outside.write_bytes(b"outside")

        with mock.patch.object(
            library, "download_root", return_value=destination.resolve()
        ):
            with self.assertRaises(library.LibraryError):
                library.delete_file(str(outside))
        self.assertTrue(outside.exists())


if __name__ == "__main__":
    unittest.main()
