# LILOPOP ship-fast offline acceptance

Ubuntu AMD64, 2026-10-03. This report supersedes the offline-translation
limitations in the historical PR #30 acceptance report.

## Implemented and unit-tested

- App-facing name, title, desktop launcher, AppStream metadata, README, SVG
  icon, 256-pixel raster icon, and packaged description use LILOPOP.
- Default speech model: Whisper Tiny multilingual, Fast. No bundled Balanced
  model; Advanced still permits an optional custom Whisper model.
- First inference needs one second of speech; subsequent passes need 500 ms.
  Token budgets follow buffered audio rather than the whole rolling window.
  The existing eight-second input/window bounds remain.
- Local CTranslate2 / Helsinki OPUS-MT int8 translation supports JA→EN,
  EN→JA, VI→EN, EN→VI. No JA↔VI pivot, OpenAI key, runtime download, or
  automatic online fallback. Auto is available for captions; offline
  translation requires an explicit source language.
- Independent bounded worker: one pending window, one in flight, 2 KiB input,
  8 KiB output, ten-second inference deadline. Stop kills/reaps the child;
  final drain is at most one second. Revision checks discard stale target
  results. Empty early hypotheses leave the session running.
- Regression coverage includes stalled translation with immediate source
  delivery, child cancellation, stale pause/resume results, empty hypotheses,
  supported pairs, UTF-8 bounds, speech scheduling, and packaging integrity.

## Short runtime checks

The extracted Debian package ran through the actual GTK→PipeWire→Whisper
execution path using GTK’s X11 backend on the Wayland desktop. One normalized FLEURS utterance per language was played through
`pw-play` into system audio. Samples are CC BY 4.0 from google/fleurs; no
recordings or model binaries are committed.

First useful caption means the first readable phrase related to the utterance,
not the first nonempty output. These are single observations, not guarantees.

| Source | First emitted text | First useful text | Useful text |
| --- | --- | --- | --- |
| EN | 1.37 s | 1.83 s | However, |
| JA | 1.41 s | 2.85 s | インターネットで |
| VI | 1.39 s | 3.50 s | Văn hóa, là bộ |

Japanese baseline at the exact PR #30 source SHA
`dc87796e4b5cb7004002e7e4c211a811bb3ea010`, using Base and the same normalized
WAV in the existing paced diagnostic: first emitted text 3.52 s. Tiny's paced
first output was 1.30 s. Both early hypotheses could be unrelated to the speech;
this comparison proves earlier output, not a 1.3-second useful Japanese caption.

The desired 1–1.5 s useful-caption target was not reached for JA or VI in these
samples. Tiny makes more recognition mistakes; overlapping phrases on
continuous speech remain an existing limitation.

Each offline caption and translation run used `bwrap --unshare-net` and
`strace -f -e trace=socket,connect`. All seven runs recorded **zero AF_INET or
AF_INET6 calls**, including the Python translation child. Local AF_UNIX audio
and desktop IPC remain available. Vocabulary cloud lookup is disabled in an
active offline session.

All four pairs also passed direct local phrase checks with “Today is a beautiful
day” / “今日は良い天気です。” / “Hôm nay thời tiết rất đẹp.” Expected target
languages and basic meaning were checked; end-to-end accuracy still depends on
Whisper's source text. Translation shows the current speech window, not a saved
history. Empty partial phrases can wait for more speech.

Caption Stop: 0.31–0.62 s. Translation Stop: 0.83–1.38 s after the empty-result
fix. Application RSS at the end of the short runs: roughly 261–294 MiB; the local MT
worker was measured separately at 255 MiB peak RSS while translating four
short phrases across all four pairs. No source stall or accumulated lag was
observed in the final translation runs. This is a short sanity check, not a soak.

## Packaging and verification

External source archives and Python wheels have exact size and SHA-256 pins.
The original model archives carry CC BY 4.0 licenses; their LICENSE and README
files are preserved beside each converted model, with source/author attribution.
The build rejects mismatches and converts weights to int8 before packaging.
The package includes OPUS CC BY 4.0, Whisper MIT, and bundled runtime notices.
The tested host package requires Python 3.14. Building on Ubuntu 24.04 selects
pinned Python 3.12 wheels. Unsupported packaging ABIs fail explicitly.
Ubuntu 24.04 package runtime, ARM64 runtime, Windows runtime, microphone input,
and constrained-CPU targets were not verified by this task.

Local gates passed: formatting, Clippy with warnings denied, locked workspace
all-feature tests (262 Rust tests), rustdoc with warnings denied, and diff
whitespace checks. Two Python packaging integrity tests also passed (264 total).

Final host package: 354,204,604 bytes (337.80 MiB); installed-size metadata:
601,898 KiB (587.79 MiB). Sizes vary with platform/toolchain and Python ABI.
The package was extracted into an isolated prefix and run without user model
selection or an OpenAI key. No model or audio binaries were added to Git.

Live controls passed on an isolated X11 display: target hide/show, source
hide/show, pause/resume, add Vietnamese while running, and remove Japanese.
The same test verified the application remained responsive and Stop completed.
This UI check used real GTK pointer/accessibility actions; it is separate from
the real-desktop audio playback checks above.

CI and exact-head automated review are separate merge gates, checked on GitHub
before merging. Main remains outside this task's scope.
