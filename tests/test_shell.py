"""Tests for the vendored bash backend's CLI and catalog safety."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from textwrap import dedent

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "animepahe-dl.sh"


class ShellTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.script = self.dir / "animepahe-dl.sh"
        shutil.copy2(SCRIPT, self.script)
        self.script.chmod(0o755)

        self.config = self.dir / "config.json"
        self.config.write_text(
            '{"cf": "cookie", "ua": "UA", "concurrent_fragments": 7}\n',
            encoding="utf-8",
        )
        self.anime_list = self.dir / "anime.list"

        self.bin = self.dir / "bin"
        self.bin.mkdir()
        self.curl_log = self.dir / "curl.log"

        self.make_executable(
            "curl-impersonate",
            '#!/usr/bin/env bash\n'
            'echo called >> "$CURL_LOG"\n'
            "exit 0\n",
        )
        self.make_executable("fzf", "#!/usr/bin/env bash\nexit 0\n")
        self.make_executable("yt-dlp", "#!/usr/bin/env bash\nexit 0\n")

    def tearDown(self):
        self.tmp.cleanup()

    def make_executable(self, name, body):
        path = self.bin / name
        path.write_text(body, encoding="utf-8")
        path.chmod(0o755)

    def run_script(self, *args, extra_env=None):
        env = dict(os.environ)
        env["PATH"] = f"{self.bin}:{env.get('PATH', '')}"
        env["CURL_LOG"] = str(self.curl_log)
        env.pop("YTDLP_CONCURRENT_FRAGMENTS", None)
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            [str(self.script), *args],
            cwd=self.dir,
            env=env,
            capture_output=True,
            text=True,
            timeout=20,
        )

    def test_slug_run_gets_past_startup_and_keeps_catalog(self):
        self.anime_list.write_text(
            "[test-slug] Test Anime   \n", encoding="utf-8"
        )
        result = self.run_script("-s", "test-slug", "-e", "1", "-l")

        combined = result.stdout + result.stderr
        self.assertNotIn("unbound variable", combined)
        self.assertTrue(
            self.curl_log.exists(),
            "the script never reached an API call; it likely failed during set_var",
        )
        self.assertIn("[test-slug] Test Anime", self.anime_list.read_text(encoding="utf-8"))

    def test_downloads_go_to_configured_destination(self):
        destination = self.dir / "media" / "anime"
        self.config.write_text(
            json.dumps(
                {
                    "cf": "cookie",
                    "ua": "UA",
                    "concurrent_fragments": 7,
                    "download_dir": str(destination),
                }
            )
            + "\n",
            encoding="utf-8",
        )
        self.anime_list.write_text("[test-slug] Test Anime   \n", encoding="utf-8")

        result = self.run_script("-s", "test-slug", "-e", "1", "-l")

        self.assertTrue(
            (destination / "Test Anime").is_dir(),
            f"destination folder was not used: {result.stdout}{result.stderr}",
        )
        self.assertFalse((self.dir / "Test Anime").exists())

    def test_environment_destination_overrides_config(self):
        from_config = self.dir / "from-config"
        from_env = self.dir / "from-env"
        self.config.write_text(
            json.dumps(
                {
                    "cf": "cookie",
                    "ua": "UA",
                    "concurrent_fragments": 7,
                    "download_dir": str(from_config),
                }
            )
            + "\n",
            encoding="utf-8",
        )
        self.anime_list.write_text("[test-slug] Test Anime   \n", encoding="utf-8")

        result = self.run_script(
            "-s", "test-slug", "-e", "1", "-l",
            extra_env={"ANIMEPAHE_OUTPUT_DIR": str(from_env)},
        )

        self.assertTrue(
            (from_env / "Test Anime").is_dir(),
            f"environment destination was not used: {result.stdout}{result.stderr}",
        )
        self.assertFalse((from_config / "Test Anime").exists())

    def test_empty_list_refresh_does_not_clobber_existing_catalog(self):
        original = "[keep] Keep Me   \n"
        self.anime_list.write_text(original, encoding="utf-8")
        # Set the env override so this test exercises the list-refresh path even
        # on the old, broken startup line.
        result = self.run_script(
            "-s", "missing", "-e", "1", "-l",
            extra_env={"YTDLP_CONCURRENT_FRAGMENTS": "8"},
        )
        self.assertEqual(
            self.anime_list.read_text(encoding="utf-8"),
            original,
            "an empty catalogue response must not replace the local catalogue",
        )
        self.assertNotEqual(result.returncode, 0)

    def run_bash(self, body):
        scenario = self.dir / "scenario.sh"
        scenario.write_text(dedent(body), encoding="utf-8")
        env = dict(os.environ)
        env.update(
            {
                "SCRIPT_PATH": str(self.script),
                "TMP": str(self.dir),
                "BIN": str(self.bin),
            }
        )
        return subprocess.run(
            ["bash", str(scenario)],
            cwd=self.dir,
            env=env,
            capture_output=True,
            text=True,
            timeout=20,
        )

    def test_select_episode_source_preserves_audio_when_resolution_is_missing(self):
        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            _ANIME_AUDIO="eng"
            _ANIME_RESOLUTION="1080"
            select_episode_source <<'SOURCES'
            https://cdn.test/jpn1080.m3u8" data-resolution="1080" data-audio="jpn" data-av1="0">
            https://cdn.test/eng720.m3u8" data-resolution="720" data-audio="eng" data-av1="0">
            SOURCES
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.strip(), "https://cdn.test/eng720.m3u8")
        self.assertIn("fallback to the highest available quality", result.stderr)
        self.assertNotIn("jpn1080", result.stdout)

    def test_select_episode_source_falls_back_when_audio_is_missing(self):
        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            _ANIME_AUDIO="spa"
            _ANIME_RESOLUTION=""
            select_episode_source <<'SOURCES'
            https://cdn.test/eng720.m3u8" data-resolution="720" data-audio="eng" data-av1="0">
            https://cdn.test/jpn1080.m3u8" data-resolution="1080" data-audio="jpn" data-av1="0">
            SOURCES
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.strip(), "https://cdn.test/jpn1080.m3u8")
        self.assertIn("audio language is not available", result.stderr)

    def test_existing_episode_is_skipped_without_resolving_or_downloading(self):
        marker = self.dir / "ytdlp.called"
        link_marker = self.dir / "link.called"
        media = self.dir / "media" / "Test Anime"
        media.mkdir(parents=True)
        (media / "1.mp4").write_text("old-bytes", encoding="utf-8")
        self.make_executable(
            "yt-dlp",
            "#!/usr/bin/env bash\n"
            f'printf called > "{marker}"\n',
        )

        result = self.run_bash(
            f"""
            set -euo pipefail
            source "$SCRIPT_PATH"
            _DOWNLOAD_DIR="$TMP/media"
            _ANIME_NAME="Test Anime"
            _ANIME_AUDIO=""
            _ANIME_RESOLUTION=""
            _CONCURRENT_FRAGMENTS=8
            _REFERER_URL="https://kwik.cx/"
            _LIST_LINK_ONLY=""
            _REPLACE_EXISTING=""
            _YTDLP="$BIN/yt-dlp"
            get_episode_link() {{ printf called > "{link_marker}"; echo "https://example.test/episode"; }}
            get_playlist_link() {{ echo "https://example.test/uwu.m3u8"; }}
            download_episode 1
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((media / "1.mp4").read_text(encoding="utf-8"), "old-bytes")
        self.assertIn("already exists; skipping", result.stderr)
        self.assertFalse(marker.exists(), "yt-dlp must not run for an existing episode")
        self.assertFalse(link_marker.exists(), "skip must happen before link resolution")

    def test_replace_mode_downloads_to_temp_and_swaps_on_success(self):
        media = self.dir / "media" / "Test Anime"
        media.mkdir(parents=True)
        final = media / "1.mp4"
        final.write_text("old-bytes", encoding="utf-8")
        args_log = self.dir / "ytdlp.args"
        self.make_executable(
            "yt-dlp",
            "#!/usr/bin/env bash\n"
            'all="$*"\n'
            'out=""\n'
            'while [[ $# -gt 0 ]]; do\n'
            '  if [[ "$1" == "-o" ]]; then out="$2"; shift 2; continue; fi\n'
            "  shift\n"
            "done\n"
            'mkdir -p "$(dirname "$out")"\n'
            'printf "new-bytes" > "$out"\n'
            f'printf "%s\\n" "$all" >> "{args_log}"\n',
        )

        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            _DOWNLOAD_DIR="$TMP/media"
            _ANIME_NAME="Test Anime"
            _ANIME_AUDIO=""
            _ANIME_RESOLUTION=""
            _CONCURRENT_FRAGMENTS=8
            _REFERER_URL="https://kwik.cx/"
            _LIST_LINK_ONLY=""
            _REPLACE_EXISTING=true
            _YTDLP="$BIN/yt-dlp"
            get_episode_link() { echo "https://example.test/episode"; }
            get_playlist_link() { echo "https://example.test/uwu.m3u8"; }
            download_episode 1
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(final.read_text(encoding="utf-8"), "new-bytes")
        self.assertFalse((media / "1.mp4.new").exists(), "temporary file must be moved away")
        args = args_log.read_text(encoding="utf-8")
        self.assertIn("--abort-on-unavailable-fragments", args)
        self.assertIn("--retry-sleep fragment:2", args)
        self.assertNotIn("--force-overwrites", args)
        self.assertIn("--concurrent-fragments 8", args)
        self.assertIn("Episode 1 replaced.", result.stderr)

    def test_failed_replacement_keeps_the_original_file(self):
        media = self.dir / "media" / "Test Anime"
        media.mkdir(parents=True)
        final = media / "1.mp4"
        final.write_text("old-bytes", encoding="utf-8")
        self.make_executable(
            "yt-dlp",
            "#!/usr/bin/env bash\n"
            'out=""\n'
            'while [[ $# -gt 0 ]]; do\n'
            '  if [[ "$1" == "-o" ]]; then out="$2"; shift 2; continue; fi\n'
            "  shift\n"
            "done\n"
            'printf "partial" > "$out"\n'
            'printf "partial" > "$out.part"\n'
            'printf "state" > "$out.ytdl"\n'
            'printf "frag" > "$out.part-Frag1"\n'
            "exit 1\n",
        )

        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            sleep() { :; }
            _DOWNLOAD_DIR="$TMP/media"
            _ANIME_NAME="Test Anime"
            _ANIME_AUDIO=""
            _ANIME_RESOLUTION=""
            _CONCURRENT_FRAGMENTS=8
            _REFERER_URL="https://kwik.cx/"
            _LIST_LINK_ONLY=""
            _REPLACE_EXISTING=true
            _YTDLP="$BIN/yt-dlp"
            get_episode_link() { echo "https://example.test/episode"; }
            get_playlist_link() { echo "https://example.test/uwu.m3u8"; }
            set +e
            download_episode 1
            status=$?
            set -e
            printf '%s\\n' "$status" > "$TMP/replace.status"
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            (self.dir / "replace.status").read_text(encoding="utf-8").strip(), "1"
        )
        self.assertEqual(final.read_text(encoding="utf-8"), "old-bytes")
        self.assertFalse((media / "1.mp4.new").exists(), "failed temp file must be cleaned up")
        self.assertFalse((media / "1.mp4.new.part").exists(), "partial download must be cleaned up")
        self.assertFalse((media / "1.mp4.new.ytdl").exists(), "resume state must be cleaned up")
        self.assertFalse(
            list(media.glob("1.mp4.new.part-Frag*")), "partial fragments must be cleaned up"
        )
        self.assertIn("failed after 3 attempts", result.stderr)

    def test_retry_reduces_fragment_concurrency_and_starts_clean(self):
        media = self.dir / "media" / "Test Anime"
        media.mkdir(parents=True)
        final = media / "1.mp4"
        frags_log = self.dir / "frags.log"
        self.make_executable(
            "yt-dlp",
            "#!/usr/bin/env bash\n"
            'out=""\n'
            'frags=""\n'
            'while [[ $# -gt 0 ]]; do\n'
            '  case "$1" in\n'
            '    -o) out="$2"; shift 2 ;;\n'
            '    --concurrent-fragments) frags="$2"; shift 2 ;;\n'
            "    *) shift ;;\n"
            "  esac\n"
            "done\n"
            f'printf "%s\\n" "$frags" >> "{frags_log}"\n'
            'count=$(cat "$TMP/attempts" 2>/dev/null || echo 0)\n'
            "count=$((count + 1))\n"
            'printf "%s\\n" "$count" > "$TMP/attempts"\n'
            "if (( count == 1 )); then\n"
            '  printf "partial" > "$out.part"\n'
            '  printf "state" > "$out.ytdl"\n'
            '  printf "frag" > "$out.part-Frag1"\n'
            "  exit 1\n"
            "fi\n"
            '[[ -e "$out.part" || -e "$out.ytdl" || -e "$out.part-Frag1" ]] && exit 9\n'
            'mkdir -p "$(dirname "$out")"\n'
            'printf "done" > "$out"\n',
        )

        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            sleep() { :; }
            _DOWNLOAD_DIR="$TMP/media"
            _ANIME_NAME="Test Anime"
            _ANIME_AUDIO=""
            _ANIME_RESOLUTION=""
            _CONCURRENT_FRAGMENTS=8
            _REFERER_URL="https://kwik.cx/"
            _LIST_LINK_ONLY=""
            _REPLACE_EXISTING=""
            _YTDLP="$BIN/yt-dlp"
            get_episode_link() { echo "https://example.test/episode"; }
            get_playlist_link() { echo "https://example.test/uwu.m3u8"; }
            download_episode 1
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(final.read_text(encoding="utf-8"), "done")
        self.assertEqual(frags_log.read_text(encoding="utf-8").split(), ["8", "4"])
        self.assertIn("retrying with 4 concurrent fragment(s)", result.stderr)
        self.assertFalse((media / "1.mp4.part").exists())
        self.assertFalse((media / "1.mp4.ytdl").exists())
        self.assertFalse(list(media.glob("1.mp4.part-Frag*")))

    def test_link_resolution_is_retried_before_the_episode_fails(self):
        self.make_executable("yt-dlp", "#!/usr/bin/env bash\nexit 0\n")

        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            sleep() { :; }
            _DOWNLOAD_DIR="$TMP/media"
            _ANIME_NAME="Test Anime"
            _ANIME_AUDIO=""
            _ANIME_RESOLUTION=""
            _CONCURRENT_FRAGMENTS=8
            _REFERER_URL="https://kwik.cx/"
            _LIST_LINK_ONLY=""
            _REPLACE_EXISTING=""
            _YTDLP="$BIN/yt-dlp"
            get_episode_link() { printf 'x\\n' >> "$TMP/link.calls"; return 1; }
            get_playlist_link() { printf called > "$TMP/playlist.called"; echo "https://example.test/uwu.m3u8"; }
            set +e
            download_episode 1
            status=$?
            set -e
            printf '%s\\n' "$status" > "$TMP/link.status"
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = (self.dir / "link.calls").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(calls), 3, "link resolution must be attempted three times")
        self.assertEqual(
            (self.dir / "link.status").read_text(encoding="utf-8").strip(), "1"
        )
        self.assertIn("could not resolve the download link (attempt 1 of 3)", result.stderr)
        self.assertFalse((self.dir / "playlist.called").exists())

    def test_get_retries_with_bounded_curl_timeouts(self):
        args_log = self.dir / "curl.args"
        counter = self.dir / "curl.count"
        self.make_executable(
            "curl-impersonate",
            "#!/usr/bin/env bash\n"
            f'count=$(cat "{counter}" 2>/dev/null || echo 0)\n'
            "count=$((count + 1))\n"
            f'printf "%s\\n" "$count" > "{counter}"\n'
            f'printf "%s\\n" "$*" >> "{args_log}"\n'
            '[[ "$count" -eq 1 ]] && exit 28\n'
            'printf "body-ok"\n',
        )

        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            sleep() { :; }
            _CURL="$BIN/curl-impersonate"
            _CF_CLEARANCE="cookie"
            _USER_AGENT="UA"
            _LIST_LINK_ONLY=""
            body="$(get "https://example.test/play")"
            printf '%s' "$body" > "$TMP/body"
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.dir / "body").read_text(encoding="utf-8"), "body-ok")
        self.assertEqual(counter.read_text(encoding="utf-8").strip(), "2")
        args = args_log.read_text(encoding="utf-8")
        self.assertIn("--connect-timeout 10", args)
        self.assertIn("--max-time 30", args)
        self.assertNotIn("cookie", result.stdout, "the cookie must not leak to stdout")

    def test_playlist_resolution_uses_bounded_timeouts(self):
        args_log = self.dir / "curl.args"
        self.make_executable(
            "curl-impersonate",
            "#!/usr/bin/env bash\n"
            f'printf "%s\\n" "$*" >> "{args_log}"\n'
            'if [[ "$*" == *"-X POST"* ]]; then\n'
            '  printf "source=https://cdn.test/uwu.m3u8?token=secret\\n"\n'
            "else\n"
            '  printf "<script>eval(x)</script>\\n"\n'
            "fi\n",
        )

        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            _CURL="$BIN/curl-impersonate"
            _REFERER_HOST="https://animepahe.pw/"
            _LIST_LINK_ONLY=""
            playlist="$(get_playlist_link "https://example.test/episode")"
            printf '%s' "$playlist" > "$TMP/playlist"
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            (self.dir / "playlist").read_text(encoding="utf-8"),
            "https://cdn.test/uwu.m3u8",
        )
        lines = args_log.read_text(encoding="utf-8").splitlines()
        page = next(line for line in lines if "-X POST" not in line)
        post = next(line for line in lines if "-X POST" in line)
        self.assertIn("--connect-timeout 10", page)
        self.assertIn("--max-time 30", page)
        self.assertIn("--connect-timeout 10", post)
        self.assertIn("--max-time 60", post)

    def test_transient_ytdlp_failure_is_retried_with_a_socket_timeout(self):
        counter = self.dir / "attempts"
        args_log = self.dir / "ytdlp.args"
        self.make_executable(
            "yt-dlp",
            "#!/usr/bin/env bash\n"
            f'count=$(cat "{counter}" 2>/dev/null || echo 0)\n'
            "count=$((count + 1))\n"
            f'printf "%s\\n" "$count" > "{counter}"\n'
            f'printf "%s\\n" "$*" >> "{args_log}"\n'
            '[[ "$count" -eq 1 ]] && exit 1\n'
            "exit 0\n",
        )

        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            _DOWNLOAD_DIR="$TMP/media"
            _ANIME_NAME="Test Anime"
            _CONCURRENT_FRAGMENTS=8
            _REFERER_URL="https://kwik.cx/"
            _LIST_LINK_ONLY=""
            _YTDLP="$BIN/yt-dlp"
            get_episode_link() { echo "https://example.test/episode"; }
            get_playlist_link() { echo "https://example.test/uwu.m3u8"; }
            download_episode 1
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(counter.read_text(encoding="utf-8").strip(), "2")
        self.assertIn(
            "--socket-timeout 30",
            args_log.read_text(encoding="utf-8"),
            "yt-dlp must allow slower CDN responses before giving up",
        )
        self.assertIn("attempt 1 failed", result.stderr)

    def test_one_failed_episode_does_not_abort_the_batch(self):
        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            _LOG="$TMP/attempts.log"
            download_episode() {
                printf '%s\\n' "$1" >> "$_LOG"
                [[ "$1" == "1" ]] && return 1
                return 0
            }
            set +e
            ( download_episodes "1,2" ) > "$TMP/batch.out" 2>&1
            status=$?
            set -e
            printf '%s\\n' "$status" > "$TMP/batch.status"
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            (self.dir / "attempts.log").read_text(encoding="utf-8").split(),
            ["1", "2"],
            "the second episode must still be attempted after the first fails",
        )
        self.assertEqual(
            (self.dir / "batch.status").read_text(encoding="utf-8").strip(), "1"
        )
        self.assertIn(
            "1 episode(s) failed",
            (self.dir / "batch.out").read_text(encoding="utf-8"),
        )


    def test_animepahe_curl_fallback_works_without_fzf(self):
        # A GUI run passes -s, so fzf must not be required. The helper should
        # also be selected when curl-impersonate is absent.
        minbin = self.dir / "minbin"
        minbin.mkdir()
        needed = [
            "bash",
            "env",
            "dirname",
            "jq",
            "mkdir",
            "grep",
            "sed",
            "awk",
            "head",
            "tail",
            "sort",
            "seq",
            "mktemp",
            "mv",
            "rm",
            "cat",
            "tee",
            "sleep",
            "expr",
            "tr",
            "cut",
            "uniq",
        ]
        for name in needed:
            target = shutil.which(name)
            if target:
                os.symlink(target, minbin / name)

        helper = minbin / "animepahe-curl"
        helper.write_text(
            '#!/usr/bin/env bash\necho called >> "$CURL_LOG"\nexit 0\n',
            encoding="utf-8",
        )
        helper.chmod(0o755)
        ytdlp = minbin / "yt-dlp"
        ytdlp.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
        ytdlp.chmod(0o755)

        result = self.run_bash(
            """
            set -euo pipefail
            source "$SCRIPT_PATH"
            PATH="$TMP/minbin"
            export PATH
            set_var
            printf '%s' "$_CURL" > "$TMP/curl-value"
            printf '%s' "${_FZF:-}" > "$TMP/fzf-value"
            """
        )

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(
            "animepahe-curl",
            (self.dir / "curl-value").read_text(encoding="utf-8"),
        )
        self.assertEqual((self.dir / "fzf-value").read_text(encoding="utf-8"), "")


if __name__ == "__main__":
    unittest.main()
