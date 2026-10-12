# Offline corpus qualification

`qualify.rs` is a **dev-only macOS arm64 evidence harness**, not shipped in the
Android application. It exercises the real `rjdjcore` engine at 22050 Hz. It does
not establish Android playback, original-app fidelity, or general compatibility.

From `rust/rjdj-core`, with Logic Pro closed:

```sh
CARGO_TARGET_DIR=/tmp/rjdj-target-J cargo run --example qualify -- \
  --out /Users/tylermanring/.agents/artifacts/2026-10-11/rjdj-completion-loop/corpus
```

Requirements: the checkout's initialized libpd/Pd submodules, Rust/Cargo, the
macOS `cc`, `file`, `shasum`, `git` and `date` commands. No additional Cargo
packages are needed. On first use the harness clones rjlib to
`/tmp/rjlib-e74c696` and checks out
`e74c696afebe672c5bfed543cdd77e2d53865db6`. An existing clone must already have
that revision and no uncommitted changes; it is never reset or edited. At that
revision the four scene directories are in **`examplescenes/`**, not `examples/`.
The fifth scene is the checkout's `ScenePlayer/res/raw/atsuke.zip`.

The harness extracts the real `abstractions.zip`, adds the seven `.pd` extras
shipped by `extra_abs.zip`, and builds the five unmodified `ScenePlayer/jni`
externals as separate `.pd_darwin` bundles in `/tmp/rj-ext`. The pinned macOS Pd
loader tries that extension. It prepares private scene copies through
`playback::prepare` and uses `playback::search_paths`: bridges/extras first,
external directory second; each scene's canvas and declared `rj/` remain local.
It never supplies the HEAD library as a scene fallback.

## Evidence

- `summary.md`: all five scenes, step 1–5 booleans, independent instantiation
  and audio verdicts, audio features, console-error counts, command and revision.
- `<scene>.source.tar`: sorted regular-file USTAR with normalized metadata;
  its SHA-256 fingerprints relative names and file bytes, not permissions/mtime.
- `<scene>.static.json`: every object class, declaration, `#` receiver/sender,
  dollar-parameter class and WAV/PNG reference across all scene `.pd` files,
  including unused libraries/help files.
- `<scene>.wav`: exactly 12 seconds, stereo IEEE float32 at 22050 Hz. Input is
  mono 440 Hz at -12 dBFS peak. The first 10 seconds include a defined touch
  sweep and z-axis acceleration sequence; the last two follow micvolume 0.
- `<scene>.events.json` and `.console.txt`: exact typed atoms and whole queued
  console lines, with tick timestamps and requested/actual input-message times.
- `<scene>.report.md`: source-derived bounded-audio/message expectations,
  per-second RMS L/R, peak and spectral centroid, and independent no-touch,
  no-accelerate, no-mute and no-transport controls where reachable. A quiet
  steady sine is not guaranteed to trigger OnsetDetection's onset threshold;
  that limitation is explicit rather than replaced with invented audio.
- `abstractions.md`: every non-help HEAD `rj/*.pd` instantiated bare, plus each
  original scene-local set tested through its own playback search context.
  Root-canvas inlet/outlet classes/line numbers and SHA-256 comparisons are
  recorded. This is only the **instantiation** slice of COR-06, not each
  abstraction's full inlet-to-outlet/audio contract.
- `externals.md`: real host build and finite-output smoke contracts for all
  five externals, their exact output events and help-load results. The three
  known hard-read hazards are preserved: centroid/energy read 513 samples;
  zcr incorrectly reads `in[0..1023]`. Signal probes reblock to 1024 (the
  control accumulator uses 512). Original help files are instantiated with
  **DSP disabled**, because the zcr help includes an unsafe naked object.
- `pd-tilde.md`: actual `[pd~]` class creation and `pd~ start -nogui` with all
  executable candidates explicitly absent. No subprocess/fallback is faked.

Independent child processes isolate scene/control/global Pd state. Library
instantiations run with DSP disabled and no invented creation arguments, so
parameterized `$n` classes or missing table/file requirements may fail honestly.
All such errors are retained. A nonzero audio verdict alone is not a contract
pass. FFT/RMS, Pd token parsing, JSON escaping and output WAV framing have small
assert-based checks inside the harness; failed worker assertions preserve
available diagnostic artifacts and do not certify that case.

The output directory is a generated-artifact directory: a rerun replaces this
harness's same-named evidence files. Private scratch directories and playback
copies are released automatically. Original scene files, engine source and
existing tests are not changed. Device step 6 and complete per-abstraction
behavior contracts remain the orchestrator's separate work.
