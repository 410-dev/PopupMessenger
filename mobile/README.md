# PopupMessenger 모바일 (Android · iOS)

Tauri 2로 만든 Android/iOS 네이티브 앱입니다. 데스크톱 앱과 같은 릴레이 프로토콜·종단간 암호화 코드(`../src-tauri/src/relay_io.rs`, `e2ee.rs`)를 그대로 공유합니다.

## 동작 방식

| 항목 | 내용 |
|---|---|
| 네트워크 | **릴레이 서버만** 씁니다. LAN 탐색·직접 전송은 없습니다 |
| 첫 실행 | 릴레이 서버에 연결(주소 → 서버 정보·인증서 지문 확인 → 로그인 / 가입 신청 / 기기 등록)하기 전에는 앱을 쓸 수 없습니다. 가입 승인 대기 중이거나 서버가 로그온을 거부하면(계정·기기 정지 등) 그 안내 화면에서 멈춥니다. "서버에서 나가기"를 하면 다시 이 화면으로 돌아갑니다 |
| 백그라운드 | 배터리와 데이터를 아끼려고 **화면에 있을 때만** 연결합니다. 앱이 화면에서 사라지면 Android는 8초, iOS는 2초 뒤에 로그오프합니다(파일 선택 창 등 잠깐 다녀오는 경우는 유지, 전송 중이면 끝날 때까지 유지). 그동안 온 쪽지는 릴레이가 보관하고(설정 > "앱이 꺼져 있을 때 받기" = 서버에 보관), 앱을 다시 열면 바로 받아 옵니다. 다른 사람 목록에는 초록 도넛(보관 후 전달)으로 보입니다 |
| 받는 사람 | 사람(계정) 단위로 보내면 그 사람의 모든 기기에 전달됩니다. 연락처에서 펼쳐서 특정 기기 하나만 고를 수도 있고, 내 다른 기기에도 보낼 수 있습니다(서버 `device_target` 기능). `contacts: lookup` 서버는 `@사용자 이름`으로 찾습니다 |
| 받은 쪽지 | 화면에 있을 때 오면 팝업 + 진동(설정에서 끄거나 방해금지면 쪽지함에만). 답장은 보낸 사람 계정으로, "전체 답장"은 공개된 그룹원 전체에게 |
| 첨부 | 보내기: 시스템 파일 선택기(사진·카메라 포함). 받기: Android는 `Android/data/com.popupmessenger.mobile/files/Documents/PopupMessenger`, iOS는 앱 Documents(파일 앱에 보임). 쪽지마다 [열기](다른 앱 / Quick Look) [공유](공유 시트) [저장](Android 10+: `Download/PopupMessenger`, iOS: "파일에 저장") |
| 암호화 | 서버 정책이 종단간 암호화면 데스크톱과 같은 X25519 + XChaCha20-Poly1305. 상대 키가 바뀌면 설정에서 확인할 때까지 보내지 않음 |

푸시 알림(FCM/APNs)은 없습니다. 앱이 꺼져 있는 동안 온 쪽지는 앱을 열어야 보입니다. 푸시를 붙이려면 릴레이 서버에 푸시 토큰 등록과 발송 기능이 필요합니다.

## 구조

```
mobile/
  ui/                  웹 UI (한 페이지: 설정 화면 → 탭 + 시트)
  src-tauri/
    src/lib.rs         명령, 앱 시작
    src/relay.rs       릴레이 클라이언트 (화면에 있을 때만 연결)
    src/store.rs       설정·쪽지 저장
    gen/android/       Android 프로젝트 (tauri android init으로 생성, 커밋함)
    gen/apple/         Xcode 프로젝트 (tauri ios init으로 생성, 커밋함)
    Info.ios.plist     iOS Info.plist에 합쳐지는 항목 (파일 앱 공유, 카메라·사진 권한 문구)
  plugin/              네이티브 플러그인 tauri-plugin-pm-native
    android/           Kotlin: FileProvider로 열기·공유, MediaStore로 저장, 시스템 바·키보드 크기 전달
    ios/               Swift: Quick Look, 공유 시트, 문서 선택기, 햅틱, 로그오프할 동안의 백그라운드 실행 시간
```

## 빌드

공통: Rust (stable), Node.js. 처음 한 번 `npm install`.

### Android

필요: Android SDK, NDK, JDK 17, Rust 타깃.

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
```

`ANDROID_HOME`(예: `%LOCALAPPDATA%\Android\Sdk`)과 `NDK_HOME`(`$ANDROID_HOME/ndk/<버전>`)을 설정합니다.

```bash
npm run android:dev
```

```bash
npm run android:build
```

결과물: `src-tauri/gen/android/app/build/outputs/` (APK/AAB). 배포용 서명은 [Tauri 문서의 Android 서명](https://v2.tauri.app/distribute/sign/android/)대로 `gen/android/keystore.properties`를 만듭니다(커밋하지 마세요, `.gitignore`에 있음).

Windows에서 Gradle이 `Unable to establish loopback connection`으로 실패하면(사용자 폴더 경로에 공백이 있을 때 JDK의 소켓 임시 파일 문제) 짧은 임시 폴더를 지정합니다.

```bash
set JAVA_TOOL_OPTIONS=-Djdk.net.unixdomain.tmpdir=C:\Users\Public\jtmp
```

에뮬레이터에서 PC의 릴레이 서버는 `10.0.2.2:<포트>`입니다.

### iOS (macOS에서)

필요: Xcode, Rust 타깃 `aarch64-apple-ios aarch64-apple-ios-sim`. Xcode 프로젝트(`src-tauri/gen/apple`)는 커밋되어 있어 `ios:init`은 다시 하지 않아도 됩니다.

```bash
npm run ios:dev
```

시뮬레이터용 빌드 (서명 필요 없음, 결과물: `src-tauri/gen/apple/build/arm64-sim/PopupMessenger.app`):

```bash
npx tauri ios build --debug --target aarch64-sim
```

기기용 빌드:

```bash
npm run ios:build
```

기기용은 서명이 필요합니다. Xcode > Settings > Accounts에 Apple ID를 추가하고, 프로젝트에 들어 있는 팀(`DEVELOPMENT_TEAM`)이 내 팀이 아니면 `APPLE_DEVELOPMENT_TEAM=<팀 ID>`를 붙여 빌드하거나 Xcode의 Signing & Capabilities에서 바꾸세요.

### 아이콘

원본은 `src-tauri/icons/icon.svg`(108×108, 배경을 꽉 채우고 그림은 가운데 66 안쪽)입니다. `npm run icons`로 각 플랫폼 아이콘을 다시 만듭니다. 다만 Android 적응형 아이콘은 직접 만든 벡터(`gen/android/app/src/main/res/drawable/ic_pm_background.xml`, `ic_pm_foreground.xml`, `ic_pm_monochrome.xml`)를 쓰는데, `tauri icon`이 `mipmap-anydpi-v26/ic_launcher*.xml`을 덮어쓰므로 실행한 뒤 이 두 파일을 git에서 되돌리세요.

### 데스크톱에서 UI 확인

`npm run dev`로 같은 앱을 PC 창으로 띄울 수 있습니다. `POPUPMESSENGER_PROFILE=<이름>`이면 별도 데이터로 한 PC에서 여러 개를 띄우고, Windows에서 `POPUPMESSENGER_CDP_PORT=<포트>`를 함께 주면 WebView2 원격 디버깅 포트를 엽니다.
