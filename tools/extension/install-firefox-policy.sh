#!/usr/bin/env bash
# Install AnimePahe GUI Auto-Sync Extension Permanently in Firefox via Enterprise Policy
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
XPI_PATH="$SCRIPT_DIR/animepahe-autosync.xpi"

echo "=================================================="
echo "  AnimePahe GUI — Permanent Firefox Extension Setup"
echo "=================================================="

# Package extension into .xpi
echo "Packaging animepahe-autosync.xpi..."
(cd "$SCRIPT_DIR" && zip -FS -r animepahe-autosync.xpi \
  manifest.json \
  background.js \
  content_animepahe.js \
  content_gui.js \
  popup.html \
  popup.js >/dev/null)

POLICY_JSON="{
  \"policies\": {
    \"ExtensionSettings\": {
      \"animepahe-autosync@local\": {
        \"installation_mode\": \"normal_installed\",
        \"install_url\": \"file://$XPI_PATH\"
      }
    }
  }
}"

echo "Configuring Firefox Enterprise Policy in /etc/firefox/policies/policies.json..."
if [[ $EUID -ne 0 ]]; then
  echo "Root privileges needed to write /etc/firefox/policies/policies.json"
  sudo mkdir -p /etc/firefox/policies
  echo "$POLICY_JSON" | sudo tee /etc/firefox/policies/policies.json >/dev/null
else
  mkdir -p /etc/firefox/policies
  echo "$POLICY_JSON" > /etc/firefox/policies/policies.json
fi

echo ""
echo "✅ Successfully installed!"
echo "Now restart Firefox. The extension will remain permanently installed across all browser restarts."
echo "=================================================="
