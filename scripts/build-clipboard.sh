#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
destination="${1:-$ROOT/target-android/sidewire-clipboard.jar}"
version=9.5.33
expected_sha256=9743e95de295fb6f5490e21da26536e4d0921809511423122ff20328a572e7f5
tool_dir="$ROOT/target/tools"
r8="$tool_dir/r8-$version.jar"

for tool in javac java jar sha256sum; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "Clipboard helper build requires $tool on PATH." >&2
    exit 1
  fi
done

scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT
mkdir -p "$tool_dir" "$scratch/classes" "$scratch/dex" "$(dirname "$destination")"
if [ ! -f "$r8" ]; then
  echo "==> Downloading R8/D8 $version"
  url="https://dl.google.com/dl/android/maven2/com/android/tools/r8/$version/r8-$version.jar"
  curl --fail --silent --show-error --location "$url" --output "$scratch/r8.jar"
  printf '%s  %s\n' "$expected_sha256" "$scratch/r8.jar" | sha256sum --check --status
  mv "$scratch/r8.jar" "$r8"
fi
if ! printf '%s  %s\n' "$expected_sha256" "$r8" | sha256sum --check --status; then
  echo "R8/D8 checksum mismatch: $r8" >&2
  exit 1
fi

echo "==> Building Android clipboard helper"
javac -encoding UTF-8 --release 8 -d "$scratch/classes" "$ROOT"/android/clipboard/*.java
java -cp "$r8" com.android.tools.r8.D8 --release --min-api 23 --output "$scratch/dex" \
  "$scratch/classes/com/sidewire/"*.class
jar --create --file "$scratch/sidewire-clipboard.jar" -C "$scratch/dex" classes.dex
mv "$scratch/sidewire-clipboard.jar" "$destination"
echo "$destination"
