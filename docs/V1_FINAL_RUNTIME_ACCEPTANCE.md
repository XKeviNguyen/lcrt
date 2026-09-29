# V1 final runtime acceptance

This report records whether the offline LCRT V1 path — PipeWire capture, local
Whisper, controller, and GTK captions — works end to end on Ubuntu AMD64. It
reuses evidence from PRs #19–#24 where it still applies.

## Final identity

- Base `develop`: `1b084aa50bb30f377b0f7cdec095279121bf8a6c`.
- Final code commit: `294af25cfbf1b0ac6ccb9815be1c25049b5d2009`. The final PR
  head adds only this report on top of it; `git diff 294af25 <head> -- crates`
  is empty. Every result in the final-code runtime table ran a release build
  whose `crates/` tree matched `294af25`. The full 30-minute soak ran on
  `64f2d55`; the soak section explains why it applies unchanged.
- OS: Ubuntu 26.04 LTS, Linux 7.0.0-34-generic, x86_64, GNOME Shell 50.1 on
  Wayland, PipeWire 1.6.2. GNOME did not advertise layer shell, so windows used
  the standard-window fallback.
- CPU: 12th Gen Intel Core i5-12500H, 16 logical CPUs. Rust 1.98.0; whisper-rs
  pinned at 0.15.1 with four inference threads.
- Model: `ggml-tiny.en.bin`, 77,704,715 bytes, SHA-256
  `921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f`
  (English-only).
- Speech fixture: whisper.cpp v1.7.6 `samples/jfk.wav`, SHA-256
  `59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`.
- Repetitive-speech fixture: the fixture's 3.0–3.7 s slice looped 129 times
  (90.3 s), generated locally with `ffmpeg` and not committed.
- Every application run used `bwrap --unshare-net` (loopback only; an HTTPS
  request fails name resolution). Wayland, D-Bus, PipeWire, and the
  accessibility bus are filesystem-path sockets, so the GUI and audio paths
  still worked. No root access or workstation network change was used.
- The window's controls were driven through AT-SPI accessibility, which invokes
  the GTK button handlers. It is not pointer input. System-audio runs used the
  existing `--smoke-source` diagnostic, because the source dropdown exposes no
  usable accessibility selection.

## Production changes in PR #25

1. **Live Whisper input stays bounded without disabling fallback**
   (`crates/lcrt-stt-whisper`).
   - Root cause, reproduced on `develop`:
     - Once the 8 s window was full, the worker consumed only one 1.5 s partial
       step of queued audio per pass, so any pass slower than the step carried
       backlog forward.
     - The queue was bounded at 256 *chunks*. PipeWire delivered 1,024- or
       2,048-frame chunks, and switched between them mid-stream, so that bound
       meant 5.5 s or 10.9 s of audio.
     - On repetitive speech, each of whisper.cpp's fallback decodes ran to 220
       tokens, so one pass could outlast the whole window.
   - Fix:
     - whisper.cpp's default temperature fallback stays on every pass, exactly
       as on `develop`. No hypothesis is decoded with fallback disabled.
     - The worker drains the whole backlog into the window before each pass.
       Before appending a chunk, it checks whether the chunk would evict audio
       no pass has inferred. If the open utterance has passed the
       minimum-speech gate, that chunk is held back until after a pass, and the
       pass is forced if none is due. A pass that the held-back chunk itself
       makes due, such as a natural final, runs before any later audio.
     - Once an utterance passes the gate, none of its audio still in the
       window is evicted uninferred. This includes sparse speech that never
       meets the step gate.
     - Below the gate, the window rolls as on `develop`: that audio is not
       eligible for inference, so no pass is manufactured for noise. Speech
       shorter than `minimum_speech` followed by final silence is still
       discarded.
     - Audio duration is the only input bound. `max_input_backlog` defaults to
       one 8 s window and may not exceed it. The command channel has no
       chunk-count capacity that small PipeWire quanta could exhaust first. At
       the bound, a live session fails with "Whisper input backlog reached 8s
       of audio; transcription cannot keep up with capture". A waiting
       producer waits on the same reservation.
     - Decode length is capped at whisper.cpp's own limit of 220 tokens per
       30 s segment, scaled to the window: 59 tokens for 8 s. English speech
       in 8 s is far below that. The cap is per segment. Passes decode
       without timestamps, so whisper.cpp ends each segment by advancing 30 s,
       and an 8 s window is one segment per fallback attempt. A pass is
       therefore at most six 59-token decodes.
     - Decoding parameters are built once per worker. whisper-rs 0.15 leaks
       the language string every time parameters are constructed, and
       `develop` built them for every pass.
2. **`--smoke-seconds` accepts up to 3,600 seconds** instead of 120, so the
   existing integrated diagnostic can run a 30-minute soak.

