# Per-abstraction real-engine contracts

Run from `rust/rjdj-core`:

```sh
CARGO_TARGET_DIR=/tmp/rjdj-target-q cargo run --example contracts -- \
  --rjlib /tmp/rjlib-e74c696 --contracts examples/contracts --out /tmp/rj-contract-results

CARGO_TARGET_DIR=/tmp/rjdj-target-q cargo run --example contracts -- \
  --rjlib /tmp/rjlib-e74c696 --contracts examples/contracts --out /tmp/rj-contract-smoke --only e_tremolo

CARGO_TARGET_DIR=/tmp/rjdj-target-q cargo test --example contracts
```

The runner loads every direct-child `*.json` except `_schema.json`. Filenames must match each contract's `name`; the target must exist in `rj/`. An unfiltered run also fails any non-help abstraction without a contract. `--only` is a focused execution, **not** complete library certification. Loading a help patch is never a contract test.

Each contract runs in its own child process, so a Pd crash is a reported failure rather than aborting the remaining library. A child has a wall-clock deadline of 30 seconds plus ten times the specified audio duration. Signal outlets are rendered in separate passes through `dac~ 1 2`; channel zero is measured. Each pass opens a fresh wrapper and repeats the same stimuli. Control outlets are captured and independently checked in every pass; their message counts are never inflated by combining passes.

The actual engine is `rjdjcore::pd::Engine` with its static libpd, one input, two outputs, 22050 Hz and 64-frame ticks. Search paths contain the wrapper directory, `rj/`, bridges from `ScenePlayer/res/raw/abstractions.zip`, and the seven extra abstractions `complex-mod~`, `hilbert~`, `rev1-final`, `rev1-stage`, `rev1~`, `rev2~`, `rev3~`. No cyclone or missing host sensors are supplied.

## Contracts and fixtures

`_schema.json` documents the JSON format. Ports are read from top-level `inlet`/`inlet~` and `outlet`/`outlet~` boxes, sorted by their horizontal position as Pd does, rather than physical source order. Every outlet must have a correctly typed expectation. The report lists unstimulated inlets; their intentional omission must be justified in `notes`. A contract must describe behavior derived from the help/source, not merely accept arbitrary output.

Creation arguments are strings or numbers in the order used in the Pd object box. Dynamic class boxes such as those in `u_stereo`, `e_mcompressor` and `e_scompressor` must have explicit arguments. An expected unavailable host object uses `"expected_result":"instantiates-with-errors"` and names that exact missing object in the notes; measurable assertions still run and must pass. `couldn't create`, `no such object` and failed Pd connections mark instantiation errors. Such errors fail by default. Expecting errors also fails if none occur; the flag does not waive signal/control assertions. Other diagnostics remain visible in the complete console.

Signal stimuli support `sine`, `noise`, `dc`, `silence`. Sine requires `freq`; amplitude defaults to 1. `dc` emits the specified amplitude, and `silence` emits zero. A signal starts at its `at_ms` (default zero); later signals on the same inlet replace the earlier signal rather than mix with it. Noise generators are seeded with 1. Control messages may also target signal inlets. Host input is otherwise silence.

Message encoding:

- `[0.5]`: float; `['bang']`: bang.
- `['symbol','hello']`: actual Pd symbol.
- `[1,2,3]`: numeric-led list; `['list','hello',3]`: explicit list.
- `['gain',0.5]`: anything message with selector `gain`.
- `receiver` replaces `inlet` for global messages, including `#touch`, `#transport`, `#accelerate`, and fixture receivers.

Use double quotes in actual JSON. A message to an absent receiver is a failure, not a silently dropped stimulus. Stimuli are sent before the tick containing `at_ms`, quantized downward to a tick boundary; equal-tick stimuli preserve JSON order. Hooks are bound before `open_patch`, so loadbang messages are included. Recorded event times are tick starts, not sample-accurate message timestamps.

`files` copies relative paths from the contract directory, rjlib root, or `rj/` (in that order) into the wrapper directory, retaining the relative path. Absolute paths and `..` traversal are rejected. These generated fixture tokens are available; messages use their basename without `generated:`:

| token | contents |
|---|---|
| `generated:sine440_1s.wav` | 22050 frames, 22050 Hz, PCM16 mono, 440 Hz sine, peak 0.5 |
| `generated:sine440_1s_stereo.wav` | the same sine in both stereo channels |
| `generated:seq-midi.txt` | `0 144 60 100;` followed by `100 128 60 0;`, newline separated |
| `generated:index.txt` | one line: `sine440_1s.wav 0` |
| `generated:index_stereo.txt` | one line: `sine440_1s_stereo.wav 0` |

