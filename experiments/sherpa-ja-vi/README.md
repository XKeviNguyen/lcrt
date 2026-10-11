# Reproduce the Japanese/Vietnamese native proof

This experiment uses sherpa-onnx 1.13.7 shared bindings and the exact patched
source build used by the application. Source, patch, and ONNX Runtime pins are
in [runtime.json](../../packaging/native/runtime.json). Model hashes and
redistribution sources are in [models.json](../../packaging/models.json).

From the repository root, run:

```sh
scripts/build-sherpa-native.sh
python3 scripts/fetch-models.py
export SHERPA_ONNX_LIB_DIR="$PWD/target/sherpa-native/install/lib"
export LD_LIBRARY_PATH="$SHERPA_ONNX_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
target/sherpa-native/native-context-test target/share/lcrt/models
cargo run --locked --release --manifest-path experiments/sherpa-ja-vi/Cargo.toml -- \
  check "$PWD/target/share/lcrt/models" - ja
cargo run --locked --release --manifest-path experiments/sherpa-ja-vi/Cargo.toml -- \
  check "$PWD/target/share/lcrt/models" - vi
```

The C++ test inspects actual decoder tokens, padding, warmup, populated decoder
cache, configured reset, sticky configuration errors, and unchanged default
blank/reset behavior. `check` then crosses the public C API through unchanged
Rust bindings. It rejects a stock native library without the acknowledgment.
No native configuration structure or exported function signature changes.
Rust drops the stream before the recognizer. The production pipeline gives both
objects one owner, with no concurrent access during decode or reset.

For natural speech, replace `check` with `paced`, replace `-` with a mono 16 kHz
signed 16-bit WAV of at most 120 seconds, and select `ja` or `vi`. `offline`
feeds the same samples without waiting for playback time. The trace reports
partial changes and final segments. Times start after model loading, which is
reported separately. One CPU inference thread is intentional.

The initial native implementation restored the token but retained an old
decoder cache because upstream result assignment preserves a destination cache
when its source is empty. The state test failed with `reset retained decoder
cache`. The correction explicitly clears that cache before assigning the fresh
language result. The corrected test passed and real two-utterance Vietnamese
replay produced both full utterances across two endpoints.

The [acceptance report](../../docs/STREAMING_JA_VI_ACCEPTANCE.md) explains the
natural-speech comparison. Recompute its short-clip metrics with:

```sh
python3 experiments/sherpa-ja-vi/summarize.py
```

To repeat the Whisper side, use the existing `lcrt-whisper-transcribe` benchmark
with `LCRT_BENCHMARK_TRACE=1`. Its normal JSON and per-caption stderr trace allow
the same predefined phrase check. This trace is opt-in for public test speech;
the application does not enable it or log caption content.
