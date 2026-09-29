# V1 final runtime acceptance

This report records whether the offline LCRT V1 application works end to end as
a native Ubuntu live-caption application with the current production local
Whisper path. It reuses the evidence from PRs #19–#24 and adds only the runtime
evidence that was still missing.

## Test identity

- Base: `develop` at `1b084aa50bb30f377b0f7cdec095279121bf8a6c` plus the
  changes in this pull request, built with Cargo `release`.
- Two builds were runtime-measured. **Build A** disabled the Whisper fallback
  for every pass and contained the smoke bound change. **Build B** is head
  `4335dad`, which added the first transcript-overlap fix.
- The final head adds four review corrections. Three are unit-tested:
  - An exact leading overlap takes precedence.
  - Punctuation-only tokens cannot anchor a skip.
  - Skipped words are retained unless enough words of the previous partial
    precede the anchor.
- The fourth correction, final passes keeping the fallback, is not directly
  tested. No test covers that branch, and none of the final-source runtime
  final passes fell back. It is verified only by code inspection: final
  passes simply keep whisper.cpp's default, which is the pre-PR behaviour
  exercised by PRs #19–#24.
- Runtime checks on the final source:
  - A deterministic paced replay of the fixture (see
    [fixes](#fixes-made-by-this-pull-request)).
  - Stop re-measured, because the final-pass fallback runs on Stop. This used
    head `9a6414f`, whose backend and Stop code are identical to the final
    source; only transcript assembly changed afterwards. It ran three
    15-second noisy-microphone cycles in one offline window. Stop took 1.37–1.51 s each time, the final passes took
    0.58 s (none fell back), and the window closed with exit 0 and no
    leftovers.
- All other system-audio, soak, microphone, and lifecycle results below come
  from Builds A and B, not the exact final source. The final-pass fallback can
  still take several decodes on a rejected final. Its worst-case Stop latency
  on the final source was not observed, and it stays bounded by the 30-second
  finish timeout.
- OS: Ubuntu 26.04 LTS, Linux 7.0.0-34-generic, x86_64, GNOME Shell 50.1
  on Wayland, PipeWire 1.6.2.
- CPU: 12th Gen Intel Core i5-12500H, 16 logical CPUs. Rust 1.98.0.
- Model: `ggml-tiny.en.bin`, 77,704,715 bytes, SHA-256
  `921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f`.
  This model is English-only.
- System-audio fixture: whisper.cpp v1.7.6 `samples/jfk.wav`, SHA-256
  `59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`.
  It was fetched to a temporary directory with the checksum pinned and was not
  committed.
- Audio devices: the built-in analog stereo microphone and the built-in analog
  stereo output monitor. Source discovery listed both.
- GNOME did not advertise layer shell, so every window used the
  standard-window fallback.

### How the UI was driven

The window's own controls were operated through the AT-SPI accessibility
interface. This invokes the same GTK button handlers as a click, but it is not
pointer input. GNOME Wayland rejects synthesized input and the source dropdown
exposes no usable accessibility selection. Because of that, the system-audio
runs used the existing `--smoke-source` diagnostic, which sends the same
`Start`/`Stop` controller actions from a separate thread. Every
application run below ran inside `bwrap --unshare-net` (see
[offline operation](#offline-operation)).

## Results

| Acceptance item | Result |
| --- | --- |
| Audible human microphone transcription | PROVEN, with limited accuracy |
| Real system-audio transcription | PROVEN |
| Same-window Start → Stop → Start | PROVEN |
| Stop while startup is in progress | LIMITED EVIDENCE (one branch runtime-tested, the other unit-tested) |
| Window close / termination | PROVEN (accessibility and UI close) |
| Offline operation with a local model | PROVEN for the exercised runs |
| Integrated PipeWire → Whisper → GTK soak | PROVEN for 30 minutes (see [soak](#integrated-soak)) |
| Speech-to-visible-caption latency | LIMITED EVIDENCE (practical method, stated uncertainty) |
| No stuck process or PipeWire node | PROVEN |
| Actionable user-visible failures | PROVEN for the exercised failure (see [fixes](#fixes-made-by-this-pull-request)) |
| Vietnamese and Japanese speech | NOT TESTED (English-only model) |

### Microphone live speech (Build A)

The owner started a session in the prepared window, read the reference
sentence, stopped the session, and closed the window. A watcher timestamped
every caption, status, and button change through AT-SPI. A temporary mic
recording was used to locate speech onset and then deleted; nothing recorded
was committed.

- Reference: "Real-time captions help me follow every conversation."
- Best recognized renditions: "Real-time captions, real-time captions, help me
  follow every conversation." and "Route time caption, have me follow every
  conversation."
- Usability: marginal. The content words after "real-time" were recognized
  consistently. "Real-time" was often heard as "Route time" or "Root time".
- Beginning of speech: not dropped, but the first words were often misheard.
- Visible updates while speaking: yes. Partial captions changed about every
  1.5 seconds during speech.
- Stop: the button returned to Start 1.0 second after Stop. The status read
  `Stopped · 1657 chunks · 21 captions`.
- Noise: the microphone's median RMS was about 0.065, roughly eight times the
  fixed 0.008 speech threshold. Utterances therefore never reached final
  silence, and the rolling window kept re-decoding the same speech. This
  exposed the duplicated-caption defect fixed below. Some repetition remains
  (see [known limitations](#known-limitations)).

### System audio (Build B, `4335dad`)

The diagnostic ran for 40 seconds on the output monitor while `pw-play` played
the JFK fixture twice through the default sink.

- The first visible caption appeared 2.69 s and 2.65 s after `pw-play` started.
- Final captions: "And so my fellow Americans Ask Not what your country can do
  for you. Ask what you can do for your country." (twice, with one punctuation
  difference).
- 1,348 chunks and 14 caption updates. The window stayed responsive, the run
  exited with status 0, and afterwards there were no `lcrt` processes and no
  LCRT PipeWire nodes.

### Same-window Start → Stop → Start (Build A)

All cycles ran in one retained normal-mode window on the microphone:

| Cycle | Start → Listening | LCRT nodes while active | Stop duration | Session status | Nodes when idle |
| --- | --- | --- | --- | --- | --- |
| 1 | 0.66 s | 1 | 2.20 s | 586 chunks · 6 captions | 0 |
| 2 | 0.61 s | 1 | 2.03 s | 584 chunks · 6 captions | 0 |
| 3 (after cancels) | 0.55 s | 1 | 1.51 s | 257 chunks · 2 captions | 0 |
| 4 (after fast cancels) | — | 1 | — | 248 chunks · 4 captions | 0 |

- Each Start acquired new audio.
- There was never more than one LCRT node.
- Session 2's caption did not begin with session 1's caption.
- The idle thread count returned to 12 after every session.
- The log contained no warnings or errors.

### Stop while startup is in progress (Build A)

- Stop was pressed six times in the same window: 486–667 ms after Start, and
  then 264 ms after Start with a cached button reference.
- In all six, audio acquisition had already committed. Each completed as a
  short cancelled session (5–17 chunks, 0 captions), with 0 nodes and 12
  threads afterwards.
- The window remained usable for full sessions afterwards.
- Warm model load plus acquisition finishes faster than the UI can deliver a
  Stop, so the branch where cancellation wins before acquisition was not
  reached at runtime. It remains covered by the deterministic unit test
  `cancellation_winning_at_the_audio_boundary_does_not_start_audio` (PR #21).
  No sleeps or production hooks were added to force the race.

### Window close and termination

- **Accessibility-invoked Close** (header-bar button, Build A): the process
  exited 0.36–0.50 s after close, in several runs, both idle and after
  sessions.
- **Close during an active microphone session** (Build A): the window was
  closed outside the automation while captioning. The process exited within
  20 ms of the window disappearing, with status 0.
- **Owner checkpoint close** (Build A): the window was closed from the UI,
  outside the automation, 4.3 s after Stop. The process exited within 20 ms
  with status 0. The owner was asked to use the title-bar X, but the close
  method was not independently observed.
- Diagnostic runs quit themselves with status 0.
- After every exit there were no `lcrt` processes and no nodes with
  `application.name = "LCRT"`.

### Offline operation

- Every application run in this report ran inside
  `bwrap --bind / / --dev-bind /dev /dev --proc /proc --unshare-net`.
- Inside that namespace only the loopback interface exists, and an HTTPS
  request fails name resolution.
- Wayland, D-Bus, PipeWire, and the accessibility bus all use filesystem-path
  Unix sockets, so the full GUI and audio paths worked.
- The workstation network was not changed and root access was not used.
- This proves the exercised runs need no network. It does not claim physical
  disconnection.
- Source inspection from PR #22 still applies: the runtime crates contain no
  network client and no implicit model download.

### Integrated soak

Build B ran after all builds had finished, with no concurrent Cargo work. The
session went through the full integrated path (output monitor → PipeWire
capture → Whisper → controller → GTK state bridge → caption label) as one
1,830-second diagnostic session.

- **Input:** the JFK fixture was played 139 times through the default sink,
  with a 2-second gap after each play, during the 1,815-second observation
  window.
- **Captions:** the label changed 966 times. The longest interval without a
  change was 3.0 seconds. The last caption was the complete quotation. The
  pipeline reported 49,758 chunks and 973 caption updates.
- **Memory:** 61 samples, one every 30 seconds. RSS was 237,792 KiB before the
  first inference, 348,924 KiB at the first steady sample, and 348,972 KiB at
  both the last and the maximum sample. That is a 48 KiB increase over about
  29.5 minutes.
- **Threads and nodes:** the thread count stayed between 17 and 21, and there
  was exactly one LCRT PipeWire node throughout.
- **Health:** the status alternated only between `Listening…` and `Final`.
  The log was 954 bytes and contained no warnings or errors.
- **Shutdown:** Stop and the final flush completed, the process exited with
  status 0 about 4 seconds after the observation window, and afterwards there
  were no `lcrt` processes and no LCRT nodes.

**Not covered by the soak:** the microphone source, pointer input, and
multi-hour stability.

### Speech-to-visible-caption latency

The visible caption was measured as the moment the GTK caption label's text
changed, observed through AT-SPI with 20 ms polling. Pixels follow on the next
frame. The two starting points differ:

- **System audio:** measured from `pw-play` start. The fixture has no leading
  silence of 100 ms or more at -40 dB. The first caption took **2.65–2.69 s**,
  which matches the first-partial behaviour: 750 ms minimum speech, the
  1.5 s step, and about 0.8 s of inference.
- **Human microphone:** speech onset was taken from 50 ms RMS bins of a
  temporary parallel recording. The mic is noisy, so onset carries about
  ±0.3 s uncertainty.
  - The first caption text changed 0.8 s after onset.
  - The first recognizable words appeared about 2.3 s after onset.
  - The complete sentence was visible about 1.2 s after the speech energy
    ended.
  - These figures come from one owner session and are not a distribution.

### Memory

- In the normal window, RSS was 129 MB at idle before any session and 215 MB
  after four sessions and six cancelled startups.
- The thread count returned to 12 whenever the window was idle.
- Soak memory is reported under [integrated soak](#integrated-soak).

## Fixes made by this pull request

1. **Live sessions failed on noisy microphone input.**
   - Symptom: with Build A's predecessor (unmodified `develop`), room noise on
     the real microphone ended the session after 24–28 seconds with
     "Whisper input queue reached its 256-chunk bound; transcription cannot
     keep up with capture". This was reproduced three times.
   - Cause: normal passes took about 0.8 s, but whisper.cpp's default
     temperature fallback re-decoded repetitive hallucinations at up to five
     higher temperatures, producing 2.0–5.0 s passes.
   - Fix: rolling partial passes now disable the fallback, because the next
     rolling-window pass re-decodes the same audio 1.5 s later anyway. Final
     passes, triggered by silence or Stop, keep whisper.cpp's fallback,
     because nothing re-decodes their audio.
   - Result (Build A, fallback disabled for all passes): the same unattended
     condition then ran for 60 seconds with 41 passes (median 0.95 s, maximum
     2.3 s) and stopped cleanly. On this noisy microphone every pass before
     Stop was a partial. The fixture
     transcript and pass count were unchanged.
2. **Rolled windows duplicated captions.**
   - Cause: the rolling-window overlap required the previous hypothesis's last
     words to match the new hypothesis's first words exactly. Whisper often
     re-recognizes the first words of a window that starts mid-word, for
     example "Route time" becoming "Roo-time". The whole previous hypothesis
     was then committed again.
   - Fix: an exact overlap at the start of the new hypothesis still always
     wins. Only when there is none may up to two garbled leading words be
     dropped, only when at least three further non-punctuation words anchor
     the overlap, and only when at least as many words of the previous partial
     precede the anchor. Committed text does not count, because it may belong
     to audio that the window no longer covers. This keeps a legitimately repeated phrase such as "go home … go
     home". A genuine leading word such as the "but" in "but I want to go
     home" is retained, and noise markers such as `♪` cannot delete words.
   - Evidence: five regression tests. On head `4335dad`, a paced replay of
     the owner's recorded segment through the diagnostic removed three
     duplicated renditions, and the fixture output was unchanged. The
     recording was then deleted. The later corrections affect only
     hypotheses that have an exact leading overlap or punctuation-only
     anchors, or too few previous-partial words before the anchor. The observed
     duplicates had none of these. On the final source, a paced fixture
     replay produced the identical transcript in 7 passes.
3. **Smoke diagnostic too short for a soak.** `--smoke-seconds` now accepts up
   to 3,600 seconds instead of 120, so the diagnostic can run the integrated
   soak. It remains bounded.

## Known limitations

- The speech gate is a fixed RMS threshold. On a microphone whose noise floor
  exceeds it, utterances finalize only on Stop, and Whisper keeps re-decoding
  and hallucinating on non-speech ("you", "♪", repeated phrases). An adaptive
  noise floor or VAD is future work.
- When a window's leading words were already committed by an earlier skip, a
  later pass can keep a short echo such as "Route time Root time caption…".
  It is at most two words and does not grow; this was chosen over risking
  the deletion of genuine words.
- Rolling-window overlap is still exact after the leading words. A word that
  Whisper changes mid-overlap (for example "help" and "have") can still repeat
  a phrase in continuous noisy input.
- Disabling fallback bounds a partial pass to one decode. A final pass keeps
  the pre-existing fallback and can still take several decodes. A single
  repetitive decode
  can still take about 2.3 s, longer than the 1.5 s step. Isolated passes like
  this are absorbed by the queue. Sustained back-to-back passes of that length
  could still fill it; this was not observed.
- The accuracy evidence is one English speaker and one sentence on the
  English-only tiny model. Vietnamese and Japanese require a multilingual
  model and remain NOT TESTED.
- Always-on-top layer-shell presentation, X11, visual appearance, Ubuntu
  ARM64 hardware, and Windows were not tested here.