`crates/lcrt-stt-whisper/src/transcript.rs` is identical to `develop`. A
rolling-window heuristic that dropped suspected garbled leading words was tried
and removed, because review showed it could silently delete real speech. V1
prefers a visible repeated phrase over silent loss.

## Final-code automated verification (`294af25`)

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo test --locked --workspace --all-features` | 96 passed, 0 failed |
| `RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps` | clean |
| `git diff --check` | clean |
| `git diff 1b084aa -- crates/lcrt-stt-whisper/src/transcript.rs Cargo.toml Cargo.lock` | empty |

New deterministic tests cover the real drain function, driven by a real
channel and real chunks with no model:

- a chunk that would evict unseen audio is deferred before it is appended;
- the first utterance stops at a full window without rolling it;
- after a pass, the drain continues past a due partial and releases each taken
  chunk's backlog reservation (under `develop`'s policy it stopped after one
  step);
- sparse speech that passes the minimum-speech gate forces a pass instead of
  rolling unseen audio away;
- short spikes below the gate never force a pass, and the window rolls as
  before;
- a final is reported only while its silence persists, which is why a final
  made due by a held-back chunk runs before later audio;
- a finish request during a drain is reported.

Further tests cover:

- the backlog admitting the same 8 s at 1,024- and 2,048-frame quanta;
- the backlog-bound validation;
- the window-scaled token limit;
- the 3,600-second smoke bound.

This report does not record CI or review results for the commit that contains
it; those are recorded on PR #25.

## Final-code runtime verification (`294af25`)

| Check | Result |
| --- | --- |
| JFK paced replay (`lcrt-whisper-transcribe benchmark paced`) | Transcript identical to `develop`: "And so, my fellow Americans Ask not what your country can do for you. Ask what you can do for your country." 7 passes; first partial 3.14 s; completion 12.61 s |
| JFK transcribe mode (waiting producer, 2 s backlog) | 5 passes. The final contained the accepted exact-overlap repetition ("…can do for you. America. Ask not what your country can do for you…"). Earlier runs of this mode on intermediate code were clean; the outcome depends on producer timing |
| Repetitive speech through output monitor → PipeWire → Whisper → GTK, 100 s, 2 runs | Both survived. Exit 0; 44 and 47 passes; median 1.86 s and 1.87 s; worst 3.58 s and 2.55 s; one Stop final each; no warnings; no leftovers |
| Same reproducer on `develop` (`1b084aa`), 2 runs | Both failed after 4 passes with the 256-chunk queue error. One run then exited with SIGSEGV (see limitations) |
| Real microphone, room noise, normal window: 150 s session, Stop, 20 s session, Stop, Close | 112 passes; median 0.95 s; worst 2.45 s; no warnings or errors. Stop 1.86 s and 1.36 s; statuses "Stopped · 7046 chunks · 49 captions" and "Stopped · 966 chunks · 11 captions". One LCRT node while active, none when idle; 12 threads when idle. Close 0.30 s, exit 0, no leftovers |
| Close during active inference, 8 launches, 2.1–7.4 s after Listening | 8/8 exit 0 in 0.32–0.34 s; no `lcrt` process or LCRT node left |
| System audio: JFK played twice through the default sink into the output-monitor diagnostic, 40 s | The first caption appeared 2.50 s and 2.44 s after `pw-play` started. Exit 0; no warnings; no leftovers. Finals can contain the accepted exact-overlap repetition |
| Stop during startup, 3 attempts in one window | Stop arrived 263–266 ms after Start, after audio acquisition had committed. Each ended as a short cancelled session (6–9 chunks, 0 captions) with no LCRT node. A full session afterwards worked; Close exit 0 |
| Natural-silence finals and same-session continuation | See [integrated soak](#integrated-soak) |
| Offline | Every run above ran inside `bwrap --unshare-net` |

### Integrated soak

The full 30-minute soak ran on code commit `64f2d55`. The only code change
from `64f2d55` to the final `294af25` is one extra condition on the deferral
branch in `drain_backlog`: `meets_minimum_speech()`. That branch runs only
when appending a chunk would evict unseen audio, which needs a pass to leave
a whole 8 s window uninferred. The soak's slowest pass was 1.71 s, so the
branch never ran, and both builds executed identical code in this scenario.
A 16.9-minute continuation on `294af25` itself is reported below as
corroboration.

**Full soak (`64f2d55`, system audio).** The run started after all builds had
finished, with no concurrent Cargo work. The path was output monitor →
PipeWire → Whisper → controller → GTK caption label, as one 1,830 s
diagnostic session.

- **Input:** the JFK fixture played 139 times, with a 2 s silence after each
  play, during a 1,815 s observation window.
- **Passes:** 973 passes: 834 partials and 139 finals (138 natural-silence
  finals and the Stop final). All 138 natural finals were followed by further
  partials in the same session. Median pass 0.90 s; slowest 1.71 s, a final.
- **Captions:** the label changed 967 times. The longest interval without a
  change was 3.4 s. The pipeline reported 50,181 chunks and 973 caption
  updates.
- **Memory:** 61 RSS samples, one every 30 s. RSS was 238,460 KiB before the
  first inference, then 349,760 KiB (first steady sample) → 350,104 KiB
  (last). That is +344 KiB in total, flat from minute 16 to 30.
- **Threads and nodes:** 17–20 threads; exactly one LCRT PipeWire node
  throughout.
- **Health:** no warnings or errors in a 162 KB log.
- **Shutdown:** process exit 0, 7.4 s after the observation window (the
  remaining smoke time plus the Stop flush); no `lcrt` process or LCRT node
  afterwards.

**Continuation on `294af25` (system audio), 16.9 minutes.**

- **Passes:** 540 passes: 463 partials and 77 natural-silence finals, all 77
  followed by further partials. Median pass 0.72 s; slowest 1.24 s.
- **Memory:** RSS 348,944 KiB → 349,308 KiB (+364 KiB), flat from minute 6.
- **Health:** exactly one LCRT node; no warnings or errors.
- **How it ended:** the window received a normal close at 16.9 minutes. The
  log stops mid-session with no error, and `lcrt` exited with status 1. That
  status is the diagnostic's defined result for a window close before its
  scheduled Stop, not a signal or crash status. The close came from outside
  the test scripts, during an owner interruption of the session; who closed
  the window was not observed. No `lcrt` process or LCRT node remained.
  This run is not counted as a complete soak.

## Reused evidence

- **Human microphone checkpoint** (an intermediate build that disabled fallback
  on partials):
  - The owner read "Real-time captions help me follow every conversation." in
    the window.
  - Partial text updated about every 1.5 s while speaking.
  - Best rendition: "Real-time captions, real-time captions, help me follow
    every conversation." "Real-time" was often heard as "Route time" or "Root
    time".
  - Stop returned to idle in 1.0 s.
  - Latency: the first caption text changed 0.8 s after speech onset, and the
    complete sentence was visible about 1.2 s after speech ended. Onset carries
    ±0.3 s uncertainty (noisy microphone), and this is one session.
  - Applicability: the capture, controller, and UI path is unchanged. The final
    decoding restores whisper.cpp's fallback on partials, and the 59-token cap
    is far above an 8 s spoken sentence, so exact recognition could differ
    wherever fallback triggers. That was not re-tested with a human.
- **Earlier same-window lifecycle** (supplements the final-code two-session and
  early-Stop runs above): four sessions and six early Stops in one window; no
  stale transcript between sessions. The controller, startup gate, and UI code
  are unchanged by this PR.
- **Cancel-before-acquisition** remains covered only by the PR #21 unit test
  `cancellation_winning_at_the_audio_boundary_does_not_start_audio`. Warm model
  loading finishes before an accessibility Stop can arrive.

## Known limitations

- The speech gate is a fixed RMS threshold. On a microphone whose noise floor
  exceeds it, utterances finalize only on Stop, and Whisper hallucinates on
  non-speech. Adaptive VAD or noise-floor handling is future work.
- The exact rolling-window overlap can visibly repeat a phrase when Whisper
  changes boundary words. This is preferred over risking silent word loss.
- Live captioning requires each pass, partial or natural final, to finish
  within the 8 s window. If a pass does not, the session fails with the backlog
  error rather than dropping audio. Across all runs on the final code
  `294af25`, the worst observed pass was 3.58 s (repetitive speech), 45% of the
  window; the real-microphone worst was 2.45 s and the soak continuation's
  1.24 s. The full `64f2d55` soak's worst was 1.71 s.
  Much slower CPUs, or heavy contention, can still reach the bound.
- Only a Stop-triggered final is bounded by the 30 s finish timeout. A
  natural-silence final has no timeout of its own; it is bounded only by the
  backlog limit above.
- A pass that reaches the 59-token cap is truncated. That requires more than
  7.3 tokens per second of audio, the same density whisper.cpp allows for a
  full 30 s segment.
- **SIGSEGV after a failed session (pre-existing):** a session that fails while
  whisper.cpp is still inside a long pass leaves its worker running. If the
  process exits before that pass ends, native teardown can crash. This was
  reproduced on `develop` and was not observed on normal close (8/8 during
  active inference).
- whisper-rs 0.15.1 `set_abort_callback_safe` reads its user data with the
  wrong type for a plain closure; LCRT does not use it.
- Accuracy evidence is one English speaker, one sentence, and the English JFK
  fixture on `tiny.en`. Vietnamese and Japanese need a multilingual model and
  were not tested.
- Pointer-click input, layer-shell always-on-top presentation, X11, ARM64
  hardware, Windows, and runs longer than 30 minutes were not tested.
