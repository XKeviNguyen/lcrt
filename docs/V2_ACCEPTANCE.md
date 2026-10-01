# V2 acceptance

This report records what the V2 release candidate (`feature/v2-final`)
delivers and the evidence behind each claim. It also records what has not
been verified.

V2 adds four things on top of the V1 offline path:

- online captions;
- real-time translation;
- vocabulary explanations;
- credential, preference and appearance management, plus an Ubuntu package.

**Status:** every V2 feature has been verified at runtime on Ubuntu AMD64,
including the cloud features against OpenAI with the owner's API key. See
[Live verification with OpenAI](#live-verification-with-openai). What remains
unverified is listed under [Known limitations](#known-limitations).

## Identity and environment

- Base: `develop` `a3229e649b14d98f4844f6f0af32e34f2cded771`.
- The runtime evidence below ran on commits between `f90798d` and the final
  code commit. Each row names its build when it matters.
- OS: Ubuntu 26.04 LTS, Linux 7.0.0-34-generic, x86_64.
- Desktop: GNOME Shell 50.1 on Wayland, PipeWire 1.6.2, GTK 4.22.4,
  libadwaita 1.9.1, gtk4-layer-shell 1.3.0. GNOME does not advertise layer
  shell, so the caption window used the standard-window fallback.
- CPU: 12th Gen Intel Core i5-12500H, 16 logical CPUs. Rust 1.98.0.
- Model: `ggml-tiny.en.bin`, SHA-256
  `921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f`.
- Isolation:
  - Every application run used an isolated `XDG_CONFIG_HOME`.
  - Runs that did not need a real display ran inside `bwrap --unshare-net`,
    which allows loopback only.
  - No root access, global network change or GitHub secret was used.
- Controls were driven through AT-SPI accessibility, which invokes the GTK
  handlers but is not pointer input.
- For the offline and no-key checks, the workstation's screen was locked
  with the display powered off (`PowerSaveMode` 3). The compositor then
  sends no frame callbacks, which limits some UI checks. The affected rows
  say so. The live OpenAI checks ran later with the display on.

## Evidence by category

| Area | Implemented | Unit / mock tested | Runtime tested here | Not verified |
| --- | --- | --- | --- | --- |
| Offline captions (Whisper) | yes (V1, preserved) | yes | system audio, 18.6 min of natural speech; microphone smoke | — |
| Online captions (`gpt-live-transcribe`) | yes | yes: protocol, out-of-order completions, reconnect, rejected key | live with OpenAI: EN, JA, VI and Auto; missing key; network failure and mid-session interruption | — |
| Translation (`gpt-realtime-translate`) | yes | yes: both lanes, show-original, closing | live with OpenAI: EN→JA, JA→EN, VI→EN, original shown and hidden | — |
| API key storage | yes | yes | real Secret Service: save, reload after restart, clear | — |
| Test connection | yes | yes | verified against OpenAI (HTTP 200); entered, saved, malformed and missing keys; unreachable service | — |
| Vocabulary popover | yes | yes: request bounds, parsing, cache | live explanations for English and Japanese; missing-key guidance | pointer selection (selections were made through accessibility) |
| Appearance and preferences | yes | yes: normalization, persistence | persisted values, reset, startup size, live resize | — |
| Packaging (`.deb` 2.0.0) | yes | metadata validated | reproducible build, `apt-get -s`, packaged GUI launch; installed by the owner with `apt` | — |
| CI | workflow updated | — | pending for the final head | — |

"Hardware tested" applies only to this Ubuntu AMD64 laptop. ARM64 and Windows
remain compile-portable goals and were not run.

## Automated verification

On the final code commit:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo test --locked --workspace --all-features` | 198 passed, 0 failed |
| `RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps` | clean |
| `git diff --check` | clean |
| `desktop-file-validate`, `appstreamcli validate --no-net` | valid; one pedantic note about the uppercase app ID, which is kept because the keyring entry is named after it |

## Runtime verification

### Offline captions, natural speech through system audio

Source material (downloaded for the test, not committed): three public-domain
LibriVox recordings from *Short Nonfiction Collection, Vol. 100*
(archive.org item `snf100_2310_librivox`, 2023). Each is by a different
volunteer reader.

| Track | Length | SHA-256 of the downloaded MP3 |
| --- | --- | --- |
| 03 Forgiveness: Mark Twain on the Biblical Patriarchs Joseph and Esau | 7:16 | `6d051187ee080fe51f443639343806beea48e484e984bd1dca2887311eaaed0a` |
| 14 Remembering the November 1913 "White Hurricane" (NOAA) | 7:24 | `1f30695e1c35b5ecbed284c8f33ae08887c7666dfa655a0d0dd88e8b5602320d` |
| 09 Know the $100 Note (US government text) | 3:52 | `aa8ed2ae464420cca10bbeb6de2495b44810282bf7d9b5c4571939f1033cb102` |

The tracks were joined with 2 s of silence between them into one 1,115.7 s
WAV (SHA-256 `d6433032…e838f07`). The file was played with `pw-play` to the
default sink. LCRT captured it from the **System audio** source in the normal
window, in Offline Captions mode.

Results:

- **Time to first caption:** 4.03 s after playback started. This includes
  the recording's lead-in before the first word.
- **Caption updates:** 537 changes across 44 captions.
  - Every gap longer than 4.5 s fell on a track boundary or a reader's pause
    (the longest was 9.4 s, between tracks). No stall occurred while speech
    was playing.
  - The 95th-percentile gap between updates was 2.8 s.
- **Stop:** 0.33 s after playback ended, keeping the final caption.
- **No warnings or errors** in the application log.
- **Memory:** RSS stayed between 352.6 and 353.6 MB from minute 2 to the end
  (+1.0 MB over 16.6 minutes). After Stop it fell to 156 MB.
- **CPU:** 2.99 cores on average. Whisper re-decodes its rolling 8 s window,
  as in V1.
- **Live versus file input:** the same model run over the same audio from a
  file (`lcrt-whisper-transcribe`) serves as a reference.

  | Transcript | Words | Words in repeated 6-grams | Unique-word estimate |
  | --- | --- | --- | --- |
  | Batch, from the file | 4,954 | 40.7% | 2,939 |
  | Live, through system audio | 5,551 | 45.3% | 3,037 |

  Measured on unique words, the live system-audio path captured the same
  amount of speech as file input.
- **Repetition:** 41–45% of offline caption words fall in phrases repeated
  within the previous 60 words. This is the rolling-window overlap
  repetition that V1 accepted in preference to silent word loss (PR #25). The
  same file transcribed by the `develop` build shows the same pattern (4,830
  words, 38.3% in repeated 6-grams, 2,979 unique), so this is pre-existing V1
  behavior, not a V2 regression. On continuous natural
  speech it is much more frequent than the "occasional repeated phrase" that
  V1 documented. It is the most significant known quality issue in Offline
  Captions; see [Known limitations](#known-limitations).

### Short regressions

- **JFK through system audio** (`f90798d`): first caption 3.44 s after
  playback started; Stop kept the caption.
- **Microphone smoke:** a 10 s session on the built-in microphone with nobody
  speaking. It streamed 451 chunks with no warnings. Stop took 2.87 s,
  including the final inference, while a build competed for the CPU.

### Online modes without cloud access

- **No key:** Start in Online Captions shows "Online mode needs an OpenAI API
  key." with an **Open Settings** button, which opens Preferences. No session
  starts and nothing is sent.
- **Network failure** (fake key, `bwrap --unshare-net`):
  - The status went Connecting… (0.4 s) → Reconnecting… (0.9 s) → Error at
    4.0 s. That is three bounded retries at 0.5 s, 1 s and 2 s.
  - Start became available again; capture stopped.
  - The fake key does not appear in the log.
  - A regression found here was fixed: the message had gained a
    "transcription failed:" prefix.
  - The error banner slides in with an animation, which cannot run while the
    display is off. Its text was therefore checked through the controller log
    and the unit test, not on screen.
- **Offline mode makes no online requests:** all offline runs ran without
  network access, and nothing in the offline backend opens a connection.

### Credentials (real Secret Service)

These checks used fake keys only.

1. **Save securely:** stored the key in the login keyring. After a restart,
   Settings showed "✓ Saved securely".
2. **Test connection**, offline:
   - A saved key and a freshly typed key each reported "Can't reach the online
     service. Check your connection."
   - A malformed key reported a paste hint.
   - With no key, it reported "Enter an API key to test."
3. **Clear:** removed the entry; `secret-tool search service
   io.github.hoangnguyen7474.Lcrt` returned nothing.
4. **Where the key never appears:**
   - `preferences.json` (mode 0600), which also has an automated test;
   - the application log;
   - command lines, URLs and environment dumps.

### Vocabulary

Selecting caption text opened the popover. With no key, it showed
"Vocabulary explanations need an OpenAI API key." with **Open Settings**, and
made no request.

On Wayland, GTK's claim to the PRIMARY selection is sometimes refused,
because an AT-SPI selection carries no input serial. The selection then
collapses before the lookup can start. This affects only the accessibility
harness, so the selection checks ran on the X11 backend (XWayland). Live
lookups are described under
[Live verification with OpenAI](#live-verification-with-openai).

### Appearance and accessibility

- **Values persist:** font size, opacity (0% accepted), width and height were
  saved to `preferences.json` within the 400 ms debounce.
- **Reset** restored the defaults in the controls and the file.
- **Startup size:** the saved size applies at startup (800×500 and 1100×450
  were observed).
- **Live resize:** with the display on, setting 980×420 resized the window
  from 760×320 at once, and Reset restored 760×320. With the display off it
  could not be observed, because GTK receives no frame callbacks then.
- **Accessibility fix:** libadwaita 1.9's `AdwSpinRow` is not exposed to
  assistive technologies. A minimal libadwaita program confirmed this. The
  four numeric settings now use rows with labelled `GtkSpinButton`s, which
  appear in the accessibility tree with values.
- **Narrower window:** the minimum window width fell from 724 to 510 px
  (683 px in Translation mode) once long device names ellipsize.

### Packaging

- `scripts/build-deb.sh` builds `lcrt_2.0.0_amd64.deb` (about 2.5 MB).
- **Reproducible:** two consecutive builds produced byte-identical packages.
- **Runtime dependencies** come from `dpkg-shlibdeps`:
  - `libadwaita-1-0`, `libc6`, `libdbus-1-3`, `libgcc-s1`;
  - `libglib2.0-0t64`, `libgtk-4-1`, `libgtk4-layer-shell0`;
  - `libpango-1.0-0`, `libpipewire-0.3-0t64`, `libstdc++6`.
- **Recommends:** `pipewire` and `gnome-keyring`.
- `apt-get -s install ./lcrt_2.0.0_amd64.deb` resolves on Ubuntu 26.04.
- **Extracted package:** the binary reports `lcrt 2.0.0`, lists PipeWire
  sources, and opens the caption window.
- A launch panic was caught this way and fixed. The icon name had been set
  before GTK initialized.
- **Contents:** the desktop entry, icon and metainfo pass validation. The
  copyright file lists the license of each of the 158 linked crates.
- Ubuntu 24.04 does not package `libgtk4-layer-shell0`, so the `.deb`
  targets 24.10 and later.
- **Installed:** the owner installed the package with `sudo apt install`;
  `dpkg -s lcrt` reports `install ok installed`, version 2.0.0. The API key
  used below was entered through the installed application.

### Review fixes (Codex, PR #27)

Six P2 findings on `8f9726f` were fixed and covered by tests:

1. Preference changes reach the controller before Start and before Shutdown.
   A font size changed and followed immediately by a window close was saved
   (41 pt), and the app exited in 0.27 s.
2. Connecting tries every resolved address.
3. A rejected session or its settings ends the session with a message.
4. Save, Test and Clear report a busy controller, and Save no longer drops
   the typed key.
5. A failed preferences write is shown to the user and retried.
6. Only the latest credential action updates the status. The Test
   connection button showed its result and was usable again afterwards.

Seven more P2 findings on `6cebfa6` were fixed:

1. A rejected empty commit no longer holds up Stop.
2. When capture outruns the network, the stale audio backlog is skipped so
   captions follow live speech. A regression test sends 7.4 s of stale audio
   without the fix and under 2.5 s with it.
3. HTTP 4xx rejections are no longer retried as network failures.
4. A session's first caption is no longer erased by the session reset that
   was coalesced into the same update. After this change, JFK through system
   audio showed its first caption at 2.98 s and kept it after Stop.
5. Selecting different text retires a pending vocabulary explanation.
6. Start keeps an unsaved-settings warning visible until a save succeeds.
7. The README no longer describes an offline language control.

Items 4–6 are GTK and controller state paths without a unit harness. Item 4
was checked at runtime; items 5 and 6 are reviewed code only.

A third review, of `e0eaefa`, found one P1 and eight P2 issues. All were
fixed:

- **P1: Vocabulary off.** Turning Vocabulary off now cancels a selection
  still settling, so nothing is sent after the switch.
- **Audio backlog:** overflow is checked on every drained block. A new test
  with a stalled uplink fails without the fix: live audio never reached the
  service.
- **Caption delivery:**
  - The newest caption survives a full event queue. The test fails without
    the fix, with the caption stuck at "w64" of 70.
  - Captions received just before a failure are delivered ahead of it. That
    test also fails without the fix.
- **Bounded turns:** a turn the service never completes is retired, and late
  events for retired turns are ignored.
- **Error classification:**
  - Exhausted quota now gets billing guidance. The old test that expected
    "try again shortly" was corrected.
  - HTTP 408 is retried.
- **Clear:** an unavailable keyring is reported instead of claiming the key
  was removed.
- **Save warning:** a successful save clears the banner only while it still
  shows the save warning.

After these fixes, offline JFK through system audio showed its first caption
at 3.07 s, and the network-failure path still ended with a clear error after
3.9 s.

A fourth review, of `0a75eb2`, found three P2 issues. All were fixed:

- **Held caption before failure:** a caption held back by a full event queue
  is sent before a terminal failure. The test fails without the fix, with the
  caption stuck at "w64".
- **Vocabulary context:** context is bounded around the trimmed selection,
  even when the selection is padded with thousands of punctuation marks.
- **Smoke diagnostics:** a diagnostic now passes only if its backend became
  ready.
  - An online smoke run without network now exits 1: "the backend never
    became ready".
  - Before the fix, a Stop during reconnect backoff could exit 0.
  - The offline smoke run still exits 0.

A fifth review, of `c2eb4f4`, found three P2 issues. All were fixed with
tests:

- **Vocabulary answers:** the UI bridge keeps the newest answer, so an older
  lookup that finishes late can't replace it.
- **Turn text:** a turn's text is bounded even if the service never
  completes it.
- **Quota errors:** an HTTP 429 whose body reports exhausted quota is shown
  with billing guidance.

A sixth review, of `d2824b5`, found five P2 issues. All were fixed:

- turns first named by a delta respect the turn cap;
- skipping a stale audio backlog closes the open transcription turn;
- a diagnostic passes only if audio was captured;
- the declared minimum Rust version is now 1.88, the first that compiles the
  let-chains the code uses, verified with `cargo +1.88.0 check`;
- the speech gate that the last finding concerned was removed, as described
  under [Live verification with OpenAI](#live-verification-with-openai).

A seventh review, of `429f078`, found two P2 issues. Both were fixed:

- the translation early close could lose late text; it was removed after the
  measurement described under
  [Live verification with OpenAI](#live-verification-with-openai);
- translated-audio events are recognized by their top-level type, so a
  transcript that contains that event name is kept.

### Security review

- TLS certificate validation stays enabled: rustls with webpki roots, and no
  `danger`/`insecure` APIs.
- The key travels only in `Authorization` headers to `api.openai.com`.
- There are no `secret-tool` or subprocess calls.
- Only obviously fake `sk-` literals appear, and only in tests.
- **Fixed in this branch:** tracing-subscriber's `log` bridge forwarded
  tungstenite's trace of the raw handshake request, `Authorization` included.
  `RUST_LOG=trace` would therefore have printed the key. The bridge is
  removed: `tracing-log` is no longer in the dependency graph (`cargo tree`),
  so no `log` record from a dependency can reach the output.
- The GTK thread performs no network, keyring or file I/O. The last file
  check, in the model chooser, was removed.

## Live verification with OpenAI

The owner installed the package, entered an API key in Settings and saved it
to GNOME Keyring. LCRT read it from the keyring for every run below. The key
was never placed on a command line, in a file, or in these tests' logs.

**Test audio.** All audio was played with `pw-play` to the default sink and
captured from the **System audio** source.

| Audio | Source and license | Length |
| --- | --- | --- |
| English, Japanese, Vietnamese read speech | Google FLEURS test split (`google/fleurs` on Hugging Face), CC BY 4.0: the first 24, 18 and 19 utterances, each normalized to −20 LUFS and joined with 0.8 s gaps | 243 s, 242 s, 242 s |
| English continuous reading | LibriVox, *Short Nonfiction Collection, Vol. 100*, track 03 (public domain), first 240 s | 240 s |

FLEURS provides a reference transcript for every utterance, which the error
rates below are measured against. Several FLEURS recordings peak near
−44 dB, which is inaudible, so each utterance was loudness-normalized first.

**Connection.** Test connection returned "✓ Connection verified" (HTTP 200)
in 1.8 s. Sessions became ready 0.8–1.7 s after Start.

**Sessions.** Each ran about four minutes.

| Session | First caption | Error rate vs reference | Longest caption gap | Stop |
| --- | --- | --- | --- | --- |
| Online Captions, English | 2.16 s | 6.1% of words | 5.4 s | 1.39 s |
| Online Captions, Japanese | 3.09 s | 15.5% of characters | 8.6 s | 1.40 s |
| Online Captions, Vietnamese | 2.14 s | 7.5% of words | 4.7 s | 1.38 s |
| Online Captions, Auto (Vietnamese, 60 s) | 2.17 s | not scored | 4.8 s | 1.50 s |
| Translation EN→JA, original shown | 1.72 s | no reference | 1.7 s | 6.48 s |
| Translation JA→EN, original hidden | 5.05 s | no reference | 10.7 s | 6.71 s |
| Translation VI→EN, original shown | 2.03 s | original lane 7.7% of words | 3.6 s | 7.05 s |

- The Japanese error rate includes one utterance that stayed too quiet to
  capture even after normalization.
- The longest gaps span the pauses between utterances. The one exception,
  8.6 s in Japanese, lies inside that quiet utterance.
- Translations were fluent and followed the speech. With the original shown,
  the source language appeared above the translation; with it hidden, only
  the translation appeared.
- No session ended in an error, and memory stayed at 130–157 MB.
- Translation Stop takes 5–7 s because the service delivers the rest of the
  translation after `session.close`; see below.

**Vocabulary.**

- Selecting "communication" in an English caption showed the part of speech,
  the meaning and a sentence explaining its use in context, 2.5 s after the
  selection settled.
- A partial Japanese selection (ターネッ) was explained as part of
  「インターネット」, with its reading.

**Mid-session interruption.** LCRT ran in a private network namespace where
`api.openai.com` resolved to a local relay that forwarded the TLS bytes
unchanged. Blocking the relay for 2 s cut the connection without touching the
workstation's networking. This was done twice in one session:

- the status went to "Reconnecting…" and back to "Listening…" after 3.8 s;
- captions resumed, with a 5.1 s caption gap each time;
- the session continued without an error, and Stop took 2.3 s.

**Defects found by these live runs, and fixed.**

1. **Quiet and short speech was discarded.** The turn gate counted only
   frames above 0.01 RMS as speech and discarded turns with under 250 ms of
   it. The service had already transcribed that audio, so a spoken
   "However," was lost, and the abandoned items made Stop wait its full 8 s
   timeout (9.4 s measured). The gate is now 0.003, about −50 dBFS, and every
   turn is committed. Stop then took 1.4 s.
2. **The vocabulary answer closed its own popover.** GTK closes a popover
   that resizes unless its parent presents it again, and a text view does
   not. A minimal GTK program reproduced this on X11 and Wayland. LCRT now
   presents the popover after updating it.

The six sessions in the table ran on the build with fix 1. Fix 2 does not
change the transcription or translation path, and was verified live
afterwards.

**A change that was tried and withdrawn.** Translation Stop takes 5–7 s. An
early close after 1.5 s without a caption change cut that to 3.2 s, and
review questioned whether it could lose text. Measuring with Stop pressed
mid-speech settled it: the service sends the rest of the translation
4.6–4.9 s after `session.close`, just before `session.closed` (for example
"this page helped the national team qualify."). The early close was removed,
and Stop waits for `session.closed`, bounded at 8 s. On the final build a
mid-speech Stop took 5.3 s and kept the late text.

## Multi-language caption lanes

Added after the V2 merge, on `feature/multi-language-caption-lanes`.

**What it is.** In Translation mode the caption area is a stack of up to
three lanes, always in this order: the original speech (optional), the first
translation, and an optional second translation. Each lane has a language
badge (`JA`, `EN`, `VI`, …) and its own selectable text.

**Architecture.** The translation service takes one output language per
session, so each target is its own session: one or two, never more. Both
receive the same captured audio. Each has its own bounded queue and bounded
reconnects, and Stop asks both to close before waiting for either, so the
waits overlap.

**Automated tests** (228 workspace tests in total at the time of writing):

- target validation: duplicates, a target equal to the shown source, a second
  target without a first, and no target at all are each corrected;
- lane order and badges, and at most three lanes;
- settings persistence, and correction of an invalid stored combination;
- two sessions filling their own lanes; one target with the source hidden;
- the same audio reaching every lane;
- one lane failing while the other keeps its captions; the session failing
  only when its last lane does;
- the original lane staying with the session that transcribed first, and
  moving to a running session when that one fails;
- one lane reconnecting while the other keeps running;
- Stop closing every session, and being idempotent;
- a replaced session closing at once;
- lane updates from a replaced session never reaching the new one;
- a started session giving the window its own lane layout;
- a finish request never waiting on a full audio queue;
- how a lane fits its height: one line when short, whole lines only.

**Runtime checks with OpenAI**, through system audio, with the owner's key
read from the keyring. The window ran on a virtual display (`Xvfb`) for the
checks that needed a screenshot, because the workstation's screen was locked.

| Check | Result |
| --- | --- |
| Japanese speech, original shown, targets English and Vietnamese | Three lanes with badges `JA`, `EN`, `VI`. All three filled: first text at 3.4 s, 5.7 s and 6.1 s. Two sessions opened. Stop took 7.5 s. |
| English speech, original hidden, target Japanese | One lane with badge `JA`, one session, Japanese translation. |
| Lane settings persist | Turning the original lane on in Settings changed the layout at once (`EN`, `JA`), was saved, and survived a restart. A hand-edited file with two identical targets loaded as one target. |
| Vocabulary from several lanes | Selections in the English, Vietnamese and Japanese lanes were each explained from that lane's text. |
| Stop and restart with two targets | A second Start began with every lane empty and showed no text from the previous run. Closing the window took 0.32 s and left no process. Four sessions were opened across the two runs, two each. |
| Reconnect with two sessions | A 2 s network cut dropped both connections. Each reconnected within its own retries, the status returned to Translating after 4.2 s, and all three lanes resumed. |
| Lane change during a session | Hiding the original lane in Settings changed the layout after 0.16 s and the new session was translating after 1.8 s, with no text from the old one. |
| Other modes | Offline Captions still shows one unlabeled lane (JFK fixture, first caption at 3.0 s). |
| Layout at three window heights | At the default 320 px the window kept its height and each lane showed one full line that followed the newest words. At 480 px the same. At 620 px each lane wrapped to two whole lines; the README screenshot is from this run. |

**Two defects found by these runs, and fixed.**

1. Replacing a session (a lane change during a session) first took 9.5 s,
   because the old session waited for final words that would be discarded. A
   replaced session now closes at once.
2. With two sessions, the service sometimes needed more than the 8 s finish
   wait to deliver the last words. Translation now waits up to 12 s.

**Review fixes (Codex, PR #29).** A review of `17ad927` found one P1 and
three P2 issues. All were fixed:

- **P1:** asking a session to finish could wait on a full audio queue, which
  delayed the close request to the other lane. The request is now the act of
  closing the audio channel, so it never waits, and the `Finish` command is
  gone.
- A lane change during a session relabeled the rows at once, while the old
  session's text was still on them. Rows are now relabeled when the
  replacement session starts, with empty rows. A layout change while idle
  clears the previous session's text.
- A translation diagnostic could be given the same language as source and
  target. `--smoke-target` equal to `--language` is now rejected.
- A two-line minimum per lane made the window taller than the height the user
  set. Lanes now share the available height and fit their text to it.

A second review, of `827b02c`, found three P2 issues. All were fixed:

- Each lane still had a minimum height of one line, so three lanes at a
  large font made the window taller than the height the user set. The
  minimum is gone, and the badge is clipped with its row, so no part of a
  lane holds the window taller. The text is fitted again when the font size
  changes.
- The window took the lane layout from its own controls, which a diagnostic
  run (`--smoke-mode translation`) bypasses. A started session now gives the
  window its own options, so the badges always name the session's languages.
- The original lane came from the first running session that had any text,
  so a slower session could take it over with a shorter transcript and
  remove words already shown. The first session to transcribe now keeps the
  original lane until it fails.

A third review, of `e84df13`, found one P2: the session's options and its
running state reached the window in two updates, so the window could lay the
rows out between them from stale options. Both are now published in one
update.

Checked after these fixes, on the virtual display: three lanes at 32, 48 and
64 pt kept the 320 px window height (before: 385 px at 64 pt). At 32 pt each lane showed one whole line at 320 px and two at 620 px,
with live Japanese speech translated into English and Vietnamese. Lowering
the font from 64 to 20 pt in Settings refitted every lane at once. At 64 pt
in 320 px a lane is shorter than one line, so its text and badge are cut off
at the edges. The OpenAI account ran out of quota during these checks, so
the live runs after the badge change used idle lanes only.

**Not verified at runtime:** one lane failing while the other continues
(covered by tests only), pointer selection, and the layout on a real display
(the screenshot is from the virtual display).

## Built-in offline model and live language controls

Added on `feature/offline-multilingual-live-languages` (PR #30), on top of
`develop` `305ce42`.

### What changed

- **Offline Captions work without setup.** The package includes Whisper
  base, multilingual. Offline Captions offer Auto and the same eight
  languages as the online modes, and always transcribe in the spoken
  language (`translate = false`). Settings shows **Offline model: Built-in
  multilingual model**; a custom model is optional, under Advanced.
- **Live language controls.** In Translation, a chip per lane next to Start
  shows or hides its lane, and its menu pauses, resumes or removes that
  target. **+** adds a target. None of these restart the session: each
  target's session opens or closes on its own, and showing or hiding a lane
  changes nothing but the window.
- **Offline Translation is not included.** See
  [Offline Translation: blocked](#offline-translation-blocked).

### Bundled model

| | |
| --- | --- |
| Artifact | `ggml-base.bin` from `huggingface.co/ggerganov/whisper.cpp`, revision `5359861c739e955e79d9a303bcbc70fb988958b1` |
| Size | 147,951,465 bytes |
| SHA-256 | `60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe` (the repository's own SHA-1, `465707469ff3a37a2b9b8d8f89f2f99de7299dac`, also matches) |
| License | MIT, OpenAI's Whisper weights (`openai/whisper` `LICENSE`, copied to `packaging/licenses/whisper-MIT.txt` and into the package's `copyright` file) |
| Installed at | `/usr/share/lcrt/models/ggml-base.bin` |

[packaging/models.json](../packaging/models.json) pins the artifact.
`scripts/fetch-models.py` downloads it once into `target/share/lcrt/models`,
reuses a cached file only if its SHA-256 matches, and discards a download of
the wrong size or SHA-256 and fails. `scripts/build-deb.sh` runs it first.
At runtime LCRT checks the model's size, not its hash, so Start never hashes
148 MB; a missing or truncated model is reported as needing a reinstall, and
LCRT never downloads one. A unit test keeps the size, file name and install
path in the code equal to the manifest's. CI does not download the model.

| Package | |
| --- | --- |
| `.deb` | 131,191,908 bytes (125 MiB) |
| Installed size | 152,696 KiB (149 MiB), of which the model is 148 MB |

### Changes found necessary by measurement

Each of these was measured before and after; the numbers are below.

1. **Auto detects the language among LCRT's eight.** Whisper's own
   detection picked English or Hindi for Japanese speech on short windows
   (Devanagari captions for Japanese audio). LCRT now takes the most likely
   of the eight offered languages, and keeps it only once a pass of at least
   3 s is at least 70% sure.
2. **Auto keeps the language it detected.** Detection is a separate encoder
   pass. Detecting on every pass made inference fall 8 s behind capture
   within 45 s; once per utterance still failed on two CPUs, after 81 s. A
   confident language is now kept and checked again every 30 s.
3. **Whisper uses the CPUs it may run on**, at most four, instead of always
   four. On two CPUs, four threads made explicit Japanese fail within 11 s.
4. **The encoder is sized to the 8 s window** (`audio_ctx` 400 instead of
   Whisper's 1,500 for 30 s). A pass on two CPUs took 3.5–4.6 s before and
   about 1 s after, captions updated twice as often, and recall was equal
   or better (table below).
5. **An English-only custom model is refused** for any spoken language other
   than English or Auto, using whisper.cpp's own `is_multilingual`, never the
   file name.

### Offline Captions through system audio

Natural speech: FLEURS test utterances (CC BY 4.0), the same files as the V2
online runs, played to the default sink and captured from its monitor. Each
run played 90 s. The app ran inside `bwrap --unshare-net` under `strace`
recording every `socket` and `connect` call.

**Recall** is the share of the reference, in characters (Japanese) or words,
that the captions recover in order. Offline captions repeat overlapping
phrases (see [Known limitations](#known-limitations)), which pushes the error
rate above 100% without reflecting recognition, so recall is the measure
compared here.

| Speech, language | First caption | Recall | Updates | Pass median / max | Stop | Peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| English, explicit | 2.3 s | 93.3% | 46 | 0.39 s / 3.28 s | 0.9 s | 343 MB |
| Japanese, explicit | 3.5 s | 78.7% | 41 | 0.49 s / 3.32 s | 0.9 s | 353 MB |
| Vietnamese, explicit | 2.3 s | 70.7% | 46 | 0.52 s / 2.98 s | 0.9 s | 351 MB |
| English, Auto | 3.9 s | 95.0% | 47 | 0.38 s / 2.45 s | 0.9 s | 460 MB |
| Japanese, Auto | 4.2 s | 79.1% | 41 | 0.62 s / 2.80 s | 0.8 s | 470 MB |
| Vietnamese, Auto | 3.7 s | 69.3% | 47 | 0.56 s / 3.15 s | 0.8 s | 460 MB |

All 16 CPUs, final build. "Updates" counts caption changes in 90 s; a pass
is one Whisper inference, including Auto's detection when it runs. Peak RSS
is the highest of the samples taken every 10 s. Recall with Whisper's full
30 s encoder, measured the same way before change 4, was 94.4% (English),
78.4% (Japanese) and 68.7% (Vietnamese), with about half as many updates.

Every session reached Stopped without an error, and opened **no IPv4 or IPv6
socket**: its only sockets were Unix sockets (D-Bus, PipeWire, X11,
accessibility). The captions were in the spoken language's own script in
every run; no run produced English for Japanese or Vietnamese speech.

**Fresh install.** The `.deb` was unpacked into an empty directory and its
`usr/bin/lcrt` started with an empty home and configuration directory, no
network, and no settings changed: Offline Captions, Auto, system audio. It
loaded its own `usr/share/lcrt/models/ggml-base.bin` and captioned 60 s of
Japanese speech: first caption 3.7 s, recall 76.1%, Stop 0.8 s, no network
socket (0 of 63 socket calls).

### Low-resource check

No 2 GB Pentium machine was available. The runs below are a simulation on
the same laptop: the app was pinned to **two logical CPUs** (`taskset -c 2,3`
of an i5-12500H) in a user scope with **`MemoryMax=2G`** and no swap. They
are not hardware certification; a Pentium-class CPU is slower than two
threads of this one.

| Speech, language | First caption | Recall | Updates | Pass median / max | Stop | Peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| English, explicit | 2.9 s | 95.0% | 46 | 0.92 s / 2.83 s | 0.9 s | 342 MB |
| Japanese, explicit | 4.4 s | 77.7% | 40 | 1.09 s / 4.04 s | 0.9 s | 354 MB |
| Vietnamese, explicit | 2.8 s | 70.0% | 45 | 1.06 s / 4.68 s | 0.9 s | 348 MB |
| English, Auto | 7.0 s | 94.4% | 45 | 0.93 s / 4.69 s | 0.9 s | 459 MB |
| Japanese, Auto | 12.1 s | 59.8% | 36 | 1.18 s / 6.79 s | 0.8 s | 470 MB |
| Vietnamese, Auto | 6.4 s | 69.3% | 44 | 1.13 s / 4.42 s | 0.9 s | 464 MB |

Every run reached Stopped without an error. Inference kept pace: the
median pass took about 1 s for 8 s of audio, and captions did not fall
progressively behind. Auto Japanese started late because its first
detections were unsure ("en" at 52%) until Japanese was detected at 99%;
the captions were Japanese throughout. These runs used the build before
the last, error-message-only edit.

Before changes 3 and 4, the same setup failed: explicit Japanese after 11 s
and Auto after 19–81 s, each with "Whisper input backlog reached 8s of
audio". Peak resident memory stayed under 500 MB in every offline run.

### Live language controls

**Real pointer and keyboard input** on the virtual display (Xvfb, X11
backend), through the XTEST extension (`target/acceptance/chips_pointer.py`
and `chips_menus.py`); state was read back through AT-SPI and from the saved
preferences. Translation with JA (spoken), EN and VI, idle:

| Step | Result |
| --- | --- |
| Click the JA chip | the source lane hides, the chip dims, `show_original` is saved as false |
| Click it again | the lane is back |
| Hide EN and JA, then click VI | VI stays: the last visible lane can't be hidden |
| EN's menu → Remove language | EN's lane and chip go; VI stays with its text |
| **+** | lists English, Chinese, Korean, Spanish, French, German (not Japanese, the spoken language, nor Vietnamese, a target); choosing German adds its lane |
| With two targets | **+** is disabled ("Maximum 2 translation languages.") |
| The only target's menu | Remove language is disabled |
| Tab | reaches the chips; Space toggles a chip; Space on its arrow opens the menu and Return runs its first item |

The menus show **Pause translation**/**Resume translation** only while a
session runs.

**While a session runs:** the OpenAI account had no quota (a 12 s
translation diagnostic failed with "the account's quota is exhausted"), so
no live session could run, and **adding, pausing, resuming and removing a
target in a live cloud session is not retested at runtime.** It is covered
by deterministic tests that run the real per-target sessions and their
worker threads against a scripted service:

- an added target opens only its own session, from the live point, while
  the other keeps its session and text;
- a removed target closes only its session, and its late text never
  reappears;
- a paused target receives no audio while the other does; resuming opens
  only its session again;
- rapid add, remove and add opens at most one session per target, with one
  connected once it settles;
- a session closed while still connecting can't report over the lane that
  replaced it;
- Stop while a target is connecting ends in bounded time, and dropping the
  session while a target resumes closes every connection;
- a failed target is reported, can be resumed, and the session fails only
  when its last running target does;
- the window's caption rows stay bound to their language, so adding or
  removing another lane never moves or clears a lane's text.
- removing the lane that provided the source text keeps that text until
  another running lane has a transcript (Codex review of `b33e126`);
- a late status from a removed target is dropped, so the window can't keep
  saying "Translating…" for it (same review);
- naming a running target as the spoken language in Settings restarts the
  session without that target, instead of leaving it translating into the
  spoken language (same review); and a target change the controller's queue
  refuses is neither shown nor saved;
- a lane's failure banner is retired when the lane recovers or is removed,
  the overall status is recomputed whenever the targets change, and Pause is
  offered only once a session runs, not while a replacement starts (Codex
  review of `c5e49b9`);
- `--language` rejects a code LCRT doesn't offer instead of running the
  diagnostic with Auto (same review).

### Offline Translation: blocked

The task asked for offline translation with Tencent's Hy-MT2 1.8B, 1.25-bit
GGUF, run in-process through llama.cpp. It is not included, because it
cannot run usefully on x86.

- **Artifact and license verified:** `tencent/Hy-MT2-1.8B-1.25Bit-GGUF`,
  revision `9df5c824a00a744fb0512a29c640466f4d97dfb0`, `Hy-MT2-1.8B-1.25Bit.gguf`,
  461,860,800 bytes, SHA-256
  `cc497fe8f033b52b3b8b00a7669e9661435432f9d4cd43f7ed24400c01507a93`,
  Apache-2.0 (the repository's `LICENSE.txt`). English, Japanese,
  Vietnamese, Chinese, Korean, Spanish, French and German are all among its
  33 languages. The license would allow bundling.
- **No released llama.cpp loads it.** Its 224 weight tensors use the
  "STQ1_0" format (1.3125 bits per weight) from llama.cpp PR #22836, which
  is open and unmerged. The file numbers that format 42, which upstream now
  uses for `Q2_0`; the PR numbers it 43. Unmodified, the file fails to load
  ("tensor … has offset …, expected …").
- **With the PR applied, it is too slow on x86.** Applied to llama.cpp
  `b11074` (the version `llama-cpp-sys-2` 0.1.157 vendors) and with the type
  number rewritten (weights unchanged), it loads and translates correctly:
  「今日はとても良い天気ですね。散歩に行きましょう。」 → "It's a great day today.
  Let's go for a walk." But the PR has only an ARM kernel; on x86 it runs a
  scalar fallback. At **2 threads: 0.8 tokens/s** for both prompt and
  output, **76 s for that one sentence**, peak RSS 540 MB. At 8 threads,
  1.6–1.9 tokens/s. A live lane needs a sentence in a few seconds.
- For comparison only, not adopted: the same model as the official Q4_K_M
  GGUF loads in unmodified llama.cpp and translated the sentence in about
  3 s on 2 threads, but needs 1.9 GB of resident memory, over the 2 GB
  budget with Whisper beside it.

The owner chose to stop here rather than write an x86 SIMD kernel or switch
models. Offline Translation needs one of those before it can ship.

## Known limitations

- **Offline repetition:** see above. A timestamp-based commit that removes
  overlap without silently dropping words is follow-up work. It is not part
  of this PR.
- **Offline speech detection:** it uses the V1 fixed RMS threshold.
- **Online turn boundaries:** a fixed level (−50 dBFS) decides when a turn
  ends. Speech over continuous background sound is committed every 15 s
  instead of at pauses; captions still update continuously.
- **Online recovery:** a reconnect discards audio captured during the outage.
  Online modes never fall back to another backend by themselves.
- **Stop wait bound:** for an unresponsive online service, Stop waits at
  most 18 s in Online Captions and 22 s in Translation (a 10 s handshake plus
  an 8 s or 12 s finish wait). Measured live: 1.4 s for captions and 5–8 s
  for translation, which is the service finishing the translation, and 14 s
  once when the service was slow.
- **Translation lanes:** at most three, the original and two targets. Each
  target is a separate paid session. The original lane's badge is `SRC`
  until the spoken language is named in Settings, and only then can a target
  equal to it be prevented.
- **Window height:** three lanes at a very large font in a short window are
  each shorter than one line, and their text is cut off at the edges. The
  window keeps the size you chose and its controls stay visible; hiding a
  lane with its chip gives the others its room.
- **No Offline Translation:** see
  [Offline Translation: blocked](#offline-translation-blocked).
- **Offline accuracy:** the base model recovers about 95% of English, 75–80%
  of Japanese and about 70% of Vietnamese (recall, above). Auto starts more
  slowly than a named language and can miss the first seconds while it is
  still unsure.
- **Live target changes in a cloud session** are covered by tests only; see
  [Live language controls](#live-language-controls).
- **The source badge** stays `SRC` under Auto: the translation service does
  not report the language it detects.
- **Window size:** a width below the control row's minimum (510–683 px) has
  no further effect.
- **Not tested:**
  - pointer selection of caption text (the lane chips were tested with real
    pointer and keyboard input);
  - a 2 GB Pentium-class machine (simulated, above);
  - live resize on X11;
  - layer-shell overlay (GNOME lacks it);
  - microphone input in the online modes;
  - ARM64 and Windows runtime.
