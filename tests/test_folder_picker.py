"""Tests for the native folder picker helper."""

import subprocess
import unittest
from pathlib import Path
from unittest import mock

from app import folder_picker


class FolderPickerTests(unittest.TestCase):
    def test_zenity_selection_is_returned_as_an_absolute_path(self):
        completed = subprocess.CompletedProcess(
            args=["zenity"], returncode=0, stdout="/tmp/My Anime\n", stderr=""
        )
        with mock.patch.object(
            folder_picker.shutil,
            "which",
            side_effect=lambda name: "/usr/bin/zenity" if name == "zenity" else None,
        ), mock.patch.object(folder_picker.subprocess, "run", return_value=completed) as run:
            result = folder_picker.pick_folder("/tmp")

        self.assertEqual(result, str(Path("/tmp/My Anime").resolve()))
        command = run.call_args.args[0]
        self.assertEqual(command[0], "/usr/bin/zenity")
        self.assertIn("--directory", command)
        self.assertTrue(any(arg.startswith("--filename=") for arg in command))

    def test_cancel_returns_none(self):
        completed = subprocess.CompletedProcess(
            args=["zenity"], returncode=1, stdout="", stderr=""
        )
        with mock.patch.object(
            folder_picker.shutil,
            "which",
            side_effect=lambda name: "/usr/bin/zenity" if name == "zenity" else None,
        ), mock.patch.object(folder_picker.subprocess, "run", return_value=completed):
            self.assertIsNone(folder_picker.pick_folder("/tmp"))

    def test_missing_dialog_program_raises_a_clear_error(self):
        with mock.patch.object(folder_picker.shutil, "which", return_value=None), \
                mock.patch.object(folder_picker, "_tkinter_pick", return_value=None), \
                mock.patch.object(folder_picker, "_display_available", return_value=False):
            with self.assertRaises(folder_picker.FolderPickerError) as caught:
                folder_picker.pick_folder("/tmp")
        self.assertIn("zenity", str(caught.exception).lower())


if __name__ == "__main__":
    unittest.main()
