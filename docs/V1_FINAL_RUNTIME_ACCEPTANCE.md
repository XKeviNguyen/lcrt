# V1 final runtime acceptance

This report records the Ubuntu AMD64 end-to-end acceptance work for the current
local Whisper V1 path. It reuses established evidence from PRs #19–#24 and adds
the runtime checks that were still missing in PR #25.

## Final scope

Base `develop`: `1b084aa50bb30f377b0f7cdec095279121bf8a6c`.

The final PR intentionally retains only two production changes:

1. rolling **partial** Whisper passes disable temperature fallback so noisy live
   input cannot multiply a single partial inference into several retries and
   overflow the bounded input queue; final passes keep whisper.cpp's existing
   fallback behavior;
2. `--smoke-seconds` accepts up to 3600 seconds so the existing integrated
   diagnostic can run a 30-minute soak.

An attempted rolling-window heuristic that skipped up to two leading words was
**fully reverted after repeated review found cases where it could silently delete
real speech**. `crates/lcrt-stt-whisper/src/transcript.rs` is therefore restored
exactly to the protected `develop` implementation. V1 deliberately prefers a
visible repeated phrase over silently losing spoken words.

## Test identity

- OS: Ubuntu 26.04 LTS, Linux 7.0.0-34-generic, x86_64, GNOME Shell 50.1 on
  Wayland, PipeWire 1.6.2.
- CPU: 12th Gen Intel Core i5-12500H, 16 logical CPUs. Rust 1.98.0.
- Model: `ggml-tiny.en.bin`, 77,704,715 bytes, SHA-256
  `921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f`.
  This model is English-only.
- System-audio fixture: whisper.cpp v1.7.6 `samples/jfk.wav`, SHA-256
  `59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`.
- Source discovery exposed the built-in microphone and built-in output monitor.
- GNOME did not advertise layer shell, so tested windows used the standard-window
  fallback.

Every application runtime used a local model inside a network-isolated
`bwrap --unshare-net` environment. No sudo or workstation-wide network change
was used.

## Acceptance summary

| Acceptance item | Result |
| --- | --- |
| Audible human microphone transcription | PROVEN, limited accuracy on noisy mic |
| Real system-audio transcription | PROVEN |
| Same-window Start → Stop → Start | PROVEN |
| Stop while startup is in progress | LIMITED: acquisition-first branch runtime-tested; cancel-first branch unit-tested |
| Window close / termination | PROVEN |
| Offline operation with local model | PROVEN for exercised runs |
| Integrated PipeWire → Whisper → controller → GTK soak | PROVEN for 30 minutes on an acceptance build |
| Speech-to-visible-caption latency | LIMITED practical measurement |
| No stuck LCRT process / PipeWire node | PROVEN |
| Vietnamese / Japanese speech | NOT TESTED; model is English-only |

## Microphone live speech

The owner read:

> Real-time captions help me follow every conversation.

Observed results:

- speech reached the production caption UI and partial text changed while the
  owner was speaking;
- best output included `Real-time captions ... help me follow every conversation`;
- `Real-time` was also misrecognized as `Route time` / `Root time`;
- the beginning of speech was not dropped, but the first words were frequently
  inaccurate;
- partials changed roughly every 1.5 seconds;
- one measured Stop returned the UI to idle in about 1.0 second.

The microphone noise floor was well above the fixed speech RMS threshold. That
kept the rolling window active during room noise and exposed the queue-overflow
bug described below.

## System audio

The output monitor was exercised with the checksum-pinned JFK fixture.

- first visible caption: approximately 2.65–2.69 seconds after playback start;
- final quotation recognized correctly for the acceptance purpose;
- process exited 0;
- no leftover LCRT process or PipeWire node.

## Same-window lifecycle

One retained normal application window completed four Start / Stop sessions and
additional fast Stops around startup.

- every Start acquired fresh audio;
- one LCRT PipeWire node existed while active and zero while idle;
- session state did not carry into the next session;
- idle thread count returned to the expected level;
- the window remained usable after the fast-stop attempts.

In all runtime startup-stop attempts, audio acquisition committed before Stop.
The opposite linearization remains covered by the deterministic PR #21 test
`cancellation_winning_at_the_audio_boundary_does_not_start_audio`; no sleeps or
production hooks were added just to force the race.

## Close and termination

Idle and active-session closes exited successfully and left no LCRT process or
LCRT PipeWire node. Accessibility-driven close exercised the GTK close path; an
owner UI close was also observed as window disappearance, but the exact physical
pointer action was not independently verified.

## Offline operation

