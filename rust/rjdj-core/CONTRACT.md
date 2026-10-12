# rjdj-core shared ABI/build contract (PRE-04)

Orchestrator-owned. Workers implement against this; changes go through the orchestrator.
Baselines: spec `/Users/tylermanring/.agents/artifacts/2026-10-11/rjdj-rust-spec/spec.md` (commit 8eb9c125),
brief `/Users/tylermanring/.agents/artifacts/2026-10-11/rjdj-rust-core/reports/RjDj Rust core on Android.md` (a8fd547).
Worktree: `/Users/tylermanring/.agents/worktrees/rjdj-rust-core` (branch `codex/rjdj-rust-core`, registered in the
`RJDJ REA` repo; placed at a space-free path because ndk-build rejects the spaced checkout path).
Build env: `JAVA_HOME=/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home`, `ANDROID_HOME=~/Library/Android/sdk`,
NDK 29.0.14206865, cargo-ndk 4.1.2, rustc 1.96.1, target aarch64-linux-android installed.
Checkout base: origin/main 994e667 (= 8eb9c12 + Oboe 1.11.0 bump PR #1 + Gradle wrapper 9.8.1 PR #2). No other drift.

## Crate

- Path `rust/rjdj-core`, package `rjdj-core`, `[lib] name = "rjdjcore"`, `crate-type = ["cdylib", "rlib"]`
  (rlib so `cargo test` runs the pure modules and the static-libpd engine on the macOS host).
- Output `librjdjcore.so`, `aarch64-linux-android` only. Release profile: `panic = "abort"`, `lto = "fat"`,
  `codegen-units = 1`, `strip = false` (debuginfo kept for tombstones; Gradle strips).
  Panic policy (FAIL-04): release aborts on panic (no unwinding across JNI/AAudio FFI); `JNI_OnLoad` installs a
  panic hook that logs message+location to logcat (tag `RjdjCore`) before the abort; JNI bodies never
  unwrap/expect on fallible paths and return explicit Status / IOException instead.
- libpd sources referenced in place: `../../PdCore/pd-core/src/main/jni/libpd` (submodule 3fc60ea, Pd 68a30a3).
  `build.rs` compiles exactly the `PD_SRC_FILES` list from `libpd/Android.mk` plus Pd `extra/` with
  `-DLIBPD_EXTRA=1`, single precision, no `PDINSTANCE`, flags
  `-DPD -DUSEAPI_DUMMY -DPD_INTERNAL -DHAVE_UNISTD_H -DHAVE_ALLOCA_H -DHAVE_LIBDL -w`
  (+`-DHAVE_ENDIAN_H` on Android; on macOS host use `-D_DARWIN_C_SOURCE` instead and link nothing extra).
  Link `-ldl -latomic` on Android (NDK r29: `-latomic` is a no-op/absent; omit if the linker rejects it).
- Module layout (one owner each; see "Write-sets"):

```
src/lib.rs        module declarations only
src/error.rs      CoreError { message: String } + Result alias; messages are the exact Java strings below
src/pd/mod.rs     pub use; Pd engine facade
src/pd/ffi.rs     extern "C" declarations for z_libpd.h / z_queued.h (see "libpd surface")
src/pd/hooks.rs   static queued-hook trampolines, print concatenator, PdEvent + event sink
src/pd/engine.rs  Engine: init, search path, open/close patch, Command enum + SPSC drain, process_float
src/archive.rs    zip streaming import with limits, entry-name safety, exactly-one-.rj rule
src/plist.rs      Info.plist flat key/value reader
src/scenes.rs     discovery, publication, owned-path ops, failure cleanup, startup recovery, import lock
src/playback.rs   playback copy prepare/release/reclaim with bridge omission
src/recording.rs  recording naming and validation
src/resample.rs   22050 <-> stream-rate resampler, allocation-free in process
src/fifo.rs       burst->tick FIFO (stream-rate bursts in/out, 64-frame Pd ticks)
src/audio.rs      ndk::audio full-duplex engine (Android only, cfg(target_os = "android"))
src/jni.rs        JNI_OnLoad + RegisterNatives + extern "system" fns (Android only)
tests/*.rs        host tests
```

## Pinned crates (Cargo.toml is orchestrator-owned; ask before adding)

cc 1.8.0 (build), jni 0.22.4, ndk 0.9.0 (features audio, api-level-28), ndk-sys 0.6.0, libc 0.2.190,
rtrb 0.4.0, arrayvec 0.7.8, zip 9.0.1 (default-features = false, features deflate), quick-xml 0.42.0.
No oboe-rs, cpal, libpd-rs, UniFFI, rusqlite, hound, Gradle Rust plugins.

## Gradle wiring (AGP 9.1.1 / Gradle 9.8.1)

- `ScenePlayer/build.gradle` registers per-variant task `cargoNdk<Variant>` (custom `DefaultTask` using
  `ExecOperations`; `@OutputDirectory DirectoryProperty outputDir`) running
  `cargo ndk -t arm64-v8a -P 28 -o <outputDir> build [--release]` with `workingDir rust/rjdj-core`.
  Output layout `<outputDir>/arm64-v8a/librjdjcore.so`.
- Wired with `androidComponents.onVariants { variant.sources.jniLibs.addGeneratedSourceDirectory(task, { it.outputDir }) }`.
  No `android.newDsl=false` in the root build, no Mozilla/Mullvad plugin.
- `defaultConfig.ndk.abiFilters = ['arm64-v8a']` (Rust core is arm64 only; externals follow).
- `useLegacyPackaging = true` stays (externals are dlopen'ed from `nativeLibraryDir`).
- `System.loadLibrary("rjdjcore")` in `RjdjApplication.onCreate` (new `android:name` in the manifest).
- `rust/rjdj-core/.cargo/config.toml`: `[target.aarch64-linux-android] rustflags = ["-C", "link-arg=-Wl,-z,max-page-size=16384"]`.
  16 KB evidence: `llvm-objdump -p librjdjcore.so` LOAD align `2**14`; `zipalign -c -P 16 -v 4 app.apk`.

## JNI surface

Java class `org.puredata.android.scenes.RjdjCore` (final, static natives, registered by `JNI_OnLoad`
via `RegisterNatives`; no `Java_` symbol names needed). Exact signatures:

```java
final class RjdjCore {
  static native String hello();                                       // phase 0: "rjdjcore <version>"
  static native int nativeInit(String filesDir, String cacheDir, String nativeLibraryDir); // Status
  // scenes (throw java.io.IOException with the exact Java message on failure)
  static native String importScene(int detachedFd, String scenesRoot);       // published scene dir path
  static native String[] recoverScenes(String scenesRoot);                   // unregistered valid .rj dirs (abs paths)
  static native String[] readInfo(String sceneDir);                          // flat [k0,v0,k1,v1,...]
  static native void deleteOwnedTree(String root, String target);
  static native String preparePlayback(String sceneDir, String cacheDir);    // playback dir path
  static native void releasePlayback(String playbackDir);
  static native boolean validateRecording(String path);                      // true = nonempty kept; false = missing/empty (empty removed)
  // audio (Status codes; never throw)
  static native int startAudio(String playbackDir, float micVolume);
  static native void stopAudio();
  static native boolean isRunning();
  static native void sendTransportPlay(boolean on);
  static native void sendTouch(int kind, int pointerId1Based, float x, float y); // kind: 0 down, 1 xy, 2 up
  static native void sendAccelerate(float x, float y, float z);
  static native void sendMicVolume(float gain);
  static native void sendRecord(String wavPath, boolean start);  // start: "#transport scene <path>" then "#transport record 1"; stop: "#transport record 0"
  static native long[] audioInfo();   // [streamRate, pdRate(22050), framesPerBurst, outputXruns, inputXruns, outputLatencyMicros, state]
  static native void setListener(Listener l);   // one listener; replaced atomically
  interface Listener {
    void onList(String receiver, Object[] args);      // Float / String atoms, drain thread
    void onPrint(String line);                        // whole lines, drain thread
    void onAudioState(int state, int status);         // State / Status, drain thread
  }
}
```

Status (int): `0 OK, 1 ALREADY_RUNNING, -1 NOT_INITIALIZED, -2 PATCH_OPEN_FAILED, -3 OUTPUT_OPEN_FAILED,
-4 INPUT_OPEN_FAILED, -5 START_FAILED, -6 INVALID_ARGUMENT, -7 DISCONNECTED, -8 INTERNAL`.
State (int): `0 STOPPED, 1 STARTING, 2 RUNNING, 3 RESTARTING, 4 FAILED`.
Rust mirrors these in `src/error.rs` as `#[repr(i32)] enum Status` / `enum State`.

Receiver names and payloads are the spec's: `#transport` (`play 1|0`, `scene <path>`, `record 1|0`),
`#micvolume <f>`, `#touch down|xy|up <id> <x> <y>`, `#accelerate <x> <y> <z>` list,
Pd->host lists on `rj_image_android` / `rj_text_android`. Pd runs at 22050 Hz, 1 in / 2 out channels.

## Threading and realtime rules

- AAudio data callback: no allocation, locks, sleeping, I/O, logging or JNI. It drains the SPSC
  command queue (`rtrb` consumer), feeds the burst->tick FIFO, calls `libpd_process_float`.
- `Command` is a fixed-size enum (paths in `arrayvec::ArrayString<1024>`); producer = any Java thread
  through a `Mutex<rtrb::Producer>` (UI side may lock); consumer = audio callback only.
  Queue capacity 256; a full queue drops the command and bumps a counter (surfaced in `audioInfo`).
- Pd->host: `libpd_queued_init` + queued hooks; one permanently JVM-attached drain thread calls
  `libpd_queued_receive_pd_messages` every 10 ms while running and delivers `Listener` callbacks.
  Print hook concatenates until `\n`.
- libpd is single-instance and not thread safe: every `libpd_*` call outside the callback happens
  while the engine is stopped (init, open patch, close patch) and under `Engine`'s lock.
- Host->Pd messages before the stream starts (micvolume, play) are queued and applied by the first callback.

## Error strings (byte-exact with Java, used as IOException messages)

- `Archive must contain exactly one .rj scene`
- `A scene with this folder name already exists`  (existing destination folder; SceneArchive.java)
- Entry rejections, limits and owned-path refusals: copy the exact strings from
  `ScenePlayer/src/org/puredata/android/scenes/SceneArchive.java` and `ScenePlayback.java` at this checkout.
- Metadata: missing/empty `name` -> the exact Java message; absent author -> `Unknown artist`; absent description -> ``.

## Externals packaging (owner decision c)

The five `rj_*` externals stay separate `.so` files built by `ScenePlayer/jni` and dlopen'ed by Pd from
`nativeLibraryDir`. They link against the Pd API exported by `librjdjcore.so`: `.cargo/config.toml`
adds `-Wl,--export-dynamic-symbol=<sym>` for the Pd API they import (lld makes version-script-local
symbols global for these). Never claim this resolves their license (LIC-01).

## Write-sets (wave 1)

- A (toolchain): `ScenePlayer/build.gradle`, `ScenePlayer/AndroidManifest.xml`, new `ScenePlayer/src/.../RjdjApplication.java`,
  `ScenePlayer/src/.../RjdjCore.java`, `rust/rjdj-core/.cargo/config.toml`, `rust/rjdj-core/src/jni.rs`, `.github/workflows/*`.
- B (libpd): `rust/rjdj-core/build.rs`, `src/pd/*`, `tests/dsp.rs`, `tests/fixtures/pd/*`.
- C (archive): `src/archive.rs`, `src/plist.rs`, `src/scenes.rs`, `tests/archive.rs`, `tests/fixtures/archive/*`.
- D (playback/recording/dsp-plumbing): `src/playback.rs`, `src/recording.rs`, `src/resample.rs`, `src/fifo.rs`,
  `tests/playback.rs`, `tests/resample.rs`.
- Orchestrator: `Cargo.toml`, `src/lib.rs`, `src/error.rs`, this file.

## Wave 2 addendum (orchestrator, after wave 1 verified: 81 host tests green, phase 0 on device)

Build-flag correction: macOS host uses `-D_DARWIN_C_SOURCE -DHAVE_MACHINE_ENDIAN_H`; Android `-DHAVE_ENDIAN_H`.

### Audio engine API (`src/audio.rs`, Android only; worker F) — consumed by `src/jni.rs` (worker E)

```rust
pub enum AudioEvent { Pd(crate::pd::hooks::PdEvent), State { state: State, status: Status } }
pub fn set_event_callback(cb: Box<dyn Fn(AudioEvent) + Send + Sync + 'static>); // one callback, replaced atomically
pub fn start(playback_dir: &Path, mic_volume: f32, search_paths: &[PathBuf]) -> Status; // opens _main.pd, opens streams, starts
pub fn stop();                 // idempotent; closes patch; State::Stopped event
pub fn is_running() -> bool;   // State::Running
pub fn state() -> State;
pub fn send(cmd: crate::pd::engine::Command) -> bool; // queued; false = dropped (full) or invalid
pub fn info() -> [i64; 7];     // [streamRate, 22050, framesPerBurst, outputXruns, inputXruns, outputLatencyMicros, state]
```
The drain thread lives in `audio.rs`: it runs while state != Stopped, calls `pd::hooks::drain_queued()` every 10 ms and
forwards every `PdEvent` plus state transitions through the callback. The callback is invoked on the drain thread only.
`jni.rs` installs a callback that permanently attaches the drain thread to the JVM on first use and calls `Listener`.

### JNI additions/changes (worker E owns `src/jni.rs` and `RjdjCore.java` in wave 2)

- `static native void recoverScenes(String scenesRoot, String[] registered, Registrar registrar) throws IOException;`
  `interface Registrar { void register(String sceneDir) throws IOException; }` — Rust calls `scenes::recover_scenes_with`,
  invoking `registrar.register` per valid candidate; a Java exception becomes that candidate's error; first error is rethrown
  after the scan as IOException with the exact message, suppressed ones added via `Throwable.addSuppressed`.
- `static native String seedDefault(String scenesRoot, String atsukeZipPath, boolean alreadySeeded) throws IOException;`
  (returns scene dir or null) — Java copies `R.raw.atsuke` to a cache file first.
- `static native String importScene(int detachedFd, String scenesRoot) throws IOException;` — Rust takes ownership of the fd.
- `startAudio(String playbackDir, float micVolume)` passes `playback::search_paths(playback_dir, files_dir, native_library_dir)`
  from the dirs stored by `nativeInit`.
- All IOException-throwing natives map `CoreError.message` byte-exactly and `suppressed` via addSuppressed.

### Java shell ownership (wave 2)

- E: `SceneSelection.java`, `SceneTabs.java`, `RecordingSelection.java`, delete `SceneArchive.java`, `ScenePlayback.java`,
  `test/**` Java tests and `test/check-scenes.sh` (ported to Rust; `checkSceneFiles` task becomes `cargo test` in build.gradle — G edits build.gradle, E tells G the task shape).
- G: `ScenePlayer.java`, new `SceneAudioService.java` (replaces pd-core PdService), `build.gradle`, root `settings.gradle`/`build.gradle`,
  `ScenePlayer/jni/**` (externals build), `AndroidManifest.xml`, `.github/workflows/android.yml`, `README.md` build section.
- F: `src/audio.rs` only (+ `tests/` none; device evidence by orchestrator).
