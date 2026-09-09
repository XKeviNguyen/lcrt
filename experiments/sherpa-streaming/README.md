# Sherpa streaming spike (milestone #23)

This standalone Rust package measures one preselected English online transducer.
It is outside the production workspace and has an independent lockfile. It is
not an LCRT backend. No app, Whisper, or PipeWire code consumes it.

The official `sherpa-onnx = 1.13.7` crate uses its default static native libraries.
Its build script can download native code outside Cargo.lock's checksum coverage.
Use the verified archive preparation below before building. CI enforces the same
check in a fresh runner. Review provenance in
[the report](../../docs/SHERPA_STREAMING_SPIKE.md). Models and native binaries
must stay outside Git; the nested `target/` is ignored.

## Build and check

Run from this directory on Ubuntu AMD64, before any benchmark. Supply the
verified archive through the official build-script override. Use a fresh target
directory if a previous build populated an unverified native cache; the build
script prefers an already extracted cache over the archive override.

```sh
set -eu
export SHERPA_ONNX_ARCHIVE_DIR="$(mktemp -d /tmp/lcrt-sherpa-native.XXXXXX)"
archive="$SHERPA_ONNX_ARCHIVE_DIR/sherpa-onnx-v1.13.7-linux-x64-static-lib.tar.bz2"
curl --fail --location --silent --show-error \
  https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.7/sherpa-onnx-v1.13.7-linux-x64-static-lib.tar.bz2 \
  --output "$archive"
printf '%s  %s\n' \
  d1be7a69ac2b30120058d8302e624239a3064085383cfa47994a14fdc44c32d6 \
  "$archive" | sha256sum --check
cargo build --release --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
```

Repository CI runs these lint/test/doc checks in a separate `sherpa-spike` job.
It does not download the ASR model or run performance measurements.

## Prepare the fixed inputs

The report records the pinned official source URLs, every file's SHA-256, and
file sizes. Download the four selected files to a directory outside Git. Verify
all hashes before running. Do not substitute another model based on results.

Restore JFK if needed:

```sh
curl -fL https://raw.githubusercontent.com/ggml-org/whisper.cpp/v1.7.6/samples/jfk.wav \
  -o /tmp/lcrt-jfk.wav
printf '%s  %s\n' \
  59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e \
  /tmp/lcrt-jfk.wav | sha256sum --check
```

The tool checks the audio format and 11-second duration. The external checksum
check is mandatory for comparisons; format alone does not identify the fixture.

## Measure

Set `model_dir` to the directory containing the report's four model files. From
this directory, run one excluded warm-up and five sequential measured runs for
each Sherpa mode. Allow 10 seconds to settle before each series. Build the
Whisper control first too; no compilation or other benchmark may overlap.

```sh
sleep 10
./target/release/lcrt-sherpa-spike paced "$model_dir" /tmp/lcrt-jfk.wav \
  > /tmp/sherpa-paced-warmup.json
for run in 1 2 3 4 5; do
  /usr/bin/time \
    -f 'user_seconds=%U\nsystem_seconds=%S\ncpu_percent=%P\nmax_rss_kib=%M\nelapsed_seconds=%e\nexit_status=%x' \
    -o "/tmp/sherpa-paced-$run.time" \
    ./target/release/lcrt-sherpa-spike paced "$model_dir" /tmp/lcrt-jfk.wav \
    > "/tmp/sherpa-paced-$run.json" \
    2> "/tmp/sherpa-paced-$run.stderr" || exit
done
```

For throughput, repeat with `offline` and separate filenames. Offline still
uses 320-sample increments, but never sleeps. Its RTF excludes model startup,
WAV decoding, and JSON serialization; it includes the synthetic flush tail.
Do not interpret offline hypothesis timestamps as real-time caption latency.

Whisper control: use the unchanged root `target/release/lcrt-whisper-transcribe
benchmark paced models/ggml-tiny.en.bin /tmp/lcrt-jfk.wav en` command, one warm-up
and three measurements, with the same GNU time wrapper. See the baseline report.

For the bounded stability run, append `28` (308 seconds of paced input). This
reuses the fixture samples in memory and continuously feeds one online stream;
it does not reconstruct the recognizer between repetitions. Endpoint resets
use the official defaults. Sample `/proc/<pid>/status` every 30 seconds and keep
stderr and exit status. Use a 360-second external timeout. Repeats are limited
to 1–60; detailed hypothesis history is retained only for single-fixture runs.

The driver is synchronous and owns both recognizer and stream. It drains ready
decode steps after each 20 ms input chunk, observes each decode result, commits
nonempty endpoint segments before reset, and flushes once at EOF with the
upstream example's 0.3-second unpaced zero tail. `decode_calls` counts API calls,
not neural-network inferences; `decode_wall_ms` is time inside those calls.
`first_final_kind` distinguishes endpoint commits from explicit EOF completion.
There is no interactive cancellation protocol in this diagnostic; invalid
arguments and input errors return nonzero, and an external timeout bounds a
native hang. Production cancellation remains future integration work.
