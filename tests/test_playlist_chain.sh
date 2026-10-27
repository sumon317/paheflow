#!/usr/bin/env bash
# Verifies the kwik -> glot.io -> m3u8 -> yt-dlp chain used by animepahe-dl.sh
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATH="$ROOT/.uv/bin:$PATH"
export PATH

_CURL="$(command -v curl-impersonate || command -v animepahe-curl || true)"
_YTDLP="$(command -v yt-dlp || true)"
if [[ -z "$_CURL" || -z "$_YTDLP" ]]; then
    echo "RESULT: FAIL - run ./requirements.sh first"
    exit 1
fi
echo "curl client: $_CURL"
echo "yt-dlp:      $_YTDLP"

_REFERER_HOST="https://animepahe.pw/"
_REFERER_URL="https://kwik.cx/"

run_js_code() {
    "$_CURL" -sS -X POST 'https://glot.io/api/mux' \
        -H 'Content-Type: application/json' \
        --data-raw $'{"action":"run","data":{"image":"glot/javascript:latest","payload":{"runInstructions":{"buildCommands":[],"runCommand":"node main.js"},"files":[{"name":"main.js","content":"'"$1"'"}],"stdin":null}}}'
}

get_playlist_link() {
    local s l t
    while read -r t; do
        s="$("$_CURL" --compressed -sS -H "Referer: $_REFERER_HOST" "$t" \
            | grep "<script>eval" \
            | awk -F 'script>' '{print $2}' \
            | sed 's/\\/\\\\/g' \
            | sed 's/"/\\"/g')"

        l="$(run_js_code "$s" \
            | grep 'source=' \
            | sed 's/.m3u8.*/.m3u8/' \
            | sed 's/.*https/https/')"

        if [[ -n "${l:-}" ]]; then
            echo "$l"
            return
        fi
    done <<< "$1"
}

echo "== resolving playlist for kwik URL =="
PLAYLIST="$(get_playlist_link "https://kwik.cx/e/12hRtYH5HQ39")"
echo "playlist: ${PLAYLIST:-<EMPTY>}"

if [[ -z "${PLAYLIST:-}" ]]; then
    echo "RESULT: FAIL - no playlist resolved"
    exit 1
fi

echo "== downloading with yt-dlp (concurrent-fragments 8) =="
rm -f /tmp/aptest.mp4
"$_YTDLP" "$PLAYLIST" --referer "$_REFERER_URL" --impersonate chrome \
    --concurrent-fragments 8 --no-warnings -q --newline \
    --progress-template 'download:%(progress._percent_str)s speed=%(progress._speed_str)s eta=%(progress._eta_str)s' \
    -o /tmp/aptest.mp4 2>&1 | head -5

if [[ -s /tmp/aptest.mp4 ]]; then
    echo "RESULT: PASS - downloaded $(du -h /tmp/aptest.mp4 | cut -f1) to /tmp/aptest.mp4"
    exit 0
fi
echo "RESULT: FAIL - no file produced"
exit 1
