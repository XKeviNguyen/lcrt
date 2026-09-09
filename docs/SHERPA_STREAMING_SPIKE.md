# Sherpa streaming ASR spike — milestone #23

This is an English feasibility measurement, not a production backend integration.
The candidate and settings were selected before any performance runs. Only one
model and one configuration were measured; no fastest-model selection occurred.

## Identity and provenance

- Starting `develop`: `ffb7c705dfc47372a48405eb00163bf8e5a1d207` (PR #22).
  Clean tree, one worktree; historical local branches existed but no unrelated
  active changes. Post-merge CI run `33976119355` succeeded.
- Measured driver `src/main.rs` SHA-256:
  `ad9edd0ae8fdab61d2034ce135b2eae82b47bbae48f3d461e402c46647cf3c15`.
  Measured release executable SHA-256:
  `17b0a5859aff55264c4febfa868846ca6e8f50c30367a01b17bc753cf9b2397d`.
- Sherpa-ONNX Rust and native version: **1.13.7**, release tag `v1.13.7`,
  Git revision `917bed95c8e5c7c18aa4d69fea42e9ef8ef0a60e`.
- Candidate: **sherpa-onnx-streaming-zipformer-en-2023-06-21**. Its int8
  encoder/joiner and float decoder are the combination selected by the
  [pinned official English Rust launch script](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.7/rust-api-examples/run-streaming-zipformer-en.sh).
  This is a CPU-capable online English transducer, with a practical but larger
  footprint than tiny.en. The choice followed the maintained official example,
  rather than a search over measured performance.
- Model files came directly from the [official model author's repository](https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-21/tree/9a65b6ea94c311ca770c2bf895b30f456a22d703),
  pinned revision `9a65b6ea94c311ca770c2bf895b30f456a22d703`. The redundant
  official GitHub archive download was stopped before testing because it also
  included unused float variants and was slow. No unofficial repack was used.
- Model directory during testing:
  `/tmp/lcrt-sherpa/sherpa-onnx-streaming-zipformer-en-2023-06-21`.
  All three ONNX hashes matched the pinned upstream LFS object hashes.

| Selected file | Bytes | Local SHA-256 |
| --- | ---: | --- |
| encoder-epoch-99-avg-1.int8.onnx | 187,823,992 | `32c98281c7bd8b63e3e142d007251b37f120572e8fdea9a4f5a79ce22b10ec4f` |
| decoder-epoch-99-avg-1.onnx | 2,092,566 | `9da02b77cb08826756ec6a88635f35a40374e4164e7c6359121a9145958a6ceb` |
| joiner-epoch-99-avg-1.int8.onnx | 259,335 | `831477d390e59a61f1b6a6f763b9903e6c6366ff6034f1ddba613be82637122f` |
| tokens.txt | 5,048 | `49e3c2646595fd907228b3c6787069658f67b17377c60aeb8619c4551b2316fb` |

Total selected model size: **190,180,941 bytes (181.37 MiB)**. This is file size,
not process RSS or filesystem cache memory.

The native archive was downloaded by the official crate's build script from
[the versioned GitHub release](https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.7/sherpa-onnx-v1.13.7-linux-x64-static-lib.tar.bz2).
Its local SHA-256 is
`d1be7a69ac2b30120058d8302e624239a3064085383cfa47994a14fdc44c32d6`.
It statically links Sherpa's C API, C++ core, ONNX Runtime, and supporting native
libraries. `ldd` on the measured executable lists libstdc++, libm, libgcc_s,
libc, and the Linux loader; no separate ONNX Runtime shared library is required.
The model and native archive are outside tracked files. The production lockfile
is unchanged; the experiment's independent lockfile pins its Rust graph.
Native archive contents are not secured by Cargo.lock: the recorded archive
hash is an additional reproducibility check. A future integration needs native
artifact verification, license/dependency review, and target-specific builds.

## Machine, configuration, and metric contract

Measurements ran on 2026-09-09 on the same Ubuntu AMD64 workstation as #22:
Intel Core i5-12500H, 16 logical CPUs, 39,706,612 KiB installed RAM, Ubuntu 26.04,
Linux `7.0.0-31-generic` (baseline used `-30`). Rust 1.98.0,
`88d9e12ae178fab0fb5cc050a94da85685d449ea`, release builds. No governor, turbo,
cache-dropping, system-wide configuration changes, or sudo were used.

Sherpa uses CPU provider, `num_threads = 4`, greedy search, 16 kHz / 80-feature
input, no hotwords, and explicit official endpoint defaults: 2.4 seconds trailing
silence without requiring speech, 1.2 seconds after speech, or 20 seconds of
utterance length. Thread count is per native session, not a four-thread cap on
the whole process. No settings were tuned after seeing results.

The absent JFK file was restored from [whisper.cpp v1.7.6](https://github.com/ggml-org/whisper.cpp/blob/v1.7.6/samples/jfk.wav).
Its SHA-256 was verified before measurement:
`59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`.
`ffprobe` confirmed 11.000 seconds, mono, 16 kHz, signed 16-bit PCM. Whisper uses
the unchanged tiny.en file/configuration in [the authoritative baseline](V1_ASR_BASELINE.md),
77,704,715 bytes, SHA-256
`921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f`,
English and four inference threads.

- Startup: immediately before recognizer construction until recognizer and
  stream are ready; WAV decoding excluded. Model startup is excluded from replay
  latency, as in #22. Cold application-start experience must consider both.
- First partial: replay start to first observable nonempty online hypothesis.
  Each 320-sample chunk is accepted only after its cumulative 20 ms duration
  elapses; ready decode steps are drained immediately and hypotheses observed.
  Whisper event polling has up to one 20 ms tick of observation granularity;
  Sherpa is observed directly after decode. This small difference is retained.
- First final: first nonempty endpoint segment committed before reset, or the
  final result after EOF draining if there was no endpoint. These are not the
  same endpoint semantics as Whisper. Sherpa's fixture endpoint occurs mid-quote;
  it is not completion of the entire quote.
- Completion: all fixture audio accepted, then EOF recognition drained. Following
  the official example, Sherpa accepts 0.3 seconds of synthetic zeros without
  pacing before `input_finished()`. This tail is explicit finalization context,
  not a substitution or extension of the benchmark WAV; its compute is included
  in completion/throughput. First text occurs well before this flush.
- Activity: `decode_calls` counts calls to `OnlineRecognizer::decode` and
  `decode_wall_ms` sums wall time inside those calls. Neither is a neural
  inference counter nor process CPU time. Whisper's inference count is separate.
- CPU and peak RSS: the same GNU `/usr/bin/time` whole-process average CPU and
  maximum resident set used by #22, including startup and pacing idle time.
- Transcript: concatenate final endpoint segments plus the EOF remainder so
  resetting a Sherpa segment cannot silently drop earlier words.
- Accuracy: lowercase, remove punctuation, collapse whitespace; exact-match
  against the JFK reference only. No broad WER/accuracy claim follows.

All builds/checks finished before measurements. Each series had a 10-second
settle, one excluded warm-up, then sequential measurements: five Sherpa paced,
three Whisper paced controls, five separate Sherpa unpaced throughput runs.
Whisper and Sherpa did not run concurrently. Min/median/max are reported, not p95.
Raw outputs, warm-ups, timing records, and stability samples are retained in
[SHERPA_STREAMING_SPIKE_RESULTS.json](SHERPA_STREAMING_SPIKE_RESULTS.json).

## Measured comparison

The current Whisper control first-partial median is **2,560.128 ms**, versus
**2,340.071 ms** in #22: **9.40% slower**. Startup also rose from 60.637 ms to
120.756 ms and CPU from 137% to 185%. The kernel changed; no specific cause for
all drift was established. These differences are material enough to retain the
new same-session control rather than silently reuse the old baseline. Series
were sequential, not interleaved, so thermal/order effects are not eliminated.

| Metric | Whisper control | Sherpa candidate |
| --- | ---: | ---: |
| Model | tiny.en | streaming Zipformer en 2023-06-21, int8 encoder/joiner |
| Model bytes | 77,704,715 | 190,180,941 |
| Startup median ms (min–max) | 120.756 (117.863–120.932) | 1,536.277 (1,495.659–1,574.584) |
| First partial min ms | 2,560.076 | 1,079.532 |
| First partial median ms | 2,560.128 | 1,083.580 |
| First partial max ms | 2,560.328 | 1,088.066 |
| First final median ms (different semantics) | 11,871.732 (EOF) | 5,884.212 (mid-quote endpoint) |
| Completion median ms (min–max) | 11,871.734 (11,842.483–11,886.328) | 11,029.572 (11,027.336–11,033.037) |
| GNU time CPU median (min–max) | 185% (184–186%) | 729% (728–731%) |
| Peak RSS median KiB (min–max) | 221,072 (220,784–221,244) | 269,456 (265,988–270,816) |
| Normalized JFK exact match | 3/3 yes | 5/5 yes |
| Offline throughput RTF median | 0.319215, historical #22 only | 0.064663, current 5 runs |
| Short stability | historical 30-minute #22 soak; not repeated | See bounded check below |

**First-partial speedup = 2,560.128 / 1,083.579599 = 2.36×** (57.7% lower
warm-model first-nonempty latency). The comparison uses the same replay clock,
fixture, and input pacing, with the small observation-granularity difference
noted above. It does not compare equally informative caption lengths. Sherpa's
first text is just **“AND”**; **“AND SO MY FELLOW”** appears at a 2,044.400 ms
median. Thus this is a material improvement in initial feedback, not evidence
of sub-250 ms useful sentences or an order-of-magnitude gain.

Median startup-plus-first-partial across each run is **2,624.343 ms Sherpa**
versus **2,680.884 ms Whisper**. This sum is not an independently measured GUI
startup metric, but shows that loading the Sherpa model largely consumes the
first-text advantage on a fresh process. Keeping a loaded model would matter.

Every measured paced Sherpa run produced:

> AND SO MY FELLOW AMERICANS ASK NOT WHAT YOUR COUNTRY CAN DO FOR YOU ASK WHAT YOU
> CAN DO FOR YOUR COUNTRY

Whisper's control retains the same words with punctuation/case. All five Sherpa
offline runs also match after normalization. This single clear English quote is
only a feasibility gate. Sherpa revises “AMERICAN” to “AMERICANS” in its partial
hypotheses; the complete traces are retained rather than equating a first token
with a stable final caption.

### Every measured paced run

Times are milliseconds; RSS is KiB. The activity column means decode API calls
for Sherpa and successful inference passes for Whisper; they are not equivalent.

| Engine / run | Startup | First partial | First final | Completion | Activity | CPU | RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| sherpa-paced-1 | 1495.659 | 1079.532 | 5884.212 | 11033.037 | 35 | 728% | 269,660 |
| sherpa-paced-2 | 1574.584 | 1083.743 | 5883.597 | 11027.336 | 35 | 729% | 269,456 |
| sherpa-paced-3 | 1536.277 | 1088.066 | 5884.697 | 11029.572 | 35 | 729% | 270,816 |
| sherpa-paced-4 | 1535.953 | 1083.580 | 5884.863 | 11030.083 | 35 | 731% | 265,988 |
| sherpa-paced-5 | 1557.654 | 1079.990 | 5877.776 | 11028.756 | 35 | 730% | 268,868 |
| whisper-paced-1 | 120.756 | 2560.128 | 11886.325 | 11886.328 | 7 | 184% | 220,784 |
| whisper-paced-2 | 117.863 | 2560.076 | 11871.732 | 11871.734 | 7 | 185% | 221,072 |
| whisper-paced-3 | 120.932 | 2560.328 | 11842.480 | 11842.483 | 7 | 186% | 221,244 |

First-final min/median/max: Whisper **11,842.480 / 11,871.732 / 11,886.325 ms**;
Sherpa **5,877.776 / 5,884.212 / 5,884.863 ms** (different endpoint semantics).

Sherpa paced decode wall time is **857.030 ms median (743.105–896.743)**,
with 35 decode API calls in every run. Full-process CPU is much larger than
this synchronous decode duration suggests. The upstream
[session setup](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.7/sherpa-onnx/csrc/session.cc)
sets intra/inter-op counts for each session, and the
[online model](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.7/sherpa-onnx/csrc/online-zipformer-transducer-model.cc)
creates separate encoder, decoder, and joiner sessions. ONNX Runtime documents
[worker spinning](https://onnxruntime.ai/docs/performance/tune-performance/threading.html)
as enabled by default and CPU-consuming. Multiple pools/spinning are a plausible
explanation, not a profiled causal proof. No spin/thread-setting sweep was run.
The actual **3.94× process CPU cost** versus Whisper must not be hidden behind
the favorable decode time. CPU percentage is not a battery-energy measurement.

Sherpa's median peak is **263.14 MiB**, versus **215.89 MiB** for Whisper, about
22% more. The highest peak across Sherpa short runs is **271,964 KiB
(265.59 MiB)**. These are process measurements, not model-file cache estimates.

### Fastest-possible throughput (separate from caption latency)

Each run still feeds 20 ms increments, without pacing, then performs the same
EOF flush. RTF = processing/flush wall time divided by 11.000 seconds. Startup,
WAV decoding, and JSON serialization are excluded. All runs use 35 decode API
calls and have normalized exact match. Offline partial timestamps in raw JSON
are not real-time latency results.

| Run | Startup ms | Completion ms | RTF | Decode wall ms | CPU | Peak RSS KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| sherpa-offline-1 | 1628.487 | 796.150 | 0.072377 | 759.521 | 437% | 266,924 |
| sherpa-offline-2 | 1574.284 | 888.141 | 0.080740 | 846.651 | 466% | 267,736 |
| sherpa-offline-3 | 1516.703 | 658.063 | 0.059824 | 630.741 | 419% | 271,964 |
| sherpa-offline-4 | 1578.404 | 645.391 | 0.058672 | 618.510 | 409% | 269,096 |
| sherpa-offline-5 | 1520.097 | 711.292 | 0.064663 | 684.290 | 433% | 268,708 |

Completion min/median/max: **645.391 / 711.292 / 888.141 ms**.
RTF min/median/max: **0.058672 / 0.064663 / 0.080740**; median throughput is
about **15.46× real time**. Startup min/median/max:
**1,516.703 / 1,574.284 / 1,628.487 ms**. Decode wall time:
**618.510 / 684.290 / 846.651 ms**. CPU: **409 / 433 / 466%**;
peak RSS: **266,924 / 268,708 / 271,964 KiB**.
The old Whisper offline figure is contextual only: it used different internal
chunk/backpressure behavior and was not remeasured in this control. No current
same-machine offline speedup ratio is claimed.

### Bounded stability check

One recognizer and one continuously fed online stream processed 28 concatenated
JFK repetitions (**308.000 seconds**) with no gaps or rebuilds between fixtures.
Completion was **308.025 seconds** from replay start; the process
exited **0** after **309.691 seconds** including startup. It performed
**963 decode calls**, **34 endpoint resets**, and wrote **zero stderr bytes**.
The transcript occupied 2939 UTF-8 bytes; detailed hypothesis tracing
was disabled for this bounded run.

Ten steady `/proc` samples, every 30 seconds from 30 through 300 seconds,
all reported **268,988 KiB RSS**, **943,608 KiB virtual size**, and **10 threads**.
The initial pre-load sample is retained separately in raw evidence. There was
**no observed RSS growth** across the steady samples, no fatal error, and a clean
EOF/exit. This is a five-minute diagnostic stability check, not a leak proof,
production acceptance, full audio/UI soak, or multi-hour validation.



## Reproduction and verification boundaries

[The isolated tool README](../experiments/sherpa-streaming/README.md) contains
build, checksum, GNU time, paced/offline, and bounded stability commands. To fetch
each model file, use the base URL
`https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-21/resolve/9a65b6ea94c311ca770c2bf895b30f456a22d703/`
plus its filename from the hash table; verify all four SHA-256 values above.

Implemented: only the standalone diagnostic, its own lockfile and tests, an
independent CI job, and this report/evidence. Production app, Whisper, PipeWire,
and root Cargo manifests/lockfile are unchanged. No backend registry, engine
manager, settings, translation, or production Transcriber was introduced.

Local gates passed: root workspace formatting, Clippy with warnings denied,
87 tests, and rustdoc with warnings denied; the isolated package's release build,
formatting, Clippy, two metric/bookkeeping tests, and rustdoc also passed.
Four additional release error-path checks rejected missing arguments, zero
repeats, a wrong-format WAV, and missing model files with nonzero exits.
CI and automated review status are recorded in the PR and final owner report.
CI tests the diagnostic bookkeeping, not model recognition or hardware latency.

Runtime/hardware evidence is only the measured release diagnostics on this
Ubuntu AMD64 host. The optional microphone path was skipped to keep the spike
focused; no microphone recognition or GTK/PipeWire integration is claimed.
No ARM64 or Windows runtime validation, audible live-speaker accuracy, native
hang cancellation, multi-hour soak, or battery/power measurement was performed.

Final simplification review: the standalone package is the smallest separate
Cargo boundary that avoids burdening production with the native dependency.
No custom download/build framework was retained. Inputs are bounded to the JFK
format and at most 60 repeats; detailed hypothesis traces are omitted for the
stability run. The small driver remains useful for reproducing the latency/CPU
trade-off before any separately authorized integration milestone.

## Decision

1. **Is this true streaming?** Yes. One persistent `OnlineStream` accepts paced
   20 ms input, and `OnlineRecognizer` produces changing hypotheses before EOF.
   It does not re-transcribe expanding Whisper windows or process the whole WAV
   up front. A first token at about 1.08 seconds demonstrates incremental output,
   though it is not instantaneous or speech-onset latency.
2. **How soon is useful text produced?** “AND” at 1,083.580 ms median, “AND SO MY
   FELLOW” at 2,044.400 ms median. The first token gives early feedback; its
   information content is limited. Full-word stability and user-perceived
   caption usefulness need real conversation tests.
3. **Is final JFK recognition acceptable?** Yes for this fixture: the entire
   normalized quote matches in 5/5 paced and 5/5 offline Sherpa runs. This cannot
   establish broad accuracy, robustness to noise, accents, or conversational
   usefulness. Endpoint segmentation differs from Whisper.
4. **How much RAM?** Paced median peak 269,456 KiB (263.14 MiB), maximum short-run
   peak 271,964 KiB. Moderate desktop memory cost, about 22% above Whisper's
   current paced median. Model files occupy an additional 181.37 MiB on disk;
   that disk number must not be added to RSS as a separate measured RAM cost.
5. **How much CPU?** Paced median 729%, versus Whisper's 185%, using GNU time.
   This is about 7.29 logical CPUs on average for the whole process, despite
   `num_threads = 4` per session. It is an important live-caption desktop cost.
6. **Enough improvement to justify production integration?** Not yet in this
   configuration. Warm-model first-text latency improves materially by 2.36×,
   but high CPU and 1.54-second model startup offset the benefit. Startup plus
   first text is nearly the same as Whisper. The result warrants focused CPU
   investigation and representative English accuracy/latency testing before
   committing to integration. No lower-thread/spin tuning benefit is claimed.
7. **What is still required for an LCRT Transcriber?** A bounded worker adapter
   conforming to the existing core contract; explicit ownership, cancellation,
   error propagation and shutdown; audio-format adaptation and backpressure;
   partial revision and segment-to-caption rules; finalization behavior;
   production latency instrumentation; model provisioning and native dependency
   verification; then real microphone/system-audio and GTK flow validation.
   These are future requirements, not justification for a backend-manager
   framework in this spike.
8. **Multilingual limitations?** This candidate is English-only. It proves
   neither Vietnamese nor Japanese streaming. The pinned official
   [Vietnamese Rust example](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.7/rust-api-examples/run-zipformer-vi.sh)
   runs an offline Zipformer example; it cannot inherit this streaming result.
   Each language needs its own supported model, streaming/latency evidence,
   recognition fixtures and live-speaker evaluation. No translation is tested.

**B. PROMISING BUT NEEDS MORE LANGUAGE/ACCURACY TESTING**

The measured English streaming latency and exact JFK final text justify keeping
this small isolated benchmark for a follow-up evaluation. They do not justify
shipping the present CPU-heavy configuration. CPU/thread-pool behavior and
representative language/accuracy evidence are explicit gates before integration;
this single quotation is a feasibility result, not production acceptance.
