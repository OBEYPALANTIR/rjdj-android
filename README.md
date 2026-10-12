# RjDj Revival for Android

An independent revival of reactive music scenes for Android 17 (API 37), built from the public ScenePlayer and libpd sources. This is a reconstruction of the scene host, not the original proprietary RjDj application or its discontinued online catalog.

## Use

Open a scene, put on headphones and tap **Play**. Grant microphone permission, then tap Play again. If Android stops showing the permission prompt after repeated denials, use **Open settings** to allow the microphone, return and tap Play. Touch the artwork or move the phone to interact with patches that support those inputs. Microphone level controls the live input.

**Record** saves stereo scene output in the app's private storage. Listen and rename it in the **Recordings** tab. Playback continues in a microphone/media foreground service when the screen turns off. Pause, Back, audio focus loss or headphone disconnection stops capture. Notification permission enables the return-to-scene notification.

The players keep their audio and recording state during rotation, window resizing, theme, language, display density, font size/weight and keyboard/navigation changes. Controls and overlays reload with the updated resources without reopening the audio engine. Process death and unhandled system configuration changes can still end playback.

Startup recovers scenes published before an interrupted database registration and removes abandoned import staging. A damaged scene cannot block importing other scenes. Playback reclaims abandoned private cache copies while preserving copies still in use. A recording appears in the library only when the scene produced a nonempty file; rejected empty files are removed.

**Import a scene** opens Android's document picker. Choose a ZIP-format `.rj`, `.rjz` or `.zip` containing exactly one `.rj` folder with `_main.pd` and `Info.plist`. Scene files remain intact; playback uses a temporary copy with Android host bridges. Imports are limited to 4,096 entries, 64 MiB per file and 256 MiB total. Duplicate folder names are rejected without overwriting scenes.

There is no network permission, account, scene marketplace or location capture. Recordings and imported scenes are local app data; Android backup may include them.

## Build and check

Requirements: JDK 21, Android SDK platform 37.0, Build Tools 37.0.0, NDK 29.0.14206865, rustup (the repo's `rust-toolchain.toml` pins Rust 1.96.1 with the `aarch64-linux-android` target; `generateNotices` rejects any other rustc), and cargo-ndk 4.1.2. Gradle 9.8.1 is supplied by the wrapper; AGP 9.1.1 supports API 37. Minimum install API is 28; the APK is arm64-only and primary acceptance is Android 17.

```sh
git submodule update --init --recursive
sdkmanager 'platforms;android-37.0' 'build-tools;37.0.0' 'ndk;29.0.14206865'
cargo install cargo-ndk --version 4.1.2 --locked
./gradlew :ScenePlayer:checkSceneFiles :ScenePlayer:assembleDebug :ScenePlayer:assembleDebugAndroidTest :ScenePlayer:lintDebug --max-workers=2
adb -s YOUR_SERIAL install -r ScenePlayer/build/outputs/apk/debug/ScenePlayer-debug.apk
```

Set `JAVA_HOME` and `ANDROID_HOME` to your installed JDK and SDK, with `cargo` on `PATH`. Gradle builds `librjdjcore.so` and the five separately loaded `rj_*` externals directly with the pinned NDK, and bundles the seven standard Pd abstractions previously supplied by pd-core. The retained `PdCore` submodule supplies source only. The package is `org.rjdj.revival`. The debug APK uses a development signing key and is not a store release.

`checkSceneFiles` runs `cargo test --manifest-path rust/rjdj-core/Cargo.toml`: archive and metadata validation, recovery, playback-copy preservation, recording validation, DSP and resampling checks. CI also checks every native library's 16 KB ELF alignment and runs `zipalign -c -P 16 -v 4` on the APK.

Grant microphone permission in the app and pause playback before the device check. Instrumentation opens the app, checks native import/SQLite recovery and the service's seven unpacked Pd abstractions, then records a tone and a muted microphone scene through the real AAudio engine:

```sh
adb -s YOUR_SERIAL install -r ScenePlayer/build/outputs/apk/androidTest/debug/ScenePlayer-debug-androidTest.apk
adb -s YOUR_SERIAL shell am instrument -w org.rjdj.revival.test/org.puredata.android.scenes.SceneEngineInstrumentation
```

A successful instrumentation run prints `PASS` and exits with `INSTRUMENTATION_CODE: -1`. It requires a nonempty stereo WAV at 22050 Hz, tone energy and channel parity, and near-zero recorded energy after microphone mute. Foreground-service, notification, touch/overlays, permission recovery and interruption flows need separate on-device verification. Physical audio latency, Bluetooth routing and sensor calibration require a phone and headphones.

## Provenance and compatibility

- Host: [libpd/pd-for-android ScenePlayer](https://github.com/libpd/pd-for-android/tree/d3ec6c97fcbe881f4fa0cc787dd938e45e31262e/ScenePlayer), copyright Peter Brinkmann and contributors. See [ScenePlayer/LICENSE.txt](ScenePlayer/LICENSE.txt).
- Native engine: pinned libpd, Pure Data and opensl_stream submodules. Their own license files remain in place.
- The bundled Atsuke scene, its artwork and rj abstractions retain the licenses inside `ScenePlayer/res/raw/atsuke.zip`, including GPLv3 notices. The host's BSD license does not replace scene licenses.
- [rjdj/rjlib](https://github.com/rjdj/rjlib/tree/e74c696afebe672c5bfed543cdd77e2d53865db6) documents the historical scene format. Its default GPL license has per-file exceptions.
- Open **Notices** in the Scenes tab for bundled component licenses, crate versions, unresolved external terms and the independent-revival trademark statement.

The host implements microphone/stereo audio, transport and recording messages, 320-unit touch coordinates, acceleration messages, and image/text overlays. Compatibility depends on the objects used by each patch; missing third-party externals are not emulated. The original app binary was unavailable for direct behavioral comparison.

