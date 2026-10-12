# Japanese and Vietnamese streaming pilot

The short natural-speech comparison supports a Japanese/Vietnamese dogfood
pilot: both languages produced much cleaner continuous captions than Whisper
Tiny, with fewer repetitions and lower CPU use. It does **not** establish a
1.5–2 second first-useful-caption result. Opening words remain unreliable.

## Scope and native proof

Baseline: `b88b907cdae201014dcfb43cc6162b35c4dc9eca`. Explicit Japanese and
Vietnamese use the bundled streaming model; Auto, other languages, and custom
Whisper models retain their existing behavior. Offline Translation receives
the same source contract, with all four existing translation directions intact.

The source patch modifies one upstream C++ header. It uses existing stream
options and leaves C ABI structures, exported signatures, and Rust bindings
unchanged. IDs derived from the verified vocabulary are `<JA>` = 6 and `<VI>` =
4. Preceding predictor padding remains -1; ordinary streams still seed 0.
Warmup does not mutate live stream context. Invalid or changed configuration
fails explicitly, and Rust rejects a native library without acknowledgment.

The first state test found retained decoder output after reset. One correction
cleared the cached predictor output before assigning the fresh language result.
The corrected test inspected real decoder state and passed; short Japanese and
Vietnamese replay emitted plausible text. A later 31.88-second Vietnamese
two-utterance replay yielded both full segments across two endpoints.

[Reproduction steps](../experiments/sherpa-ja-vi/README.md),
[native pins](../packaging/native/runtime.json), and
[model pins](../packaging/models.json) describe the exact artifacts. The original
Apache-2.0 model, its matching decoder/vocabulary, and the official int8 export
script establish redistribution provenance. Native and model notices ship in
the package, including ONNX Runtime's third-party notices.

## Natural speech comparison

Four public [Google FLEURS](https://huggingface.co/datasets/google/fleurs)
validation clips, licensed CC BY 4.0, supplied known reference text. Two clips
per language were converted to mono 16 kHz signed 16-bit PCM without gain
normalization. Each engine received identical samples in paced mode. Runs were
serial, alternated engine order, and used one streaming inference thread.
Model load time is separate from phrase timing. Phrase criteria were chosen
before replay. This is a short comparison, not a statistical accuracy study.

| Clip | Reference units | Whisper edits | Streaming edits | First predefined correct phrase: Whisper / streaming |
| --- | ---: | ---: | ---: | --- |
| Japanese 0, 11.10 s | 46 characters | 200 | 3 | Not observed / 3.08 s |
| Japanese 1, 22.86 s | 81 characters | 501 | 9 | 7.92 s / not observed |
| Vietnamese 0, 14.94 s | 34 words | 265 | 2 | Not observed / 2.76 s |
| Vietnamese 1, 10.20 s | 39 words | 43 | 7 | Not observed / not observed |

Counts use NFKC, case folding, and punctuation removal. Japanese uses character
edit distance; Vietnamese uses word edit distance. Insertions from repeated
Whisper text explain counts larger than reference length. Japanese script and
number spelling differences also count as edits. The opening phrases of the
second streaming clips were misrecognized, so they do not qualify as useful
phrase successes despite better final text. Examples include Japanese `たしは`
in place of `私は`, and Vietnamese missing the satellite-phone opening phrase.

The streaming traces showed no repeated phrase expansion and stayed within
45–85 ms of the paced audio duration at completion. They used 24–32% of one
CPU across startup and replay, 504,004–513,716 KiB peak RSS, and 2.64–3.12 s
startup. Whisper used 119–256% CPU and 134,172–135,032 KiB peak RSS.
Streaming uses substantially more memory. These measurements are from the
development machine, not Pentium hardware.

[Public references, hashes, and observed traces](evidence/streaming-ja-vi/ab.json)
retain the evidence. `python3 experiments/sherpa-ja-vi/summarize.py` recomputes
the table. No audio or model binaries are committed.

## Real GTK and PipeWire smoke

The extracted installable package ran through actual GTK Start, Stop, Restart,
and Close actions against a dedicated PipeWire system-audio sink. This used
X11/Xvfb on Ubuntu AMD64, not a native Wayland overlay or microphone hardware.
Japanese and Vietnamese produced readable source captions, with Stop preserving
the last result and subsequent Start creating a fresh recognizer.

An initial Vietnamese restart showed only the early phrase despite continued
capture. That run lacked an input recording, so its cause is **unresolved**.
Two subsequent recorded rounds and three subsequent rounds without a recording
link produced the full Vietnamese utterance. Source review found no reset or
sample-loss mechanism in the new adapter. Similar chunk counts alone do not
prove equal input: PipeWire EMPTY buffers become silence. This remains a dogfood
caveat; these later passes are not evidence that the earlier symptom was fixed.

Actual GTK text buffers, rather than accessibility labels, supplied the
[recorded restart observations](evidence/streaming-ja-vi/ui-capture-report.json)
and [constrained observations](evidence/streaming-ja-vi/ui-constrained-report.json).
The constrained run used a user scope with two CPU cores and `MemoryMax=2G`.
The observed scope peak reached 678,907,904 bytes while active; this is a sampled
lower bound on the final peak, since the completed transient scope was removed.
App peak RSS in those rounds was 633,500–643,900 KiB. Stop completed in
0.55–0.84 seconds, with no queue-drop warnings. This smoke catches major
resource failures; it is not low-end hardware certification.

JA→EN and VI→EN both displayed source and target updates through the unchanged
CTranslate2 wrapper. The Japanese target was poor (`And that's what I'm going
to do.` for the gap-year passage). This validates compatibility and responsiveness,
not translation accuracy. EN→JA and EN→VI remain on the unchanged Whisper path
and were covered by existing contract tests, without a new audio campaign.

All app probes ran with `bwrap --unshare-net`; successful `strace -f` traces for
the app and its translation child contained zero AF_INET or AF_INET6 socket
calls. Models were present before startup. No inference network fallback or
runtime downloads were used. Temporary preferences and IPC sessions were
isolated from the owner's settings. Speech-test execution stayed under ten
minutes, excluding builds and installation preparation.

## Package and verification boundaries

The dogfood `.deb` bundles the four verified streaming files and source-built
C API/ONNX Runtime libraries in normal `/usr/share/lcrt` and `/usr/lib/lcrt`
paths. Its original tested build was 607,921,708 bytes with 956,370 KiB installed
size, versus 354,172,260 bytes and 601,911 KiB for the prior local package.
Final documentation and provenance staging can change these sizes slightly.

The host package targets Ubuntu 26 AMD64 and requires its host-derived shared
library and Python ABI dependencies. Build on the Ubuntu release intended for
installation; this artifact has not been installed on Ubuntu 24.04, ARM64, or
Windows. Package extraction and execution were tested. System-wide installation,
real speaker-loopback hardware, microphone capture, native Wayland overlay,
and prolonged sessions remain unverified.

Focused tests cover Unicode replacement/finalization, vocabulary-derived IDs,
missing and same-size-corrupt artifacts, routing, native state/reset behavior,
and real native Stop/restart. Packaging tests reject changed patch/source bytes
before building. CI keeps every existing job and adds patched-native proof,
real native lifecycle checks, and the separate experiment's quality gates.
Exact final-SHA local gates, CI, and PR review status are reported in the PR
handoff rather than inferred from this runtime smoke.
