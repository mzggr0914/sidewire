#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

target=
skip_build=false
for arg in "$@"; do
  case "$arg" in
    --help | -h)
      echo "Usage: bash scripts/release.sh [--host-only | --android-only] [--skip-build]"
      exit 0
      ;;
    --host-only | --android-only)
      if [ -n "$target" ]; then
        echo "Select at most one release target." >&2
        exit 2
      fi
      target="$arg"
      ;;
    --skip-build) skip_build=true ;;
    *)
      echo "Unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done
if [ "$(uname -s)" != Linux ]; then
  echo "Run this script on Linux or inside WSL." >&2
  exit 1
fi

version="$(bash "$ROOT/scripts/release-version.sh")"
if ! "$skip_build"; then
  if [ -n "$target" ]; then
    bash "$ROOT/scripts/build.sh" "$target"
  else
    bash "$ROOT/scripts/build.sh"
  fi
fi
if [ "$target" != --android-only ]; then
  bash "$ROOT/scripts/release-linux.sh" --skip-build
fi
if [ "$target" = --host-only ]; then
  exit 0
fi

android_target="${CARGO_TARGET_DIR:-$ROOT/target-android}"
daemon="$android_target/aarch64-linux-android/release/sidewired"
clipboard="$android_target/sidewire-clipboard.jar"
for file in "$daemon" "$clipboard" webui/dist/index.html; do
  if [ ! -s "$file" ]; then
    echo "Missing build output: $file" >&2
    exit 1
  fi
done

scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT
stage="$scratch/module"
mkdir -p "$stage/bin" "$stage/webroot" "$ROOT/dist"
install -m 0644 module/module.prop module/sepolicy.rule "$stage/"
install -m 0755 module/action.sh module/customize.sh module/service.sh "$stage/"
install -m 0755 module/bin/sidewirectl "$daemon" "$stage/bin/"
install -m 0644 "$clipboard" "$stage/bin/"
cp -R webui/dist/. "$stage/webroot/"

for script in action.sh customize.sh service.sh bin/sidewirectl; do
  if LC_ALL=C grep -q $'\r' "$stage/$script"; then
    echo "Module script must use LF line endings: $script" >&2
    exit 1
  fi
  sh -n "$stage/$script"
done

name="SideWire-KernelSU-v$version-arm64.zip"
archive="$scratch/$name"
(cd "$stage" && zip -qr "$archive" .)
unzip -tq "$archive"
entries="$(unzip -Z1 "$archive")"
if grep -Fq '\' <<< "$entries"; then
  echo "Release ZIP contains Windows-style path separators." >&2
  exit 1
fi
for entry in module.prop sepolicy.rule action.sh customize.sh service.sh \
  bin/sidewired bin/sidewirectl bin/sidewire-clipboard.jar webroot/index.html; do
  if ! grep -Fxq "$entry" <<< "$entries"; then
    echo "Release ZIP is missing $entry at the module root." >&2
    exit 1
  fi
done
mv "$archive" "$ROOT/dist/$name"
sha256sum "$ROOT/dist/$name"