No table is silently created just because a WAV is provided. Isolated table players and bus/delay utilities can declare the caller infrastructure explicitly:

```json
{
  "files": ["generated:sine440_1s.wav"],
  "tables": [
    {"name":"rjqa_sample", "file":"generated:sine440_1s.wav"},
    {"name":"rjqa_sample-1", "file":"generated:sine440_1s.wav"},
    {"name":"rjqa_sample-2", "file":"generated:sine440_1s.wav"}
  ],
  "fixtures": [
    {"object":"delwrite~ rjqa_delay 1000", "signal":{"kind":"sine","freq":440,"amp":0.5}},
    {"object":"s_osc MYOSCILLATOR"}
  ],
  "feedback": [{"outlet":1,"inlet":1,"object":"* 10"}]
}
```

Include only the infrastructure the abstraction's documented caller needs. `tables` makes named `array define` boxes and loadbang `soundfiler read -resize` messages; one entry loads the first WAV channel into its named array. For stereo tables, declare both `-1` and `-2` entries, or let the abstraction's own stereo loader fill them. Table files are prepared even if not also listed in `files`.

A `fixtures` object has its first inlet fed by its optional signal, otherwise by `r rjqa_fixture<index>` for scheduled receiver messages. These are actual Pd objects, not host emulations. `feedback` wires an abstraction outlet through a real Pd operation and synchronously back into the specified inlet; this permits documented `u_listmap`/filter/reduce callbacks while still capturing the outlet. Object declarations cannot contain semicolons, commas or newlines.

## Measurement semantics

- RMS is `sqrt(mean(sample²))` on the specified half-open `window_ms` (default the entire duration). Time-to-frame conversion is floor. The last rendered tick is trimmed to the specified duration. Empty/non-finite windows fail.
- `silent:true` requires RMS strictly below `1e-4` on that window.
- `centroid_hz` is a **spectral magnitude centroid**, not zero-crossing pitch: 1024-point Hann-windowed FFT, 512-frame hop, bins 0 through Nyquist, all magnitudes accumulated before division. The final partial windows are zero-padded. Silence has centroid zero. This is the same convention as `qualify.rs`; it is not a fundamental-frequency detector.
- `changes_after_ms` compares RMS of the entire `[0,split)` and `[split,end)` render, independent of `window_ms`, and requires `abs(after-before) > 0.2*before`. Silence-to-nonzero counts as a change; silence-to-silence does not.
- Signal/range/centroid bounds are inclusive. Control `range` checks every numeric atom of every captured message, and a message with no numeric atoms fails a range assertion.
- Positive `min_messages` is a lower bound. `min_messages:0` **alone** (without type/range/exact) declares an explicitly quiet outlet and requires zero messages, including loadbang. With type/range/exact present, zero is a lower bound and those assertions apply to observed messages.
- A type-preserving Pd route precedes the outlet label. Float, bang, symbol and list are checked as those Pd types; arbitrary-selector messages normalize to `list`, with the selector retained as the first atom. Bang exact values are `["bang"]`; symbols are `["value"]`.
- `exact` is the prefix of emitted messages, each requiring the exact atom-vector length and exact symbol strings. Numeric values have a fixed rounding tolerance `abs(actual-expected) <= 1e-6 * max(1,abs(expected))`; it is not widened after failures. Later messages are permitted unless another assertion excludes them.

## Reports and explicit unresolved constructs

`<out>/<name>.md` includes contract/stimuli, generated host patch, all queued/native console lines, measured expectations and failures. `contracts.md` is the summary table; `contracts.json` contains machine-readable reports and counts. Exit status is nonzero for assertion, instantiation, missing-contract, malformed-contract, missing-file, worker crash or timeout failures.

`unresolved.md` separately scans HEAD `rj/`, the four example-scene copies, and the extracted Atsuke library and scene patches, including help files. It gives physical `file:line` for `seq`, `clone` targets, dynamic `$1`/`$2` class boxes, message-created objects, runtime library/subprocess declarations, and `.pd`/`.wav`/`.aif` references. `seq` is cyclone-only and remains unresolved in the conversion help patches; it is not a dependency of `c_seqplay` playback. Dynamic class substitutions use the explicit contract arguments, including numeric Pd float-box shorthand. Audio references distinguish present, absent, and unresolved runtime filenames in each tree. Help placeholders do not become scene fixtures. Static resolution is evidence about paths/classes, not proof of runtime behavior; only the contract results certify the measured abstraction.
