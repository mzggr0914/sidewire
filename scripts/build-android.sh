#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [ "$(uname -s)" != Linux ]; then
  echo "Run the Android build on Linux or inside WSL." >&2
  exit 1
fi
if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
for tool in cargo node pnpm; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "Android build requires $tool on PATH." >&2
    exit 1
  fi
done
if ! cargo ndk --version > /dev/null 2>&1; then
  echo "Install cargo-ndk with: cargo install cargo-ndk --locked" >&2
  exit 1
fi

sdk="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android/Sdk}}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$sdk/ndk/30.0.16248370}"
if [ ! -d "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt" ]; then
  echo "Linux NDK r30 not found. Set ANDROID_NDK_HOME to its installation directory." >&2
  exit 1
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target-android}"

echo "==> Building KernelSU WebUI"
(
  cd webui
  pnpm install --frozen-lockfile
  pnpm build
)

bash "$ROOT/scripts/build-clipboard.sh" "$CARGO_TARGET_DIR/sidewire-clipboard.jar"

echo "==> Building Android arm64 daemon"
cargo ndk -t arm64-v8a -P 23 build --release --locked -p sidewired
echo "$CARGO_TARGET_DIR/aarch64-linux-android/release/sidewired"
