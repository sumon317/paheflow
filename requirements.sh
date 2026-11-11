#!/usr/bin/env bash
#
# AnimePahe GUI dependency installer.
#
# Supported: Linux (dnf, apt, pacman, zypper), macOS (Homebrew), and WSL2.
# The script installs system packages, bootstraps uv when needed, and installs
# yt-dlp plus the project's animepahe-curl helper into .uv/bin.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

UV_DIR="$ROOT/.uv"
TOOL_DIR="$ROOT/.uv/tools"
TOOL_BIN="$ROOT/.uv/bin"
PYTHON_DIR="$ROOT/.uv/python"

MODE="install"
ASSUME_YES=0
INSTALL_OPTIONAL=1

PLATFORM=""
PM=""
UV_BIN=""
PYTHON_CMD=""
MANAGED_PYTHON=""
APT_UPDATED=0

if [[ -t 1 ]]; then
    C_BOLD=$'\033[1m'
    C_OK=$'\033[32m'
    C_WARN=$'\033[33m'
    C_ERR=$'\033[31m'
    C_DIM=$'\033[2m'
    C_OFF=$'\033[0m'
else
    C_BOLD=""
    C_OK=""
    C_WARN=""
    C_ERR=""
    C_DIM=""
    C_OFF=""
fi

info() { printf '%s==>%s %s\n' "$C_BOLD" "$C_OFF" "$*"; }
ok()   { printf '%s[ok]%s %s\n' "$C_OK" "$C_OFF" "$*"; }
warn() { printf '%s[warn]%s %s\n' "$C_WARN" "$C_OFF" "$*" >&2; }
fail() { printf '%serror:%s %s\n' "$C_ERR" "$C_OFF" "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
Usage: ./requirements.sh [OPTIONS]

Install or verify the dependencies for AnimePahe GUI.

Options:
  --check        Only report missing dependencies; do not install anything.
  --yes, -y      Do not ask for confirmation before installing.
  --no-optional  Skip optional packages (fzf, mpv).
  -h, --help     Show this help.

Required for the native Rust GUI: Rust/Cargo, ffmpeg, and either
curl-impersonate or the bundled animepahe-curl helper.
The legacy Python/bash CLI additionally uses Python 3.11+ and yt-dlp.

This script installs ffmpeg plus yt-dlp and animepahe-curl into .uv/bin.
It does not install Rust; install it from https://rustup.rs if needed.
config.json and cookies are never read or modified by this script.
EOF
}

while (( $# > 0 )); do
    case "$1" in
        --check) MODE="check" ;;
        --yes|-y) ASSUME_YES=1 ;;
        --no-optional) INSTALL_OPTIONAL=0 ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown option: $1 (try --help)" ;;
    esac
    shift
done

has_cmd() {
    command -v "$1" >/dev/null 2>&1
}

python_ok() {
    [[ -n "$1" ]] || return 1
    "$1" -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)' \
        >/dev/null 2>&1
}

