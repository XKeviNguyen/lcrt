# V2 acceptance

This report records what the V2 release candidate (`feature/v2-final`)
delivers and the evidence behind each claim. It also records what has not
yet been verified.

V2 adds four things on top of the V1 offline path:

- online captions;
- real-time translation;
- vocabulary explanations;
- credential, preference and appearance management, plus an Ubuntu package.

**Status:** everything that can be verified without an OpenAI API key has been
verified. The online and translation modes, vocabulary lookups, and a
successful Test connection have passed their automated tests against a
scripted fake service. They have **not** been exercised against OpenAI,
because no API key was available. See [Credential checkpoint](#credential-checkpoint).

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
- The workstation's screen was locked with the display powered off
  (`PowerSaveMode` 3) for most of the session. The compositor then sends no
  frame callbacks, which limits some UI checks. The affected rows say so.

## Evidence by category

| Area | Implemented | Unit / mock tested | Runtime tested here | Not verified |
| --- | --- | --- | --- | --- |
| Offline captions (Whisper) | yes (V1, preserved) | yes | system audio, 18.6 min of natural speech; microphone smoke | — |
| Online captions (`gpt-live-transcribe`) | yes | yes: protocol, out-of-order completions, reconnect, rejected key | missing-key and network-failure paths | live transcription with OpenAI (EN/JA/VI) |
| Translation (`gpt-realtime-translate`) | yes | yes: both lanes, show-original, `session.close` → `session.closed` | through the shared online session paths | live translation with OpenAI (EN→JA, JA→EN, VI→EN) |
| API key storage | yes | yes | real Secret Service: save, reload after restart, clear | — |
| Test connection | yes | yes | entered, saved, malformed and missing keys; unreachable service | a successful check against OpenAI |
| Vocabulary popover | yes | yes: request bounds, parsing, cache | selection → popover → missing-key guidance | a live explanation from OpenAI |
| Appearance and preferences | yes | yes: normalization, persistence | persisted values, reset, startup size | live resize on a lit display |
| Packaging (`.deb` 2.0.0) | yes | metadata validated | reproducible build, `apt-get -s`, packaged GUI launch | `dpkg -i` (needs sudo) |
| CI | workflow updated | — | pending for the final head | — |

"Hardware tested" applies only to this Ubuntu AMD64 laptop. ARM64 and Windows
remain compile-portable goals and were not run.

## Automated verification

On the final code commit:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo test --locked --workspace --all-features` | 181 passed, 0 failed |
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

On Wayland with the screen locked, GTK's claim to the PRIMARY selection was
refused, because an AT-SPI selection carries no input serial. The selection
therefore collapsed before the lookup could start. The same check passed on
the X11 backend (XWayland). Pointer selection on a lit Wayland session
remains to be confirmed.

### Appearance and accessibility

- **Values persist:** font size, opacity (0% accepted), width and height were
  saved to `preferences.json` within the 400 ms debounce.
- **Reset** restored the defaults in the controls and the file.
- **Startup size:** the saved size applies at startup (800×500 and 1100×450
  were observed).
- **Live resize:** it could not be observed. With the display off, GTK
  receives no frame callbacks and did not commit a new size. A minimal GTK
  program behaved the same way under these conditions and resized normally in
  the same session when it had frames.
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

## Credential checkpoint

No OpenAI API key was available (`OPENAI_API_KEY` absent, keyring empty).
Acceptance of the cloud features needs one run with a key:

1. **Setup:** Settings → Online → paste the key → Test connection
   (expect "✓ Connection verified") → Save securely.
2. **Online Captions:** about 5 minutes of natural speech in each of
   English, Japanese and Vietnamese, played through system audio, with the
   spoken language set and with Auto.
3. **Translation:** about 5 minutes each of EN→JA, JA→EN and VI→EN, with
   Show original on and off.
4. **Vocabulary:** select words in English and Japanese captions.
5. **Network failure:** one brief interruption during an online session
   (for example, disable Wi-Fi for 5 s in the user session). Expect
   Reconnecting… and recovery, or a clear error.

Record latency to the first caption, the stability of Stop, and any errors.

## Known limitations

- **Offline repetition:** see above. A timestamp-based commit that removes
  overlap without silently dropping words is follow-up work. It is not part
  of this PR.
- **Offline speech detection:** it uses the V1 fixed RMS threshold.
- **Online recovery:** a reconnect discards audio captured during the outage.
  Online modes never fall back to another backend by themselves.
- **Stop wait bound:** Stop waits at most 18 s (10 s handshake + 8 s finish)
  for an unresponsive online service.
- **Window size:** a width below the control row's minimum (510–683 px) has
  no further effect.
- **Not tested:**
  - live resize on X11;
  - layer-shell overlay (GNOME lacks it);
  - pointer input;
  - ARM64 and Windows runtime.
