# SideWire

[한국어](README.ko.md)

KernelSU Module URL: https://modules.kernelsu.org/en/modules/sidewire/

SideWire is a native bridge for controlling rooted Android devices from Windows, macOS, or Linux. It does not rely on ADB.

It provides a Rust daemon installed as a KernelSU module and a Rust CLI/server on the host.

Secure connections are the default, and normal traffic is authenticated and encrypted after pairing.

It supports most of the workflows you would normally use ADB for, but ADB is not used internally, so SideWire does not show up as an ADB session.

## What it does

- interactive shell / PTY and one-shot commands
- root or shell execution
- recursive file push and pull
- PC and Android text clipboard
- TCP forward and reverse tunnels
- inbound and outbound connection modes
- multiple devices with stable device IDs
- one-time PIN pairing with encrypted sessions afterward

## Requirements

- arm64 Android device
- KernelSU-compatible module environment
- Windows x86_64, macOS (Apple Silicon or Intel), or Linux x86_64 host
- both devices reachable over the same LAN/VPN

Release binaries and the KernelSU ZIP are published from the GitHub Releases page.

## Quick start

1. Install the SideWire KernelSU module and open its WebUI.
2. Choose `Secure`, set the connection mode/endpoint, and start SideWire.
3. Press **Generate pairing PIN**.
4. Pair the PC once:

```powershell
sidewire pair 192.168.0.123
```

The desktop server does **not** need to be running while pairing. Enter the six-digit PIN shown by the WebUI.

For the default outbound mode, start the host server afterward:

```powershell
sidewire server
```

Then use SideWire from another terminal:

```powershell
sidewire devices
sidewire shell
sidewire exec --as root id
```

Interactive shells use the Android shell's line editing, history, and Tab completion.
On Windows, this also keeps pasted input and shell prompts in the same output stream.
The `--raw` option remains available for compatibility and uses the same interactive mode.

## Clipboard and files

```powershell
sidewire clipboard push
sidewire clipboard pull
sidewire push .\build /data/local/tmp/build
sidewire pull /sdcard/Download/report.txt ./
sidewire pull /sdcard/MyFolder .\MyFolder
```

`clipboard push` copies the PC clipboard to Android; `pull` does the reverse. Recursive `push`/`pull` transfers use up to 4 concurrent files by default; use `--jobs 1..16` to tune concurrency. Protocol 1.2 peers also apply receive-window flow control to file chunks, while older peers keep the existing fallback behavior.

For a single-file `pull`, an existing local directory keeps the remote file name. Otherwise, the destination is used as the output file path. An existing output file is overwritten.

## Connection modes

**Outbound** is the usual setup: Android connects to `sidewire server` on the PC.

**Inbound** makes Android listen for the host instead:

```powershell
sidewire server --connect 192.168.0.123:58321
```

Inbound devices can also be discovered with `sidewire discover` or `sidewire server --discover`.

## Security

Secure mode uses SPAKE2 for the short-lived pairing PIN and Noise for authenticated encrypted sessions. The PIN is only used to establish a random long-term shared secret.

Plaintext mode exists for trusted development networks, but both sides must explicitly opt into it. Secure connections never silently downgrade.

See [SECURITY.md](SECURITY.md) for vulnerability reporting.

## Building

Build the Linux CLI and the KernelSU module on Linux or inside WSL:

```bash
bash scripts/build.sh
bash scripts/release.sh
```

Use `--host-only` for the Linux CLI or `--android-only` for the KernelSU module. The release script writes the selected artifacts to `dist/`.

For a Linux host build, install Rust and a C toolchain. Android builds also require Node.js 24 (for example via `fnm`), the pnpm version pinned in `webui/package.json`, JDK 21, Linux Android NDK r30, and `cargo-ndk`. Install the Android Rust target with `rustup target add aarch64-linux-android` and install cargo-ndk with `cargo install cargo-ndk --locked`. Set `ANDROID_NDK_HOME` if the NDK is outside the SDK's default location. The release script uses `zip`, `unzip`, and `sha256sum`.

Build the Windows CLI with `cargo build --release --locked -p sidewire` from a native Windows Rust/MSVC environment. On macOS, build the CLI with:

```bash
bash scripts/build-macos.sh
```

Run the normal Rust checks with:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The wire protocol is currently `1.2` with capability negotiation for optional features.

## License

GNU General Public License v3.0 (`GPL-3.0-only`). See [LICENSE](LICENSE).
