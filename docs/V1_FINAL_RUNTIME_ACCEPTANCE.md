# V1 final runtime acceptance

This report records whether the offline LCRT V1 path — PipeWire capture, local
Whisper, controller, and GTK captions — works end to end on Ubuntu AMD64. It
reuses evidence from PRs #19–#24 where it still applies.

## Final identity

- Base `develop`: `1b084aa50bb30f377b0f7cdec095279121bf8a6c`.
- Final code commit: `cecb0dd7d4ae0942c1f35d9423422861fb92d774`. The final PR
  head adds only this report on top of it; `git diff cecb0dd <head> -- crates`
  is empty. Every "final code" result below ran a release build of `cecb0dd`
  with a clean worktree.
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
     - The worker drains the whole backlog into the window before each pass. It
       stops only when more audio would evict audio that no pass has inferred.
     - Pending input is bounded by one rolling window of audio duration, not a
       chunk count. When the bound is reached, the session fails with
       "Whisper input backlog reached one 8s rolling window of audio;
       transcription cannot keep up with capture". The chunk-count ceiling
       rises to 2,048, so it never binds first.
     - Decode length is capped at whisper.cpp's own limit of 220 tokens per
       30 s segment, scaled to the window: 59 tokens for 8 s. English speech
       in 8 s is far below that.
     - Decoding parameters are built once per worker. whisper-rs 0.15 leaks
       the language string every time parameters are constructed, and
       `develop` built them for every pass.
2. **`--smoke-seconds` accepts up to 3,600 seconds** instead of 120, so the
   existing integrated diagnostic can run a 30-minute soak.

`crates/lcrt-stt-whisper/src/transcript.rs` is identical to `develop`. A
rolling-window heuristic that dropped suspected garbled leading words was tried
and removed, because review showed it could silently delete real speech. V1
prefers a visible repeated phrase over silent loss.

## Final-code automated verification (`cecb0dd`)

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo test --locked --workspace --all-features` | 92 passed, 0 failed |
| `RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps` | clean |
| `git diff --check` | clean |
| `git diff 1b084aa -- crates/lcrt-stt-whisper/src/transcript.rs Cargo.toml Cargo.lock` | empty |

New deterministic tests cover:

- the drain continuing past a due partial until un-inferred audio fills the
  window (it fails under `develop`'s policy);
- the backlog admitting the same 8 s of audio at 1,024- and 2,048-frame quanta;
- the window-scaled token limit;
- the 3,600-second smoke bound.

This report does not record CI or review results for the commit that contains
it; those are recorded on PR #25.

## Final-code runtime verification (`cecb0dd`)

| Check | Result |
| --- | --- |
| JFK paced replay (`lcrt-whisper-transcribe benchmark paced`) | Transcript identical to `develop`: "And so, my fellow Americans Ask not what your country can do for you. Ask what you can do for your country." 7 passes; first partial 2.38 s; completion 11.71 s |
| JFK transcribe mode (waiting producer) | Correct final; 4 passes (5 before coalescing) |
| Repetitive speech through output monitor → PipeWire → Whisper → GTK, 100 s, 2 runs | Both survived. Exit 0; 48 and 49 passes; median 1.83 s and 1.76 s; worst 2.41 s and 2.45 s (31% of the window); one Stop final each; no warnings; no leftovers |
| Same reproducer on `develop` (`1b084aa`), 2 runs | Both failed after 4 passes with the 256-chunk queue error. One run then exited with SIGSEGV (see limitations) |
| Real microphone, room noise, normal window: 150 s session, Stop, 20 s session, Stop, Close | 114 passes; median 0.63 s; worst 1.34 s; no warnings or errors. Stop 1.82 s and 1.23 s; statuses "Stopped · 7049 chunks · 60 captions" and "Stopped · 957 chunks · 8 captions". One LCRT node while active, none when idle; 12 threads when idle. Close 0.31 s, exit 0, no leftovers |
| Close during active inference, 8 launches, 2.1–7.4 s after Listening | 8/8 exit 0 in 0.31–0.36 s; no `lcrt` process or LCRT node left |
| Natural-silence finals and same-session continuation | See [integrated soak](#integrated-soak) |
| System audio: JFK played twice through the default sink into the output-monitor diagnostic, 40 s | The first caption appeared 2.44 s and 2.46 s after `pw-play` started. Exit 0; no warnings; no leftovers. Each final contained a visible repeated clause ("…can do for you. America ask Not what your country can do for you…"), the accepted exact-overlap limitation |
| Stop during startup, 3 attempts in one window | Stop arrived 260–269 ms after Start, after audio acquisition had committed. Each ended as a short cancelled session (8–9 chunks, 0 captions) with no LCRT node and a stable thread count. A full session afterwards worked; Close exit 0 |
| Offline | Every run above ran inside `bwrap --unshare-net` |

### Integrated soak

The soak ran after all builds had finished, with no concurrent Cargo work. The
path was output monitor → PipeWire → Whisper → controller → GTK caption label,
as one 1,830 s diagnostic session.

- **Input:** the JFK fixture played 139 times, with a 2 s silence after each
  play, during a 1,815 s observation window.
- **Passes:** 952 passes: 813 partials and 139 finals (138 natural-silence
  finals and the Stop final). 138 of 139 finals were followed by further
  partials in the same session. Median pass 0.64 s; slowest 4.37 s, a fallback
  final.
- **Captions:** the label changed 947 times. The longest interval without a
  change was 4.4 s. The pipeline reported 49,763 chunks and 952 caption
  updates.
- **Memory:** 61 RSS samples, one every 30 s. RSS was 237,624 KiB before the
  first inference, then 349,212 KiB (first steady sample) → 351,668 KiB
  (last). The growth was step-shaped:
  - +336 KiB by minute 14;
  - flat from minute 14 to 22, about 250 passes;
  - one +2,056 KiB step at 23.7–24.1 min, exactly when the soak's four
    slowest passes ran (3.9–4.4 s, two of them fallback finals);
  - +64 KiB over the last 4 min.

  That pattern fits whisper.cpp retaining its peak decode working set after a
  fallback burst, not a per-pass leak.
- **Threads and nodes:** 17–20 threads; exactly one LCRT PipeWire node
  throughout.
- **Health:** no warnings or errors in a 158 KB log.
- **Shutdown:** process exit 0, 8.3 s after the observation window (the
  remaining smoke time plus the Stop flush); no `lcrt` process or LCRT node
  afterwards.

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
- **Earlier same-window lifecycle**: four sessions and six early Stops in one
  window; no stale transcript between sessions. The controller, startup gate,
  and UI code are unchanged by this PR.
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
  error rather than dropping audio. On this CPU the worst observed pass was
  2.45 s on pathological input. Much slower CPUs, or heavy contention, can
  still reach the bound.
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
