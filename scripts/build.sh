#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "${1:-}" in
  --help | -h)
    echo "Usage: bash scripts/build.sh [--host-only | --android-only]"
    exit 0
    ;;
  "" | --host-only | --android-only) ;;
  *)
    echo "Unknown argument: $1" >&2
    exit 2
    ;;
esac
if [ "$#" -gt 1 ]; then
  echo "Select at most one build target." >&2
  exit 2
fi
if [ "$(uname -s)" != Linux ]; then
  echo "Run this script on Linux or inside WSL." >&2
  exit 1
fi

if [ "${1:-}" != --android-only ]; then
  bash "$ROOT/scripts/build-linux.sh"
fi
if [ "${1:-}" != --host-only ]; then
  bash "$ROOT/scripts/build-android.sh"
fi
