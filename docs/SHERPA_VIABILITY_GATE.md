# Sherpa English viability gate

**B. KEEP SHERPA EXPERIMENTAL.** CPU reduction is proven on this host. Select
**one thread, CPU provider defaults** for any separately authorized follow-up.
The three-word advantage survives, but five-word output is no earlier than
Whisper. This gate does not justify production integration on useful-text
latency alone. No production integration, multilingual work, or V2 was done.

## Results

Medians; latency/startup in milliseconds, peak RSS in MiB. Current rows each
have one excluded warm-up and **three** measured paced runs. Historical rows
reuse [PR #23 evidence](SHERPA_STREAMING_SPIKE_RESULTS.json), unchanged.

| Configuration | CPU | Startup | First text | 3 words | 5 words | RSS | Exact match |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Historical Sherpa, 4 threads | 729% | 1536.277 | 1083.580 | 1724.891 | 2364.635 | 263.14 | 5/5 |
| Historical Whisper, 4 threads | 185% | 120.756 | 2560.128 | unknown | unknown | 215.89 | 3/3 |
| Current Sherpa, 4 threads (control) | 737% | 1154.158 | 1079.992 | 1720.994 | 2361.207 | 261.45 | 3/3 |
| **Current Sherpa, 1 thread** | **14%** | **808.237** | **1079.865** | **1721.064** | **2362.997** | **259.97** | **3/3** |
| Current Sherpa, 4 threads, no spinning | 19% | 830.795 | 1076.699 | 1716.052 | 2356.960 | 259.60 | 3/3 |
| Current Whisper, 4 threads | 135% | 63.108 | 2340.287 | 2340.287 | 2340.287 | 215.99 | 3/3 |

Selected one-thread min–max: CPU **14–14%**, startup **807.723–846.459 ms**,
first text **1079.748–1081.581 ms**, three words **1721.025–1723.145 ms**,
five words **2360.674–2363.440 ms**, peak RSS **265468–271408 KiB**.
Current four-thread control CPU spans 725–748%; no-spin spans 19–20%.
Completion medians: four-thread control **11022.129 ms**, one thread
**11016.494 ms**, no-spin **11013.050 ms**, Whisper **11608.731 ms**.
All measured processes exited 0 with empty stderr and exact normalized final
JFK text. Raw timings, hypotheses, commands, warm-ups and hashes are retained in
[SHERPA_VIABILITY_GATE_RESULTS.json](SHERPA_VIABILITY_GATE_RESULTS.json).

## What the words mean

Milestones are first observation of a hypothesis with at least 1, 3 or 5 tokens:
lowercase, remove ASCII punctuation, split whitespace. They measure **token
count, not correct/stable words or visible pixels**. Sherpa's existing history
supplies both historical and current thresholds, all before its first endpoint.
Historical Whisper JSON lacks partial text, so its missing values cannot be
inferred. The smallest diagnostic-only addition records three timestamp/text
pairs in Whisper; inference, queues and pacing are unchanged.

Sherpa emits “AND”, then “AND SO MY”, then “AND SO MY FELLOW AMERICAN”. It later
corrects “AMERICAN” to “AMERICANS”. Current Whisper's first hypothesis is
“And so my fellow men”: five words, also requiring correction. Both final
transcripts normalize to:

> And so my fellow Americans ask not what your country can do for you ask what
> you can do for your country

One-thread Sherpa's first-token advantage is **1260 ms (54%)**, but its
three-word advantage is **619 ms (26%)** and five words are **23 ms later**
than current Whisper. The three-word fragment contains little meaning; the
five-word difference is small and near the 20 ms Whisper observation tick.
There is no demonstrated material five-word advantage. Do not compare Sherpa's
“AND” to Whisper's five-word hypothesis as equally useful text.
Startup plus three-word latency is about **2.53 s Sherpa versus 2.40 s Whisper**
(sum of medians, not measured GUI startup). Keeping the model loaded matters.

## CPU investigation and exact settings

Pinned Sherpa-ONNX **1.13.7** constructs separate encoder, decoder and joiner
[ONNX sessions](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.7/sherpa-onnx/csrc/online-zipformer-transducer-model.cc).
Its [session setup](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.7/sherpa-onnx/csrc/session.cc)
sets both intra/inter-op counts per session. Default CPU execution is sequential;
inter-op pools require parallel execution. Four intra-op threads mean the caller
plus three workers per session, consistent with PR #23's ten observed process
threads. One thread removes these extra workers; the selected smoke observed
one process thread.

[ONNX Runtime's official threading documentation](https://onnxruntime.ai/docs/performance/tune-performance/threading.html)
explains that worker spinning defaults on and consumes CPU while waiting.
Pinned Sherpa supports `provider = "cpu:<config-file>"` and forwards keys prefixed
`SessionConfig.` to `Ort::SessionOptions::AddConfigEntry`. This is reachable
through Rust's public `OnlineModelConfig.provider`; no fork, patch, private API,
or invented environment variable was needed. The verified native archive also
contains this forwarding implementation.

Exactly three configurations were tested:

- Control: `num_threads=4`, `provider="cpu"`, default spinning.
- Lower-thread: `num_threads=1`, `provider="cpu"`, defaults otherwise unchanged.
- Additional: `num_threads=4`, provider points to
  [cpu-no-spinning.conf](../experiments/sherpa-streaming/cpu-no-spinning.conf), with
  `SessionConfig.session.intra_op.allow_spinning=0` and
  `SessionConfig.session.inter_op.allow_spinning=0`.

At the same four-thread count, disabling spinning cuts median CPU **737% → 19%**
(97.4%). One thread cuts it to **14%** (98.1%; historical comparison **729% → 14%**).
This controlled intervention strongly implicates spinning, beyond merely changing
thread count; it is not a per-function CPU profile. No global/shared pool was
configured. One thread wins the balance: lower CPU and slightly faster startup
than no-spin, similar memory, identical final words, and only about 5–6 ms slower
3/5-word milestones. No additional configuration sweep is warranted here.

## Method and verification

2026-09-10, same Ubuntu AMD64 i5-12500H / 16 logical CPU workstation as #23,
Ubuntu 26.04, Linux 7.0.0-31-generic, Rust 1.98.0. Starting clean `develop`:
`584840ca94f369af704cdf9c83cfc615b5dc26ad`; starting CI run `34370813543` passed.
Same Zipformer English 2023-06-21 int8 encoder/joiner and float decoder,
16 kHz / 80 features, greedy search, endpoint defaults, 20 ms cumulative pacing,
and 0.3-second unpaced EOF zero tail as #23. Whisper remains tiny.en, English,
four threads. Startup excludes WAV decoding; replay times exclude model load.
GNU time CPU/RSS cover the entire process, including startup and paced idle time.

All four model hashes, Whisper model hash, and native archive hash matched #23;
all 15 extracted native-cache files matched the verified archive. JFK SHA-256:
`59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`.
The temporary inputs were restored from the pinned upstream URLs in the prior
report. [Reproduction commands](../experiments/sherpa-streaming/README.md#viability-gate-24)
include all settings; raw evidence includes measured source/executable hashes.

Builds and checks finished before measurements. Each series settled for ten
seconds, ran one excluded warm-up, then three sequential measured paced replays.
Order: control, one thread, no-spin, Whisper. The control was justified by a new
session; Whisper required new word observations. No offline series or long soak
was repeated. The owner intended the workstation to be idle; no deliberately
started build/benchmark overlapped. Background OS activity and thermal/order
bias are not eliminated. Current Whisper is faster than #23 and close to #22;
no cause of historical drift is asserted. Historical results remain unchanged.

Implemented: standalone configuration arguments/file and diagnostic-only Whisper
metrics. Unit-tested: normalization thresholds, revisions, final-only threshold
arrival and JSON output; all **88 workspace + 2 isolated tests** passed. Local
formatting, Clippy (all targets/features, warnings denied), tests and rustdoc
(warnings denied) passed for both Cargo workspaces. Release invalid-thread and
missing-config checks reject input before inference. One selected 11-second
paced smoke completed with exact final text, empty stderr, and exit 0; its
three-second `/proc` sample recorded one thread. CI/review outcomes are recorded
in the PR; CI does not run recognition benchmarks.

Hardware/runtime evidence covers these release diagnostics on this Ubuntu AMD64
host only. No GTK pixels, microphone/system-audio integration, ARM64/Windows,
noise/accent robustness, battery power, cancellation under native hangs or new
long-term stability claims. One quotation is insufficient for broad accuracy.
Final simplification review retained existing Sherpa traces and only three
bounded Whisper observations; no dependency, production state or framework was
added. CPU is no longer the blocker. Representative useful-caption evidence is
still needed before paying production integration complexity.