detect_python() {
    local candidate
    PYTHON_CMD=""
    for candidate in \
        "$TOOL_BIN/python3" \
        python3 python3.14 python3.13 python3.12 python3.11 python; do
        if python_ok "$candidate"; then
            if [[ "$candidate" == */* ]]; then
                PYTHON_CMD="$candidate"
            else
                PYTHON_CMD="$(command -v "$candidate")"
            fi
            return 0
        fi
    done
    return 1
}

detect_platform() {
    case "$(uname -s)" in
        Darwin)
            PLATFORM="macOS"
            PM="brew"
            has_cmd brew || fail \
                "Homebrew is required on macOS. Install it from https://brew.sh and rerun this script."
            ;;
        Linux)
            if grep -qi microsoft /proc/version 2>/dev/null; then
                PLATFORM="WSL2"
            else
                PLATFORM="Linux"
            fi
            if has_cmd dnf; then
                PM="dnf"
            elif has_cmd apt-get; then
                PM="apt"
            elif has_cmd pacman; then
                PM="pacman"
            elif has_cmd zypper; then
                PM="zypper"
            elif has_cmd brew; then
                PM="brew"
            else
                fail "No supported package manager found (dnf, apt-get, pacman, zypper, brew)."
            fi
            ;;
        *)
            fail "Unsupported operating system: $(uname -s). Linux, macOS, and WSL2 are supported."
            ;;
    esac
}

run_root() {
    if (( EUID == 0 )); then
        "$@"
    else
        sudo "$@"
    fi
}

apt_update_once() {
    if (( APT_UPDATED == 0 )); then
        info "Refreshing apt package lists..."
        run_root apt-get update
        APT_UPDATED=1
    fi
}

pm_install() {
    (( $# > 0 )) || return 0
    case "$PM" in
        dnf)
            run_root dnf install -y "$@"
            ;;
        apt)
            apt_update_once
            run_root env DEBIAN_FRONTEND=noninteractive apt-get install -y "$@"
            ;;
        pacman)
            run_root pacman -S --needed --noconfirm "$@"
            ;;
        zypper)
            run_root zypper --non-interactive install "$@"
            ;;
        brew)
            brew install "$@"
            ;;
        *)
            fail "Unsupported package manager: $PM"
            ;;
    esac
}

install_ffmpeg() {
    local pkg
    local candidates=()
    case "$PM" in
        dnf) candidates=(ffmpeg-free ffmpeg) ;;
        zypper) candidates=(ffmpeg ffmpeg-7 ffmpeg-6 ffmpeg-5 ffmpeg-4) ;;
        *) candidates=(ffmpeg) ;;
    esac
    for pkg in "${candidates[@]}"; do
        info "Installing ffmpeg package: $pkg"
        if pm_install "$pkg"; then
            hash -r 2>/dev/null || true
            if has_cmd ffmpeg; then
                return 0
            fi
        fi
        warn "Package '$pkg' did not provide ffmpeg; trying the next candidate."
    done
    return 1
}

find_tool() {
    # Resolve a command, preferring the project-local uv tool directory.
    local name="$1"
    if [[ -x "$TOOL_BIN/$name" ]]; then
        printf '%s\n' "$TOOL_BIN/$name"
        return 0
    fi
    command -v "$name" 2>/dev/null || true
}

find_cloudflare_client() {
    local path
    path="$(find_tool curl-impersonate)"
    if [[ -n "$path" ]]; then
        printf '%s\n' "$path"
        return 0
    fi
    find_tool animepahe-curl
}

find_existing_uv() {
    local path
    if path="$(command -v uv 2>/dev/null)" && [[ -n "$path" ]]; then
        printf '%s\n' "$path"
        return 0
    fi
    for path in "$UV_DIR/uv" "$UV_DIR/bin/uv"; do
        if [[ -x "$path" ]]; then
            printf '%s\n' "$path"
            return 0
        fi
    done
    return 1
}

install_uv() {
    if UV_BIN="$(find_existing_uv)"; then
        return 0
    fi

    info "uv is not installed; installing it into $UV_DIR"
    local installer
    installer="$(mktemp "${TMPDIR:-/tmp}/animepahe-uv.XXXXXX")"
    mkdir -p "$UV_DIR"
    if has_cmd curl; then
        if ! curl -L -fS https://astral.sh/uv/install.sh -o "$installer"; then
            rm -f "$installer"
            fail "Could not download the uv installer."
        fi
    elif has_cmd wget; then
        if ! wget -O "$installer" https://astral.sh/uv/install.sh; then
            rm -f "$installer"
            fail "Could not download the uv installer."
        fi
    else
        rm -f "$installer"
        fail "curl or wget is required to download uv."
    fi
    if ! UV_INSTALL_DIR="$UV_DIR" UV_NO_MODIFY_PATH=1 sh "$installer"; then
        rm -f "$installer"
        fail "uv installation failed."
    fi
    rm -f "$installer"

    if [[ -x "$UV_DIR/uv" ]]; then
        UV_BIN="$UV_DIR/uv"
    elif [[ -x "$UV_DIR/bin/uv" ]]; then
        UV_BIN="$UV_DIR/bin/uv"
    else
        fail "uv was installed but the uv executable was not found in $UV_DIR."
    fi
    "$UV_BIN" --version
}

ensure_managed_python() {
    info "Python 3.11+ is not available; installing Python 3.12 with uv..."
    UV_PYTHON_INSTALL_DIR="$PYTHON_DIR" \
        "$UV_BIN" python install --no-bin 3.12
    MANAGED_PYTHON="$(
        UV_PYTHON_INSTALL_DIR="$PYTHON_DIR" \
            "$UV_BIN" python find --managed-python 3.12
    )"
    [[ -n "$MANAGED_PYTHON" && -x "$MANAGED_PYTHON" ]] \
        || fail "uv could not locate the managed Python 3.12 interpreter."
    "$MANAGED_PYTHON" -c 'import sys; print("Python", sys.version.split()[0], "installed")'
}

install_tools() {
    export UV_TOOL_DIR="$TOOL_DIR"
    export UV_TOOL_BIN_DIR="$TOOL_BIN"
    export UV_PYTHON_INSTALL_DIR="$PYTHON_DIR"
    mkdir -p "$TOOL_BIN"
    export PATH="$TOOL_BIN:$PATH"

    info "Installing yt-dlp with curl_cffi support..."
    "$UV_BIN" tool install --upgrade --python "$PYTHON_CMD" \
        "yt-dlp[default,curl-cffi]"

    info "Installing the animepahe-curl helper..."
    "$UV_BIN" tool install --force --python "$PYTHON_CMD" \
        "$ROOT/tools/animepahe_curl"

    # The managed interpreter is only used when the system Python is too old.
    # Create the shim after uv tool installs so it cannot be overwritten by a
    # tool reinstall.
    if [[ -n "$MANAGED_PYTHON" ]]; then
        rm -f "$TOOL_BIN/python3" "$TOOL_BIN/python"
        ln -s "$MANAGED_PYTHON" "$TOOL_BIN/python3"
        ln -s "$MANAGED_PYTHON" "$TOOL_BIN/python"
        PYTHON_CMD="$TOOL_BIN/python3"
    fi
}

report_line() {
    # $1: status (ok|missing|optional), $2: label, $3: detail
    case "$1" in
        ok) printf '  %s[ok]%s   %-22s %s\n' "$C_OK" "$C_OFF" "$2" "$3" ;;
        missing) printf '  %s[!!]%s   %-22s %s\n' "$C_ERR" "$C_OFF" "$2" "$3" ;;
        optional) printf '  %s[--]%s   %-22s %s\n' "$C_WARN" "$C_OFF" "$2" "$3" ;;
    esac
}

check_requirements() {
    local failures=0
    local path ytdlp helper version

    detect_python || true

    if [[ -n "$PYTHON_CMD" ]] && python_ok "$PYTHON_CMD"; then
        version="$("$PYTHON_CMD" -c 'import sys; print(sys.version.split()[0])' 2>/dev/null)"
        report_line ok "Python 3.11+" "$PYTHON_CMD (Python $version)"
    else
        report_line missing "Python 3.11+" "not found; run ./requirements.sh"
        failures=$((failures + 1))
    fi

    if path="$(command -v jq 2>/dev/null)"; then
        report_line ok "jq" "$path"
    else
        report_line missing "jq" "run ./requirements.sh"
        failures=$((failures + 1))
    fi

    if path="$(command -v ffmpeg 2>/dev/null)"; then
        report_line ok "ffmpeg" "$path"
    else
        report_line missing "ffmpeg" "run ./requirements.sh"
        failures=$((failures + 1))
    fi

    ytdlp="$(find_tool yt-dlp)"
    if [[ -n "$ytdlp" ]]; then
        if "$ytdlp" --list-impersonate-targets 2>/dev/null | grep -q "curl_cffi"; then
            report_line ok "yt-dlp" "$ytdlp (curl_cffi impersonation)"
        else
            report_line missing "yt-dlp" "$ytdlp has no curl_cffi support; run ./requirements.sh"
            failures=$((failures + 1))
        fi
    else
        report_line missing "yt-dlp" "run ./requirements.sh"
        failures=$((failures + 1))
    fi

    helper="$(find_cloudflare_client)"
    if [[ -n "$helper" ]]; then
        if [[ "$helper" == *animepahe-curl ]]; then
            if "$helper" --version >/dev/null 2>&1; then
                report_line ok "Cloudflare client" "$helper"
            else
                report_line missing "Cloudflare client" "$helper cannot start (missing curl_cffi?)"
                failures=$((failures + 1))
            fi
        else
            report_line ok "Cloudflare client" "$helper"
        fi
    else
        report_line missing "Cloudflare client" "curl-impersonate or animepahe-curl not found"
        failures=$((failures + 1))
    fi

    if [[ -n "$UV_BIN" ]] || has_cmd uv; then
        report_line ok "uv" "${UV_BIN:-$(command -v uv)}"
    else
        report_line optional "uv" "not installed (run ./requirements.sh)"
    fi

    if path="$(find_tool fzf)" && [[ -n "$path" ]]; then
        report_line ok "fzf (optional)" "$path"
    else
        report_line optional "fzf (optional)" "interactive terminal picker; GUI does not need it"
    fi

    if path="$(find_tool mpv)" && [[ -n "$path" ]]; then
        report_line ok "mpv (optional)" "$path"
    else
        report_line optional "mpv (optional)" "playlist link playback only"
    fi

    if (( failures > 0 )); then
        printf '\n%s%d required dependency check(s) failed.%s\n' \
            "$C_ERR" "$failures" "$C_OFF" >&2
        return 1
    fi
    return 0
}

# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------

detect_platform
detect_python || true

info "Platform: $PLATFORM ($PM)"

if [[ "$MODE" == "check" ]]; then
    UV_BIN="$(find_existing_uv 2>/dev/null || true)"
    check_requirements
    exit $?
fi

required_pkgs=()
ffmpeg_missing=0
optional_pkgs=()

has_cmd jq || required_pkgs+=("jq")
has_cmd ffmpeg || ffmpeg_missing=1
if (( INSTALL_OPTIONAL == 1 )); then
    has_cmd fzf || optional_pkgs+=("fzf")
    has_cmd mpv || optional_pkgs+=("mpv")
fi

printf '\n'
if (( ${#required_pkgs[@]} > 0 )); then
    info "System packages to install: ${required_pkgs[*]}"
fi
if (( ffmpeg_missing == 1 )); then
    info "System packages to install: ffmpeg"
fi
if (( ${#optional_pkgs[@]} > 0 )); then
    info "Optional packages: ${optional_pkgs[*]}"
fi
if ! find_existing_uv >/dev/null; then
    info "uv: install into .uv/"
fi
info "Python tools: yt-dlp[default,curl-cffi] and animepahe-curl into .uv/bin"
printf '\n'

if (( ASSUME_YES == 0 )); then
    if [[ ! -t 0 ]]; then
        fail "No terminal available for confirmation. Re-run with --yes."
    fi
    read -r -p "Proceed with installation? [Y/n] " answer
    case "$answer" in
        ""|y|Y|yes|YES) ;;
        *) echo "Cancelled."; exit 1 ;;
    esac
fi

# Ask for the password once up front so package output is not interrupted.
needs_root=0
if (( ${#required_pkgs[@]} > 0 || ffmpeg_missing == 1 || ${#optional_pkgs[@]} > 0 )); then
    needs_root=1
fi
if (( needs_root == 1 )) && [[ "$PM" != "brew" ]] && (( EUID != 0 )); then
    has_cmd sudo || fail "sudo is required to install system packages."
    if sudo -n true 2>/dev/null; then
        # Credentials are cached (or the user has passwordless sudo).
        :
    elif [[ -t 0 && -t 2 ]]; then
        info "Administrator access is needed to install system packages."
        sudo -v || fail "sudo authentication failed."
    elif (( ${#required_pkgs[@]} > 0 || ffmpeg_missing == 1 )); then
        fail "Administrator access is required to install required packages, but no terminal is available for the sudo password. Run this script interactively, or install them with $PM and rerun."
    else
        warn "No terminal is available for the sudo password; skipping optional packages: ${optional_pkgs[*]}"
        optional_pkgs=()
        needs_root=0
    fi
fi

if (( ${#required_pkgs[@]} > 0 )); then
    info "Installing required packages: ${required_pkgs[*]}"
    pm_install "${required_pkgs[@]}" || fail "Could not install: ${required_pkgs[*]}"
    hash -r 2>/dev/null || true
fi

if (( ffmpeg_missing == 1 )); then
    install_ffmpeg || fail "Could not install ffmpeg with $PM."
fi

if (( ${#optional_pkgs[@]} > 0 )); then
    for pkg in "${optional_pkgs[@]}"; do
        info "Installing optional package: $pkg"
        if ! pm_install "$pkg"; then
            warn "Optional package '$pkg' could not be installed; continuing."
        fi
    done
    hash -r 2>/dev/null || true
fi

if ! has_cmd jq; then
    fail "jq is still missing after installation."
fi
if ! has_cmd ffmpeg; then
    fail "ffmpeg is still missing after installation."
fi

install_uv
detect_python || true
if [[ -z "$PYTHON_CMD" ]]; then
    ensure_managed_python
    PYTHON_CMD="$MANAGED_PYTHON"
fi

install_tools

printf '\n'
info "Verifying the installation..."
export PATH="$TOOL_BIN:$PATH"
UV_BIN="${UV_BIN:-$(command -v uv 2>/dev/null || true)}"
if check_requirements; then
    printf '\n%sSetup complete.%s Start the GUI with:\n\n' "$C_OK" "$C_OFF"
    printf '    ./animepahe-gui\n\n'
    printf 'Then open Settings, paste your cf_clearance cookie, click\n'
    printf '"Use this browser'"'"'s User-Agent", and save.\n'
else
    fail "Verification failed. Review the messages above."
fi
