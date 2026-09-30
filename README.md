<div align="center">

<img src="docs/assets/lcrt-banner.svg" alt="LCRT: live captions, real-time translation and words in context. One sentence shown in English, Japanese and Vietnamese." width="100%">

<br>

[![CI](https://github.com/XKeviNguyen/lcrt/actions/workflows/ci.yml/badge.svg?branch=develop)](https://github.com/XKeviNguyen/lcrt/actions/workflows/ci.yml)
![Version 2.0.0](https://img.shields.io/badge/version-2.0.0-1a5fb4)
![License MIT](https://img.shields.io/badge/license-MIT-2ea043)
![Ubuntu AMD64](https://img.shields.io/badge/platform-Ubuntu%20AMD64-e95420?logo=ubuntu&logoColor=white)
![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-b7410e?logo=rust&logoColor=white)
![GTK4 and libadwaita](https://img.shields.io/badge/UI-GTK4%20%2B%20libadwaita-4a86cf?logo=gtk&logoColor=white)
![No telemetry](https://img.shields.io/badge/telemetry-none-0b1220)

**[Install](#-install-on-ubuntu)** ·
**[Use](#-use)** ·
**[Modes](#-three-modes-one-window)** ·
**[Measured](#-measured-not-promised)** ·
**[Privacy](#-privacy-at-a-glance)** ·
**[How it works](#-how-it-works)** ·
**[Develop](#-development)**

</div>

---

LCRT is a native desktop app for live captions and real-time translation. It
captions whatever your computer is playing, or your microphone, in a small
window that stays out of the way.

<table>
<tr>
<td width="33%" valign="top">

### 🎧 Hear it, read it

Captions for **system audio** (videos, calls, lectures) or your
**microphone**, updated as the words are spoken.

</td>
<td width="33%" valign="top">

### 🌏 Across languages

Real-time **translation** into one or two languages at once, each in its
own labeled lane, with the original speech above them when you want it.

</td>
<td width="33%" valign="top">

### 📖 Learn as you go

**Select a word** in the captions to see its meaning, reading and how it is
used in that sentence.

</td>
</tr>
<tr>
<td valign="top">

### 🔒 Private by default

**Offline Captions** run a local Whisper model. Audio never leaves the
device, and LCRT has no telemetry.

</td>
<td valign="top">

### 🔑 Your own key

Online features use **your** OpenAI API key, stored in the desktop keyring
and never in a file or log.

</td>
<td valign="top">

### 🎨 Make it yours

Font, size, colors, transparency and window size are adjustable and
remembered.

</td>
</tr>
</table>

Primary platform: **Ubuntu AMD64** (PipeWire, GTK4, libadwaita, Wayland or
X11). Ubuntu ARM64 and Windows 10/11 are portability targets.

## 🧭 Three modes, one window

<div align="center">
<img src="docs/assets/lcrt-flow.svg" alt="Audio from the system or the microphone is captured with PipeWire and goes to exactly one backend: Whisper on this device, OpenAI realtime transcription, or OpenAI realtime translation. The result appears in the caption window, where selecting text asks for its meaning in context." width="100%">
</div>

| | Offline Captions | Online Captions | Translation |
| --- | --- | --- | --- |
| **Engine** | Whisper, on this device | OpenAI realtime transcription | OpenAI realtime translation |
| **Audio goes to** | nowhere | OpenAI, while a session runs | OpenAI, while a session runs |
| **Languages** | the chosen model's (the tiny model is English) | English, Japanese, Vietnamese, Chinese, Korean, Spanish, French, German, or Auto | the spoken language is detected automatically; translated into one or two of those eight |
| **You choose** | a Whisper model file | the spoken language, or Auto | one or two target languages; show or hide the original |
| **Needs** | a model file | your OpenAI API key and a network | your OpenAI API key and a network |
| **Cost** | none | charged to your OpenAI account | charged to your OpenAI account, once per target language |

Each session uses exactly one backend. LCRT never switches to another backend
or to a paid service on its own.

### Multi-language lanes

<div align="center">
<img src="docs/assets/lcrt-lanes.png" alt="The LCRT window during a translation session: three stacked caption lanes labeled JA, EN and VI, showing Japanese speech with its English and Vietnamese translations." width="640">
<br>
<sub>A real session. The speech is a FLEURS test utterance (CC BY 4.0).</sub>
</div>

In Translation mode the caption area becomes a stack of lanes. Each lane has a
language badge on the left and its own selectable text:

1. the **original speech**, when you choose to show it;
2. the **first translation**;
3. an optional **second translation**.

The order never changes, and there are at most three lanes: the original and
two translations. For example, Japanese speech can be shown with English and
Vietnamese below it.

- **One session per target.** The translation service takes one output
  language per session, so a second target opens a second session and is
  charged separately. There are never more sessions than targets.
- **Targets are kept valid.** Two targets can't be the same, and a target
  can't repeat a spoken language you named. Such a choice is corrected as
  soon as you make it, and there is always at least one target.
- **Lanes fail independently.** If one target's session fails, LCRT says so
  and the other lane keeps translating.
- **Words in context work in every lane.** Select text in any lane to have it
  explained from that lane's own text.
- **Lanes fit the window you chose.** The lanes share the window's height and
  never enlarge it. A lane with room for two lines wraps its text, as in the
  picture. With less room it shows one line that follows the newest words.
  Either way only whole lines are shown.

## 📊 Measured, not promised

These numbers come from real runs through system audio on one Ubuntu 26.04
laptop, scored against reference transcripts. The method, test audio and
every result are in [docs/V2_ACCEPTANCE.md](docs/V2_ACCEPTANCE.md).

| Session | First caption | Error against the reference | Stop |
| --- | --- | --- | --- |
| Online Captions, English | 2.2 s | 6.1% of words | 1.4 s |
| Online Captions, Japanese | 3.1 s | 15.5% of characters | 1.4 s |
| Online Captions, Vietnamese | 2.1 s | 7.5% of words | 1.4 s |
| Translation, English → Japanese | 1.7 s | not scored | 6.5 s |
| Translation, Vietnamese → English | 2.0 s | original lane 7.7% of words | 7.1 s |
| Offline Captions, English (tiny model) | 4.0 s | see the note below | 0.3 s |

- **Network drops:** after a 2 s cut in the middle of a session, captions
  were back 3.8 s later without an error.
- **Translation takes a few seconds to stop** because the service delivers
  the last words of the translation after you press Stop.
- **Offline Captions repeat phrases** on continuous speech: about 40% of the
  words are repeats. It is the main known quality issue, and it is listed
  with the other [limitations](#-limitations).

## 📦 Install on Ubuntu

The Debian package is built for the Ubuntu release it is built on. It needs
`libgtk4-layer-shell0`, which Ubuntu packages from 24.10 onward. Ubuntu 26.04
LTS is the tested release.

```sh
scripts/build-deb.sh            # prints target/debian/lcrt_2.0.0_amd64.deb
sudo apt install ./target/debian/lcrt_2.0.0_amd64.deb
```

Then open **LCRT Live Captions** from the app grid, or run `lcrt`.

## 🚀 Use

1. **Choose a mode and an audio source.** System audio sources are listed as
   **System audio**, microphones as **Microphone**. Online modes also have a
   language:
   - Online Captions: the spoken language, or Auto.
   - Translation: the target language. The spoken language is detected
     automatically.

   Offline Captions has no language choice. It follows the chosen Whisper
   model.
2. **Press Start.** Captions update as speech is recognized. **Stop** finishes
   the last sentence and keeps the text on screen.
3. **Select a word or phrase** in the captions to see what it means in that
   sentence.
4. **Open Settings** to choose the Whisper model, enter your OpenAI API key,
   set up translation lanes, and adjust appearance and vocabulary.

In Translation mode the window shows the first target and an **Original**
checkbox. **Settings → General → Translation lanes** has the rest: the spoken
language (it names the original lane), and translation targets 1 and 2.
Changing a lane while a session runs restarts the session with the new lanes.

LCRT remembers the last mode, source and languages.

<details>
<summary><b>Offline model</b>: where to get one</summary>

<br>

LCRT does not download models by itself. Download the checksum-verified tiny
English model and choose it in **Settings → General**:

```sh
./scripts/download-whisper-model.sh     # saves models/ggml-tiny.en.bin
```

</details>

<details>
<summary><b>OpenAI API key</b>: how it is stored</summary>

<br>

Enter the key in **Settings → Online** and choose **Save securely** to store
it in the desktop keyring (GNOME Keyring or another Secret Service provider).
**Test connection** checks the key.

- If no keyring is available, the key is kept only until LCRT quits.
- As a fallback, LCRT reads `OPENAI_API_KEY` from its environment and never
  displays it.
- The key is never written to a file or log.

</details>

<details>
<summary><b>Window behavior</b>: staying on top</summary>

<br>

On Wayland compositors that support layer-shell protocol v4 or newer, the
caption window is pinned near the bottom of the screen above other windows.
GNOME Wayland, X11 and older compositors use a standard window: transparency
still works, but LCRT cannot keep it on top.

</details>

## 🔒 Privacy at a glance

| What you do | What leaves your computer |
| --- | --- |
| Offline Captions | Nothing. |
| Online Captions | Audio from the selected source, only while speech is detected, plus the language hint. |
| Translation | All audio from the selected source while the session runs, plus the target language. With two targets, the same audio goes to two sessions, one per target. |
| Select text, with Vocabulary on | The selection (at most 200 characters) and up to 160 characters of caption on each side. |
| Select text, with Vocabulary off | Nothing. |
| Test connection | One request that lists models. No audio or text. |

LCRT has no telemetry, analytics or crash reporting, and it saves neither
audio nor transcripts to disk. Everything it sends goes from your computer
directly to OpenAI. [docs/PRIVACY.md](docs/PRIVACY.md) has the full details.

## 🧠 How it works

```mermaid
flowchart LR
    PW["PipeWire capture<br>lcrt-audio-pipewire"] --> P["Caption pipeline<br>lcrt-core"]
    P --> W["Whisper backend<br>lcrt-stt-whisper"]
    P --> O["OpenAI realtime backends<br>lcrt-openai"]
    W --> C["Caption state<br>lcrt-core"]
    O --> C
    C --> UI["Caption window<br>lcrt-ui-gtk"]
    UI -. "selected text" .-> V["Vocabulary lookup<br>lcrt-openai"]
    V -.-> UI
    APP["Controller, settings, credentials<br>lcrt-app"] --- P
    APP --- UI
```

| Crate | What it holds |
| --- | --- |
| `lcrt-core` | The portable domain: audio chunks, the caption pipeline, caption state, sessions and preferences. No OS-specific code. |
| `lcrt-audio-pipewire` | PipeWire capture for microphones and system-output monitors. |
| `lcrt-stt-whisper` | Local speech-to-text through whisper.cpp, with a bounded rolling window. |
| `lcrt-openai` | Realtime transcription and translation over WebSocket, vocabulary lookups, and keyring-backed credentials. |
| `lcrt-ui-gtk` | The GTK4 and libadwaita caption window, Settings and the vocabulary popover. |
| `lcrt-app` | The `lcrt` binary: the controller that owns sessions, settings and credentials. |

A few rules shape the design:

- **One backend per session.** Starting a new session replaces the running
  one only after it has fully stopped.
- **Bounded everywhere.** Audio queues, caption history and reconnects all
  have limits. When the network falls behind, LCRT skips stale audio to stay
  with live speech.
- **The window never waits.** Network, keyring and file work happen off the
  GTK thread.
- **Late events can't leak.** Every session has a generation, and updates
  from a replaced session are dropped.

The portable core and its adapter boundaries are described in
[docs/architecture.md](docs/architecture.md).

## 🚧 Limitations

- **Offline Captions repeat overlapping phrases** on continuous speech. V1
  chose a visible repeat over silently losing words. A better fix is planned.
- Offline accuracy and speed depend on the model and CPU. The tiny model is
  English-focused.
- Audio sources are discovered at launch.
- Online modes need a network connection. LCRT reconnects a few times after a
  brief drop, then reports the problem.
- Translation shows at most three lanes: the original and two targets. Each
  target is a separate paid session.
- The original lane's badge shows `SRC` until you name the spoken language in
  Settings, because Translation detects the language without reporting it.
  For the same reason, a target that equals the spoken language can only be
  prevented once you have named it.
- Translation takes 5–8 s to stop, and up to about 14 s when the service is
  slow, because the service delivers the last words after you press Stop.
  Changing lanes during a session does not wait for them.
- Always-on-top needs a compositor with layer shell. GNOME does not provide
  it.

## 📍 Roadmap

| | Milestone | Status |
| --- | --- | --- |
| **V1** | Live captions from the microphone and system audio, offline | ✅ Done |
| **V2** | Online captions, real-time translation, vocabulary, secure key storage, appearance, Ubuntu package | ✅ Done |
| **V3** | Richer language help (grammar, examples), plus vocabulary history | 🔭 Planned |
| Later | Ubuntu ARM64 and Windows 10/11 | 🔭 Planned |

## 🔧 Development

The checked-in `rust-toolchain.toml` pins the primary development and CI
toolchain to Rust 1.98.0 with the `rustfmt` and `clippy` components. This is
separate from the workspace's declared minimum, Rust 1.88 (`rust-version` in
`Cargo.toml`), which is the first release with the let-chains the code uses.

Install the native development prerequisites (Ubuntu 24.04 or newer):

```sh
sudo apt install build-essential clang cmake libadwaita-1-dev libdbus-1-dev \
  libgtk-4-dev libpipewire-0.3-dev libspa-0.2-dev libwayland-dev meson \
  ninja-build pkg-config wayland-protocols
scripts/install-gtk4-layer-shell.sh
```

The layer-shell installer skips its checksum-verified source build when the
system already provides GTK4 layer shell 1.0.4 or newer.

Run the same local quality gates as CI with:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps
```

Run from source, optionally overriding the model for one run:

```sh
cargo run -p lcrt-app --bin lcrt -- --model models/ggml-tiny.en.bin
```

<details>
<summary><b>Diagnostics</b>: source IDs and bounded runs</summary>

<br>

For source IDs, and a bounded diagnostic run that starts captions itself and
closes after the given time:

```sh
cargo run -p lcrt-app --bin lcrt -- --list-sources
cargo run -p lcrt-app --bin lcrt -- \
  --model models/ggml-tiny.en.bin --smoke-source SOURCE_ID --smoke-seconds 10
```

`--smoke-mode online|translation` runs the same diagnostic against OpenAI and
uses the saved or `OPENAI_API_KEY` key. A diagnostic passes only if its
backend became ready and audio was captured. Closing the window cancels the
session without waiting on a blocked worker, so a stuck native call cannot
delay exit.

</details>

<details>
<summary><b>Linux audio</b>: the PipeWire capture utility</summary>

<br>

PipeWire capture development requires `libpipewire-0.3-dev`,
`libspa-0.2-dev`, and `pkg-config`. The bounded diagnostic utility enumerates
both microphone sources and system-output monitor targets:

```sh
cargo run -p lcrt-audio-pipewire --bin lcrt-pw-capture -- list
cargo run -p lcrt-audio-pipewire --bin lcrt-pw-capture -- capture <source-id> 3
```

The capture duration is clamped to 1–30 seconds. The utility reports the
negotiated format and aggregate sample statistics. It neither records audio
to disk nor silently substitutes synthetic audio when PipeWire fails.

</details>

<details>
<summary><b>Caption window</b>: the scripted demo</summary>

<br>

The Ubuntu window uses GTK4 and libadwaita. Install `libgtk-4-dev` and
`libadwaita-1-dev`, then launch its incremental-caption demonstration with:

```sh
cargo run -p lcrt-ui-gtk --bin lcrt-caption-ui
```

The demo drives the same window with scripted partial and final captions. Its
`--smoke-test` mode injects deterministic updates and closes itself. It does
not exercise audio capture or online services.

</details>

<details>
<summary><b>Local Whisper</b>: the file transcription utility</summary>

<br>

The speech-to-text adapter uses whisper.cpp through `whisper-rs`, runs model
inference on a dedicated worker, downsamples input to 16 kHz mono, and keeps
both its input queue and rolling audio window bounded. Models are deliberately
excluded from Git. Download the English tiny model locally with:

```sh
./scripts/download-whisper-model.sh
```

The downloader verifies the model's pinned SHA-256 digest before installation.

Transcribe a signed 16-bit PCM or 32-bit float WAV file with the bounded
diagnostic utility:

```sh
cargo run -p lcrt-stt-whisper --bin lcrt-whisper-transcribe -- \
  models/ggml-tiny.en.bin path/to/audio.wav en
```

A missing or invalid model produces an actionable error in the window. In
Offline Captions mode, LCRT does not download models and sends no audio to any
remote service.

</details>

## 📄 License

[MIT](LICENSE). The package's copyright file lists the license of every
third-party crate it links.

<div align="center">
<sub>Built with Rust, GTK4 and PipeWire.</sub>
</div>
