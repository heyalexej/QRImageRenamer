#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Build a release tarball for qrir on the current machine.

This script is intended to be run on:
- macOS x86_64 (produces x86_64-apple-darwin)
- Linux x86_64 (produces x86_64-unknown-linux-gnu)

Usage:
  build_dist.sh --version vX.Y.Z

Output:
  dist/qrir-vX.Y.Z-<target>.tar.gz
EOF
}

VERSION=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="${2:-}"
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

if [[ -z "${VERSION}" ]]; then
  echo "--version is required (example: --version v0.1.0)" >&2
  exit 2
fi

TARGET="$(rustc -vV | awk -F': ' '/^host:/{print $2}')"
if [[ -z "${TARGET}" ]]; then
  echo "Could not determine rust host target via: rustc -vV" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

echo "Building qrir (${VERSION}) for ${TARGET}"
cargo build --release --bin qrir

mkdir -p dist
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

cp -f "target/release/qrir" "${STAGE}/qrir"
chmod 0755 "${STAGE}/qrir"

OUT="dist/qrir-${VERSION}-${TARGET}.tar.gz"
tar -czf "${OUT}" -C "${STAGE}" qrir
echo "Wrote: ${OUT}"

