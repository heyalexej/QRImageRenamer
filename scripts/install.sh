#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Install qrir from GitHub Releases.

Usage:
  install.sh --repo ORG/REPO [--version vX.Y.Z] [--prefix DIR]

Examples:
  ./install.sh --repo YOUR_ORG/YOUR_REPO
  ./install.sh --repo YOUR_ORG/YOUR_REPO --version v0.1.0
  ./install.sh --repo YOUR_ORG/YOUR_REPO --prefix "$HOME/.local"
EOF
}

REPO=""
VERSION=""
PREFIX="${HOME}/.local"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      REPO="${2:-}"
      shift 2
      ;;
    --version)
      VERSION="${2:-}"
      shift 2
      ;;
    --prefix)
      PREFIX="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown arg: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$REPO" ]]; then
  echo "--repo is required (example: --repo YOUR_ORG/YOUR_REPO)" >&2
  exit 2
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required" >&2
  exit 2
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required (used for GitHub API JSON parsing)" >&2
  exit 2
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}:${ARCH}" in
  Darwin:x86_64)
    TARGET="x86_64-apple-darwin"
    ;;
  Linux:x86_64)
    TARGET="x86_64-unknown-linux-gnu"
    ;;
  *)
    echo "Unsupported platform: ${OS}/${ARCH}" >&2
    echo "Supported: macOS x86_64, Linux x86_64" >&2
    exit 2
    ;;
esac

if [[ -z "$VERSION" ]]; then
  VERSION="$(python3 - "$REPO" <<'PY'
import json
import sys
import urllib.request

repo = sys.argv[1]
url = f"https://api.github.com/repos/{repo}/releases/latest"
req = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json"})
with urllib.request.urlopen(req) as r:
    data = json.load(r)
print(data["tag_name"])
PY
  )"
fi

ASSET="qrir-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Installing qrir ${VERSION} (${TARGET}) from ${REPO}"
echo "Download: ${URL}"

curl -fsSL "$URL" -o "${TMP}/${ASSET}"

mkdir -p "${TMP}/extract"
tar -xzf "${TMP}/${ASSET}" -C "${TMP}/extract"

if [[ ! -f "${TMP}/extract/qrir" ]]; then
  echo "Archive did not contain expected ./qrir binary" >&2
  exit 1
fi

INSTALL_BIN="${PREFIX}/bin"
mkdir -p "${INSTALL_BIN}"
install -m 0755 "${TMP}/extract/qrir" "${INSTALL_BIN}/qrir"

echo "Installed: ${INSTALL_BIN}/qrir"
echo "Try: qrir --help"
