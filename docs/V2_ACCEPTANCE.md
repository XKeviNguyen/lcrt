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
- The translation Stop times above are from before the fix described below;
  after it, Stop took 3.2 s.

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
2. **Translation Stop waited for a slow confirmation.** The service takes
   5–7 s to confirm `session.close`, during which no caption changes.
   Translation now closes once captions have been quiet for 1.5 s.
3. **The vocabulary answer closed its own popover.** GTK closes a popover
   that resizes unless its parent presents it again, and a text view does
   not. A minimal GTK program reproduced this on X11 and Wayland. LCRT now
   presents the popover after updating it.

The six sessions in the table ran on the build with fix 1. Fixes 2 and 3 do
not change the transcription path, and were each verified live afterwards.

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
- **Stop wait bound:** Stop waits at most 18 s (10 s handshake + 8 s finish)
  for an unresponsive online service. Measured live: 1.4 s for captions and
  3.2 s for translation.
- **Window size:** a width below the control row's minimum (510–683 px) has
  no further effect.
- **Not tested:**
  - pointer and keyboard input (controls and selections were driven through
    accessibility);
  - live resize on X11;
  - layer-shell overlay (GNOME lacks it);
  - microphone input in the online modes;
  - ARM64 and Windows runtime.
