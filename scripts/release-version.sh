#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

version="$(awk -F'"' '/^\[workspace\.package\]/{pkg=1; next} /^\[/{pkg=0} pkg && /^version = "/{print $2; exit}' Cargo.toml)"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.+-]+)?$ ]]; then
  echo "Could not read workspace version from Cargo.toml." >&2
  exit 1
fi
module_version="$(awk -F= '/^version=/{sub(/\r$/, "", $2); print $2; exit}' module/module.prop)"
webui_version="$(awk -F'"' '/^[[:space:]]*"version"[[:space:]]*:/{print $4; exit}' webui/package.json)"
if [ "$module_version" != "$version" ] || [ "$webui_version" != "$version" ] \
  || ! grep -Fxq "ui_print \"- SideWire $version\"" module/customize.sh; then
  echo "Release version mismatch: Cargo=$version, module=$module_version, WebUI=$webui_version. Check module/customize.sh too." >&2
  exit 1
fi
if [ "$#" -gt 0 ] && [ "$1" != "v$version" ]; then
  echo "Release tag mismatch: expected v$version, got $1." >&2
  exit 1
fi
printf '%s\n' "$version"
