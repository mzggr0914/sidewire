# SideWire

[English](README.md)

SideWire는 루팅된 Android 기기를 Windows, macOS, Linux에서 제어하기 위한 네이티브 브리지입니다. ADB에 의존하지 않습니다.

KernelSU 모듈로 설치되는 Rust 데몬과, PC 쪽은 Rust로 작성된 CLI/server를 제공합니다.

기본 설정은 보안 연결이며, 페어링 이후의 일반 통신은 인증과 암호화가 적용됩니다.

SideWire 연결은 ADB 세션으로 표시되지 않습니다. Android의 기본 보안 정책이 적용되고 SELinux가 enforcing 모드일 때, 일반 앱(`untrusted_app`)은 SideWire 데몬(`sidewired`)이 실행 중인지 직접 확인할 수 없습니다.

## 주요 기능

- 대화형 shell / PTY와 단일 명령 실행
- root 또는 shell 권한으로 실행
- 디렉터리 포함 파일 push / pull
- Android 클립보드 텍스트 읽기·쓰기·지우기
- TCP forward / reverse tunnel
- inbound / outbound 연결 모드
- 여러 기기 동시 관리
- 일회용 PIN 페어링과 이후 자동 암호화 연결

## 요구 사항

- arm64 Android 기기
- KernelSU 모듈을 사용할 수 있는 환경
- Windows x86_64, macOS(Apple Silicon/Intel), Linux x86_64
- PC와 Android가 같은 LAN/VPN에서 서로 통신 가능해야 함

릴리스 바이너리와 KernelSU ZIP은 GitHub Releases에서 배포합니다.

## 빠른 시작

1. SideWire KernelSU 모듈을 설치하고 WebUI를 엽니다.
2. `Secure`를 선택하고 연결 모드/주소를 설정한 뒤 SideWire를 시작합니다.
3. **Generate pairing PIN**을 누릅니다.
4. PC에서 한 번만 페어링합니다.

```powershell
sidewire pair 192.168.0.123
```

페어링할 때 PC의 `sidewire server`가 켜져 있을 필요는 없습니다. WebUI에 표시된 6자리 PIN만 입력하면 됩니다.

기본 outbound 모드라면 페어링 후 PC에서 server를 실행합니다.

```powershell
sidewire server
```

다른 터미널에서 기기를 확인하고 바로 사용할 수 있습니다.

```powershell
sidewire devices
sidewire shell
sidewire exec --as root id
```

대화형 셸의 입력 편집, 명령 기록, Tab 자동완성은 Android 셸이 처리합니다.
Windows에서도 붙여 넣은 입력과 셸 프롬프트가 같은 출력 스트림으로 표시됩니다.
`--raw` 옵션은 호환성을 위해 유지되며 같은 대화형 모드로 동작합니다.

## Android 클립보드

```powershell
sidewire clipboard get
sidewire clipboard set "SideWire에서 보낸 텍스트"
sidewire clipboard clear
```

`clipboard get`은 Android 클립보드 텍스트를 그대로 표준 출력에 쓰며 줄바꿈을 추가하지 않습니다.

## 파일 전송

```powershell
sidewire push .\build /data/local/tmp/build
sidewire pull /sdcard/Download/report.txt ./
sidewire pull /sdcard/MyFolder .\MyFolder
```

재귀 `push`/`pull`은 기본적으로 파일을 최대 4개까지 병렬 전송하며 `--jobs 1..16`으로 동시성을 조절할 수 있습니다. 프로토콜 1.2 상대끼리는 파일 청크에 receive-window flow control도 적용하고, 이전 버전 상대와는 기존 방식으로 호환됩니다.

단일 파일 `pull`의 목적지가 기존 로컬 디렉터리이면 원격 파일명을 유지해 저장합니다. 그 외에는 목적지를 저장할 파일 경로로 사용하며, 같은 이름의 파일이 있으면 덮어씁니다.

## 연결 모드

**Outbound**는 Android가 PC의 `sidewire server`로 접속하는 기본 방식입니다.

**Inbound**에서는 Android가 연결을 기다리고 PC가 직접 접속합니다.

```powershell
sidewire server --connect 192.168.0.123:58321
```

Inbound 기기는 `sidewire discover` 또는 `sidewire server --discover`로 찾을 수도 있습니다.

## 보안

Secure 모드는 짧게 유지되는 페어링 PIN에 SPAKE2를 사용하고, 이후 세션은 Noise로 인증 및 암호화합니다. PIN 자체를 연결 비밀번호로 계속 사용하는 구조는 아닙니다.

신뢰할 수 있는 개발용 네트워크를 위한 plaintext 모드도 있지만 양쪽에서 직접 켜야 하며, secure 연결이 자동으로 plaintext로 내려가지는 않습니다.

보안 취약점 제보 방법은 [SECURITY.ko.md](SECURITY.ko.md)를 참고하세요.

## 빌드

Linux 또는 WSL에서 개발용 Linux CLI와 KernelSU 모듈 구성물을 빌드합니다.

```bash
bash scripts/build.sh
```

빌드와 패키징을 한 번에 진행해 배포 파일을 `dist/`에 만들려면 다음을 실행합니다.

```bash
bash scripts/release.sh
```

두 스크립트 모두 `--host-only`로 Linux CLI를, `--android-only`로 KernelSU 모듈을 선택할 수 있습니다. 이미 해당 대상을 빌드했다면 `release.sh`에 `--skip-build`를 붙여 기존 결과만 패키징합니다.

Linux CLI 빌드에는 Rust와 C 컴파일러가 필요합니다. Android 빌드에는 Node.js 24(예: `fnm`으로 설치), `webui/package.json`에 고정된 pnpm, Linux용 Android NDK r30, `cargo-ndk`도 필요합니다. `rustup target add aarch64-linux-android`와 `cargo install cargo-ndk --locked`를 실행하세요. NDK가 SDK 기본 경로에 없으면 `ANDROID_NDK_HOME`을 지정하세요.

클립보드 도우미 빌드에는 JDK와 `sha256sum`이 필요하며, 첫 빌드에서 D8을 내려받으려면 `curl`이 필요합니다. CI는 JDK 21을 사용하며 로컬 빌드에도 이 버전을 권장합니다. KernelSU ZIP 패키징에는 `zip`과 `unzip`도 필요합니다.

Windows CLI는 Windows의 Rust/MSVC 환경에서 `cargo build --release --locked -p sidewire`로 빌드합니다. macOS CLI는 macOS에서 다음 명령으로 빌드합니다.

```bash
bash scripts/build-macos.sh
```

기본 Rust 검사는 다음과 같습니다.

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

현재 wire protocol은 `1.2`이며 선택 기능은 capability negotiation으로 협상합니다.

## 라이선스

GNU General Public License v3.0 (`GPL-3.0-only`)을 사용합니다. 자세한 내용은 [LICENSE](LICENSE)를 참고하세요.
