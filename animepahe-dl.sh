#!/usr/bin/env bash
#
# Download anime from animepahe in terminal
#
#/ Usage:
#/   ./animepahe-dl.sh [-a <anime name>] [-s <anime_slug>] [-e <episode_num1,num2,num3-num4...>] [-r <resolution>] [-l] [-d]
#/
#/ Options:
#/   -a <name>               anime name
#/   -s <slug>               anime slug/uuid, can be found in $_ANIME_LIST_FILE
#/                           ignored when "-a" is enabled
#/   -e <num1,num3-num4...>  optional, episode number to download
#/                           multiple episode numbers seperated by ","
#/                           episode range using "-"
#/                           all episodes using "*"
#/   -r <resolution>         optional, specify resolution: "1080", "720"...
#/                           by default, the highest resolution is selected
#/   -o <language>           optional, specify audio language: "eng", "jpn"...
#/   -l                      optional, show m3u8 playlist link without downloading videos
#/   -R                      optional, replace existing episode files (default: skip)
#/   -d                      enable debug mode
#/   -h | --help             display this help message

set -e
set -u

usage() {
    printf "%b\n" "$(grep '^#/' "$0" | cut -c4-)" && exit 1
}

set_var() {
    _SCRIPT_PATH=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

    # Tools installed by requirements.sh live inside the project. Put them
    # first so the launcher and a direct `./animepahe-dl.sh` use the same uv
    # toolchain without changing the user's shell profile.
    local _TOOL_BIN="$_SCRIPT_PATH/.uv/bin"
    if [[ -d "$_TOOL_BIN" ]]; then
        case ":$PATH:" in
            *":$_TOOL_BIN:"*) ;;
            *) PATH="$_TOOL_BIN:$PATH"; export PATH ;;
        esac
    fi

    # Prefer a real curl-impersonate binary when available. The bundled
    # animepahe-curl helper (curl_cffi) is the supported fallback.
    if _CURL="$(command -v curl-impersonate 2>/dev/null)"; then
        :
    elif _CURL="$(command -v animepahe-curl 2>/dev/null)"; then
        :
    else
        _CURL=""
    fi
    [[ -n "$_CURL" ]] || command_not_found "curl-impersonate or animepahe-curl"

    _JQ="$(command -v jq)" || command_not_found "jq"
    _FZF="$(command -v fzf 2>/dev/null || true)"

    # yt-dlp is not needed for link-only runs (-l).
    if [[ -z "${_LIST_LINK_ONLY:-}" ]]; then
        _YTDLP="$(command -v yt-dlp)" || command_not_found "yt-dlp"
    else
        _YTDLP="$(command -v yt-dlp 2>/dev/null || true)"
    fi

    # Concurrent fragments: env > config.json > default 8
    _CONCURRENT_FRAGMENTS="${YTDLP_CONCURRENT_FRAGMENTS:-$("$_JQ" -r '.concurrent_fragments // 8' "$_SCRIPT_PATH/config.json" 2>/dev/null || echo 8)}"
    if [[ ! "$_CONCURRENT_FRAGMENTS" =~ ^[0-9]+$ ]]; then
        _CONCURRENT_FRAGMENTS=8
    elif (( _CONCURRENT_FRAGMENTS < 1 )); then
        _CONCURRENT_FRAGMENTS=1
    elif (( _CONCURRENT_FRAGMENTS > 64 )); then
        _CONCURRENT_FRAGMENTS=64
    fi

    _HOST="https://animepahe.pw"
    _ANIME_URL="$_HOST/anime"
    _API_URL="$_HOST/api"
    _REFERER_URL="https://kwik.cx/"
    _REFERER_HOST="https://animepahe.pw/"

    # Guard the reads so a hand-edited, invalid config.json produces the
    # friendly check_config() error instead of a silent jq exit under set -e.
    _USER_AGENT="$("$_JQ" -r '.ua' "$_SCRIPT_PATH/config.json" 2>/dev/null)" || true
    _CF_CLEARANCE="$("$_JQ" -r '.cf' "$_SCRIPT_PATH/config.json" 2>/dev/null)" || true
    _ANIME_LIST_FILE="$_SCRIPT_PATH/anime.list"
    _SOURCE_FILE=".source.json"

    # Downloads: env > config.json > project directory
    _DOWNLOAD_DIR="${ANIMEPAHE_OUTPUT_DIR:-}"
    if [[ -z "$_DOWNLOAD_DIR" ]]; then
        _DOWNLOAD_DIR="$("$_JQ" -r '.download_dir // ""' "$_SCRIPT_PATH/config.json" 2>/dev/null || true)"
    fi
    if [[ -z "$_DOWNLOAD_DIR" ]]; then
        _DOWNLOAD_DIR="$_SCRIPT_PATH"
    elif [[ "$_DOWNLOAD_DIR" == "~"* ]]; then
        _DOWNLOAD_DIR="$HOME${_DOWNLOAD_DIR:1}"
    fi
    [[ "$_DOWNLOAD_DIR" == /* ]] || _DOWNLOAD_DIR="$_SCRIPT_PATH/$_DOWNLOAD_DIR"
    mkdir -p "$_DOWNLOAD_DIR" || print_error "Cannot create download directory: $_DOWNLOAD_DIR"
}

set_args() {
    expr "$*" : ".*--help" > /dev/null && usage
    _DEFAULT_ANIME_RESOLUTION="1080"
    while getopts ":hlda:s:e:r:o:R" opt; do
        case $opt in
            a)
                _INPUT_ANIME_NAME="$OPTARG"
                ;;
            s)
                _ANIME_SLUG="$OPTARG"
                ;;
            e)
                _ANIME_EPISODE="$OPTARG"
                ;;
            l)
                _LIST_LINK_ONLY=true
                ;;
            r)
                _ANIME_RESOLUTION="$OPTARG"
                ;;
            o)
                _ANIME_AUDIO="$OPTARG"
                ;;
            R)
                _REPLACE_EXISTING=true
                ;;
            d)
                _DEBUG=true
                ;;
            h)
                usage
                ;;
            \?)
                print_error "Invalid option: -$OPTARG"
                ;;
        esac
    done
}

print_info() {
    # $1: info message
    [[ -z "${_LIST_LINK_ONLY:-}" ]] && printf "%b\n" "\033[32m[INFO]\033[0m $1" >&2
}

print_warn() {
    # $1: warning message
    [[ -z "${_LIST_LINK_ONLY:-}" ]] && printf "%b\n" "\033[33m[WARNING]\033[0m $1" >&2
}

print_error() {
    # $1: error message
    printf "%b\n" "\033[31m[ERROR]\033[0m $1" >&2
    exit 1
}

command_not_found() {
    # $1: command name
    print_error "$1 command not found!"
}

require_fzf() {
    [[ -n "${_FZF:-}" ]] || print_error \
        "fzf is required to choose an anime interactively. Install fzf, or pass -s <slug> to run non-interactively."
}

get() {
    # $1: url
    # Disable xtrace while the cookie is part of the command line so that
    # `-d` never prints cf_clearance to the GUI log.
    local _had_xtrace=0 _status=0 _attempt
    case $- in *x*) _had_xtrace=1; set +x ;; esac
    for _attempt in 1 2 3; do
        if "$_CURL" -sS -L "$1" \
            --connect-timeout 10 --max-time 30 \
            -b "cf_clearance=$_CF_CLEARANCE" -A "$_USER_AGENT" --compressed; then
            _status=0
            break
        else
            _status=$?
        fi
        if (( _attempt < 3 )); then
            print_warn "Request failed (attempt $_attempt of 3), retrying..."
            sleep "$_attempt"
        fi
    done
    [[ "$_had_xtrace" == 1 ]] && set -x
    return "$_status"
}

download_anime_list() {
    local tmp
    tmp="$(mktemp "${_ANIME_LIST_FILE}.tmp.XXXXXX")" || return 1
    if get "$_ANIME_URL" \
        | grep "/anime/" \
        | sed -E 's/.*anime\//[/;s/" title="/] /;s/\">.*/   /;s/" title/]/' \
        > "$tmp" && [[ -s "$tmp" ]]; then
        mv -f "$tmp" "$_ANIME_LIST_FILE"
    else
        rm -f "$tmp"
        print_warn "Could not refresh anime.list; keeping the existing catalogue."
        return 1
    fi
}

search_anime_by_name() {
    # $1: anime name
    local d n
    d="$(get "$_HOST/api?m=search&q=${1// /%20}")" \
        || print_error "Search request failed. Check your connection and cf_clearance."
    n="$("$_JQ" -r '.total' <<< "$d" 2>/dev/null)" || true
    [[ -z "${n:-}" ]] && print_error "No search result... Need a new cf value in config.json"
    if [[ "$n" -eq "0" ]]; then
        echo ""
    else
        "$_JQ" -r '.data[] | "[\(.session)] \(.title)   "' <<< "$d" \
            | tee -a "$_ANIME_LIST_FILE" \
            | remove_slug
    fi
}

get_episode_list() {
    # $1: anime id
    # $2: page number
    get "${_API_URL}?m=release&id=${1}&sort=episode_asc&page=${2}"
}

download_source() {
    local d p n
    mkdir -p "$_DOWNLOAD_DIR/$_ANIME_NAME"
    d="$(get_episode_list "$_ANIME_SLUG" "1")" \
        || print_error "Could not fetch the episode list. Check your connection and cf_clearance."
    # jq exits 5 on non-JSON input (a Cloudflare challenge page); guard the
    # assignment so set -e cannot abort before the friendly error below.
    p="$("$_JQ" -r '.last_page' <<< "$d" 2>/dev/null)" || true
    [[ -z "${p:-}" ]] && print_error "No search result... Need a new cf value in config.json"

    if [[ "$p" -gt "1" ]]; then
        for i in $(seq 2 "$p"); do
            n="$(get_episode_list "$_ANIME_SLUG" "$i")" \
                || print_error "Could not fetch episode list page $i."
            d="$(echo "$d $n" | "$_JQ" -s '.[0].data + .[1].data | {data: .}')"
        done
    fi

    echo "$d" > "$_DOWNLOAD_DIR/$_ANIME_NAME/$_SOURCE_FILE"
}

select_episode_source() {
    # Read candidate source lines from stdin and print the best matching URL.
    # The requested audio track is preserved when the requested resolution is
    # missing: the highest resolution available in that track is used instead.
    local lines base selected="" audio_matches resolution_matches
    lines="$(cat)"

    base="$lines"
    if [[ -n "${_ANIME_AUDIO:-}" ]]; then
        print_info "Select audio language: $_ANIME_AUDIO"
        audio_matches="$(grep 'data-audio="'"$_ANIME_AUDIO"'"' <<< "$lines" || true)"
        if [[ -n "$audio_matches" ]]; then
            base="$audio_matches"
        else
            print_warn "Selected audio language is not available, fallback to default."
        fi
    fi

    if [[ -n "${_ANIME_RESOLUTION:-}" ]]; then
        print_info "Select video resolution: ${_ANIME_RESOLUTION}p"
        resolution_matches="$(grep 'data-resolution="'"$_ANIME_RESOLUTION"'"' <<< "$base" || true)"
        if [[ -n "$resolution_matches" ]]; then
            selected="$resolution_matches"
        else
            print_warn "Selected video resolution is not available, fallback to the highest available quality."
        fi
    fi

    if [[ -z "$selected" ]]; then
        # Prefer kwik sources, then take the highest resolution available.
        selected="$(grep kwik <<< "$base" || true)"
        [[ -z "$selected" ]] && selected="$base"
        selected="$(sed -E 's/.*data-resolution="([0-9]+)".*/\1 &/' <<< "$selected" \
            | sort -nr \
            | head -1 \
            | sed -E 's/^[0-9]+ //')"
    fi

    head -1 <<< "$selected" | awk -F '" ' '{print $1}'
}

get_episode_link() {
    # $1: episode number
    local s o l
    s=$("$_JQ" -r '.data[] | select((.episode | tonumber) == ($num | tonumber)) | .session' --arg num "$1" < "$_DOWNLOAD_DIR/$_ANIME_NAME/$_SOURCE_FILE" 2>/dev/null) || true
    if [[ "$s" == "" ]]; then
        print_warn "Episode $1 not found!"
        return 2
    fi

    if ! o="$(get "${_HOST}/play/${_ANIME_SLUG}/${s}")"; then
        print_warn "Episode $1: could not load the play page."
        return 1
    fi

    l="$(grep \<button <<< "$o" \
        | grep data-src \
        | sed -E 's/data-src="/\n/g' \
        | grep 'data-av1="0"' || true)"
    if [[ -z "$l" ]]; then
        print_warn "Episode $1: no playable source found."
        return 1
    fi

    l="$(select_episode_source <<< "$l")" || return 1
    if [[ -z "$l" ]]; then
        print_warn "Episode $1: no playable source found."
        return 1
    fi
    echo "$l"
}

run_js_code() {
    # $1: js code
    "$_CURL" -sS -X POST 'https://glot.io/api/mux' \
        --connect-timeout 10 --max-time 60 \
        -H 'Content-Type: application/json' \
        --data-raw $'{"action":"run","data":{"image":"glot/javascript:latest","payload":{"runInstructions":{"buildCommands":[],"runCommand":"node main.js"},"files":[{"name":"main.js","content":"'"$1"'"}],"stdin":null}}}'
}

get_playlist_link() {
    # $1: episode link
    local s l t
    while read -r t; do
        [[ -z "$t" ]] && continue
        s="$("$_CURL" --compressed -sS -H "Referer: $_REFERER_HOST" \
            --connect-timeout 10 --max-time 30 "$t" \
            | grep "<script>eval" \
            | awk -F 'script>' '{print $2}' \
            | sed 's/\\/\\\\/g' \
            | sed 's/"/\\"/g')"

        if [[ -z "${s:-}" ]]; then
            print_warn "Could not read the stream page."
            continue
        fi

        l="$(run_js_code "$s" \
            | grep 'source=' \
            | sed 's/.m3u8.*/.m3u8/' \
            | sed 's/.*https/https/')"

        if [[ -n "${l:-}" ]]; then
            echo "$l"
            return 0
        fi
        print_warn "Stream resolver did not return a playlist."
    done <<< "$1"
    return 1
}

download_episodes() {
    # $1: episode number string
    local origel el uniqel failures=0
    origel=()
    if [[ "$1" == *","* ]]; then
        IFS="," read -ra ADDR <<< "$1"
        for n in "${ADDR[@]}"; do
            origel+=("$n")
        done
    else
        origel+=("$1")
    fi

    el=()
    for i in "${origel[@]}"; do
        if [[ "$i" == *"*"* ]]; then
            local eps fst lst
            eps="$("$_JQ" -r '.data[].episode' "$_DOWNLOAD_DIR/$_ANIME_NAME/$_SOURCE_FILE" | sort -nu)"
            fst="$(head -1 <<< "$eps")"
            lst="$(tail -1 <<< "$eps")"
            i="${fst}-${lst}"
        fi

        if [[ "$i" == *"-"* ]]; then
            s=$(awk -F '-' '{print $1}' <<< "$i")
            e=$(awk -F '-' '{print $2}' <<< "$i")
            for n in $(seq "$s" "$e"); do
                el+=("$n")
            done
        else
            el+=("$i")
        fi
    done

    IFS=" " read -ra uniqel <<< "$(printf '%s\n' "${el[@]}" | sort -n -u | tr '\n' ' ')"

    [[ ${#uniqel[@]} == 0 ]] && print_error "Wrong episode number!"

    for e in "${uniqel[@]}"; do
        # One failed episode must not abandon the rest of the batch. The
        # failure is counted and the script exits non-zero after all attempts.
        if ! download_episode "$e"; then
            failures=$((failures + 1))
        fi
    done

    if (( failures > 0 )); then
        print_error "$failures episode(s) failed."
    fi
}

download_episode() {
    # $1: episode number
    local num="$1" l pl v tmp attempt rc output frags
    v="$_DOWNLOAD_DIR/${_ANIME_NAME}/${num}.mp4"
    tmp="${v}.new"

    if [[ -n "${_LIST_LINK_ONLY:-}" ]]; then
        rc=0
        l=$(get_episode_link "$num") || rc=$?
        if (( rc != 0 )) || [[ "$l" != *"/"* ]]; then
            print_warn "Wrong download link or episode $1 not found!"
            return 1
        fi
        if ! pl=$(get_playlist_link "$l"); then
            print_warn "Missing video list! Skip downloading!"
            return 1
        fi
        echo "$pl"
        return 0
    fi

    if [[ -f "$v" && -z "${_REPLACE_EXISTING:-}" ]]; then
        print_warn "Episode $1 already exists; skipping (use -R to replace)."
        return 0
    fi

    output="$v"
    if [[ -n "${_REPLACE_EXISTING:-}" ]]; then
        # Never destroy a working episode until the replacement is complete.
        output="$tmp"
    fi

    # CDN hosts occasionally time out. Re-resolve the playlist before each
    # attempt so a stale stream URL is replaced. Link-resolution failures are
    # retried here too instead of aborting the batch on the first timeout.
    for attempt in 1 2 3; do
        rc=0
        l=$(get_episode_link "$num") || rc=$?
        if (( rc == 2 )); then
            print_warn "Episode $1 not found; skipping."
            return 1
        fi
        if (( rc != 0 )) || [[ -z "${l:-}" || "$l" != *"/"* ]]; then
            print_warn "Episode $1: could not resolve the download link (attempt $attempt of 3)."
        else
            pl=""
            if ! pl=$(get_playlist_link "$l"); then
                pl=""
            fi
            if [[ -z "$pl" ]]; then
                print_warn "Episode $1: missing video list (attempt $attempt of 3)."
            else
                # The CDN rate-limits bursts of fragment requests. Reduce
                # concurrency on retries so an episode can still finish.
                frags="$_CONCURRENT_FRAGMENTS"
                if (( attempt == 2 )); then
                    (( frags > 4 )) && frags=4
                elif (( attempt >= 3 )); then
                    frags=1
                fi
                (( frags >= 1 )) || frags=1
                if (( frags != _CONCURRENT_FRAGMENTS )); then
                    print_warn "Episode $1: retrying with $frags concurrent fragment(s)."
                fi

                # Start every attempt from a clean slate. yt-dlp resumes its
                # .part/.ytdl state by default; mixing that with a fresh retry
                # can append duplicate fragments and corrupt the episode.
                rm -f "$output" "$output".part "$output".ytdl \
                    "$output".temp.* "$output".part-Frag*

                print_info "Downloading Episode $1..."
                if "$_YTDLP" "$pl" --referer "$_REFERER_URL" --impersonate chrome \
                    --concurrent-fragments "$frags" --no-warnings -q --progress \
                    --socket-timeout 30 --fragment-retries 10 \
                    --retry-sleep fragment:2 \
                    --abort-on-unavailable-fragments \
                    -o "$output"; then
                    if [[ -n "${_REPLACE_EXISTING:-}" ]]; then
                        if mv -f "$tmp" "$v"; then
                            print_info "Episode $1 replaced."
                            return 0
                        else
                            print_warn "Episode $1 downloaded, but could not replace the existing file."
                            rc=1
                        fi
                    else
                        return 0
                    fi
                else
                    rc=$?
                    print_warn "Episode $1 download attempt $attempt failed with exit code $rc."
                fi
            fi
        fi

        if (( attempt < 3 )); then
            print_warn "Episode $1 will be retried."
            sleep "$((attempt * 2))"
        fi
    done

    # Never leave a partial replacement or resumable state behind.
    rm -f "$output" "$output".part "$output".ytdl \
        "$output".temp.* "$output".part-Frag*
    print_warn "Episode $1 failed after 3 attempts."
    return 1
}

select_episodes_to_download() {
    [[ "$(grep 'data' -c "$_DOWNLOAD_DIR/$_ANIME_NAME/$_SOURCE_FILE")" -eq "0" ]] && print_error "No episode available!"
    "$_JQ" -r '.data[] | "[\(.episode | tonumber)] E\(.episode | tonumber) \(.created_at)"' "$_DOWNLOAD_DIR/$_ANIME_NAME/$_SOURCE_FILE" >&2
    echo -n "Which episode(s) to download: " >&2
    read -r s
    echo "$s"
}

remove_brackets() {
    awk -F']' '{print $1}' | sed -E 's/^\[//'
}

remove_slug() {
    awk '{$1="";print}' | awk '{$1=$1;print}'
}

get_slug_from_name() {
    # $1: anime name
    grep -F "] $1" "$_ANIME_LIST_FILE" | tail -1 | remove_brackets
}

check_config() {
    if [[ -z "${_CF_CLEARANCE:-}" ]]; then
        print_error "Missing cf_clearance, please add it in config.json!"
    fi
    if [[ -z "${_USER_AGENT:-}" ]]; then
        print_error "Missing user-agent, please add it in config.json!"
    fi
}

main() {
    set_args "$@"
    set_var
    check_config

    # Enable tracing only after the cookie has been read, so `-d` cannot print
    # the credential through the `_CF_CLEARANCE=...` assignment.
    [[ -n "${_DEBUG:-}" ]] && set -x

    if [[ -n "${_INPUT_ANIME_NAME:-}" ]]; then
        require_fzf
        _ANIME_NAME=$("$_FZF" -1 <<< "$(search_anime_by_name "$_INPUT_ANIME_NAME")")
        _ANIME_SLUG="$(get_slug_from_name "$_ANIME_NAME")"
    elif [[ -n "${_ANIME_SLUG:-}" ]]; then
        # A catalog refresh is only needed when the requested slug is absent.
        if ! grep -qF "[${_ANIME_SLUG}]" "$_ANIME_LIST_FILE" 2>/dev/null; then
            download_anime_list || print_error "Anime slug not found!"
        fi
    else
        require_fzf
        download_anime_list
        _ANIME_NAME=$("$_FZF" -1 <<< "$(remove_slug < "$_ANIME_LIST_FILE")")
        _ANIME_SLUG="$(get_slug_from_name "$_ANIME_NAME")"
    fi

    [[ "$_ANIME_SLUG" == "" ]] && print_error "Anime slug not found!"
    _ANIME_NAME="$(grep "$_ANIME_SLUG" "$_ANIME_LIST_FILE" \
        | tail -1 \
        | remove_slug \
        | sed -E 's/[[:space:]]+$//' \
        | sed -E 's/[^[:alnum:] ,\+\-\)\(]/_/g')"

    if [[ "$_ANIME_NAME" == "" ]]; then
        print_warn "Anime name not found! Try again."
        download_anime_list
        exit 1
    fi

    download_source

    [[ -z "${_ANIME_EPISODE:-}" ]] && _ANIME_EPISODE=$(select_episodes_to_download)
    download_episodes "$_ANIME_EPISODE"
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