The exercised GUI/audio runs operated inside a network namespace with no normal
network access while retaining local Wayland, D-Bus and PipeWire Unix sockets.
The local Whisper path required no network. This does not claim physical network
disconnection.

## Integrated soak

A 30-minute integrated acceptance run exercised:

`output monitor → PipeWire → Whisper → controller/event path → GTK caption label`

Observed:

- 139 JFK plays;
- 966 visible caption changes;
- longest observed gap between caption changes: about 3.0 seconds;
- steady RSS approximately 348,924 → 348,972 KiB (+48 KiB);
- 17–21 threads during the active soak;
- no warnings or errors;
- clean Stop / exit and no leftovers.

The soak was performed on an intermediate acceptance build while the discarded
transcript experiment was still present. It remains evidence for the integrated
capture/controller/UI stability path, but it is **not claimed as a 30-minute run
of the exact final PR source**. The final transcript assembler is not a newer
unverified heuristic: it has been restored exactly to the already-established
`develop` implementation.

## Latency evidence

The GTK caption label was observed through AT-SPI; this is label-update timing,
not direct pixel photometry.

- system audio: about 2.65–2.69 seconds from playback start to first label text;
- one human-microphone session: first text about 0.8 seconds after estimated
  speech onset; first recognizable words about 2.3 seconds after onset; complete
  sentence about 1.2 seconds after speech energy ended;
- microphone onset uncertainty was about ±0.3 seconds and the single session is
  not a latency distribution.

## Production defect fixed: noisy-mic queue overflow

On unmodified `develop`, the noisy real microphone ended sessions after roughly
24–28 seconds with:

`Whisper input queue reached its 256-chunk bound`

This reproduced three times.

Normal partial inference took around 0.8 seconds, but whisper.cpp temperature
fallback could re-decode repetitive/hallucinated partials several times, turning
individual passes into multi-second work and letting the bounded queue fill.

Final behavior in this PR:

- `InferenceKind::Partial`: `temperature_inc = 0`, so each rolling partial is a
  single decode; the next rolling-window pass will see the audio again.
- `InferenceKind::Final`: keep whisper.cpp's normal fallback, because no later
  rolling pass re-decodes that final audio.

After disabling fallback on the partial-driven noisy path, the same condition ran
60 seconds and stopped cleanly. A later final-source Stop check ran three 15-second
noisy-microphone cycles with Stop around 1.37–1.51 seconds; those final passes did
not happen to trigger fallback. Worst-case fallback Stop latency was therefore
not observed and remains bounded by the existing finish timeout.

The deterministic JFK path retained the same final transcript.

## Transcript repetition decision

The acceptance run also exposed repeated phrases when Whisper changes words at a
rolling-window boundary. Several increasingly guarded heuristics were tried to
drop suspected garbled leading words. Codex review repeatedly produced valid
counterexamples where those rules could delete genuinely new speech.

The heuristic has therefore been removed rather than made more complicated.
The final policy is conservative:

- keep the established exact overlap algorithm from `develop`;
- tolerate visible repeated text when Whisper changes boundary words;
- never silently discard unmatched leading words based on a fuzzy/count-only
  guess.

Adaptive VAD/noise-floor handling or a future ASR/backend may reduce repetition
without introducing lossy transcript heuristics.

## Verification boundaries

The PR must finish with:

- formatting clean;
- Clippy with warnings denied;
- workspace tests green;
- rustdoc with warnings denied;
- `git diff --check` clean;
- all PR CI jobs green on the final HEAD;
- Codex review on the final HEAD with no open P0/P1/P2;
- post-merge CI green on the exact merge SHA.

Removing the unsafe transcript heuristic does not justify repeating the 30-minute
soak or the human microphone checkpoint. A focused deterministic replay plus the
normal quality gates is sufficient to verify the reversion and retained fixes.

## Known limitations

- Fixed RMS speech detection is weak when the microphone noise floor exceeds the
  threshold; adaptive VAD/noise-floor behavior is future work.
- Exact rolling-window overlap can visibly repeat phrases when Whisper changes
  boundary words. This is explicitly preferred over risking silent word loss.
- A final Whisper pass may use temperature fallback and therefore take longer
  than a partial pass; the existing bounded finish timeout remains the guard.
- Accuracy evidence here is one English speaker/sentence and one English JFK
  fixture on `tiny.en`.
- Vietnamese and Japanese require a suitable multilingual path and were not
  tested here.
- Pointer-click Start/Stop, layer-shell always-on-top presentation, X11, ARM64
  hardware, Windows runtime and multi-hour stability were not validated here.
