# LCRT

LCRT is a native desktop app for live captions and real-time translation. It
captions whatever your computer is playing (system audio) or your microphone,
in a small window that stays out of the way.

- **Offline Captions:** a local Whisper model; audio never leaves the device.
- **Online Captions:** OpenAI realtime transcription for English, Japanese,
  Vietnamese and more.
- **Translation:** OpenAI realtime translation. You can show the original
  speech above the translation.
- **Vocabulary:** select a word or phrase in the captions to see its meaning in
  context.
- **Appearance:** font, size, text and background colors, transparency and
  window size are remembered between runs.

Online features use your own OpenAI API key, and API charges may apply to your
OpenAI account. LCRT has no telemetry. [docs/PRIVACY.md](docs/PRIVACY.md) lists
exactly what is sent, when, and where your key is stored.

Primary platform: Ubuntu AMD64 (PipeWire, GTK4, libadwaita, Wayland or X11).
Ubuntu ARM64 and Windows 10/11 are portability targets.

## Install on Ubuntu

The Debian package is built for the Ubuntu release it is built on. It needs
`libgtk4-layer-shell0`, which Ubuntu packages from 24.10 onward; Ubuntu 26.04
LTS is the tested release.

```sh
scripts/build-deb.sh            # prints target/debian/lcrt_2.0.0_amd64.deb
sudo apt install ./target/debian/lcrt_2.0.0_amd64.deb
```

Then open **LCRT Live Captions** from the app grid, or run `lcrt`.

## Use

1. Choose a mode, an audio source, and for online modes a language:
   - Offline Captions and Online Captions: the spoken language (or Auto).
   - Translation: the target language. The spoken language is detected
     automatically.
2. Press **Start**. Captions update as speech is recognized; **Stop** finishes
   the last sentence and keeps the text on screen.
3. Open **Settings** to:
   - choose the Whisper model for Offline Captions;
   - enter your OpenAI API key;
   - adjust appearance and vocabulary.

System audio sources are listed as **System audio**, microphones as
**Microphone**. LCRT remembers the last mode, source and languages.

### Offline model

LCRT does not download models by itself. Download the checksum-verified tiny
English model and choose it in **Settings → General**:

```sh
./scripts/download-whisper-model.sh     # saves models/ggml-tiny.en.bin
```

### OpenAI API key

Enter the key in **Settings → Online** and choose **Save securely** to store it
in the desktop keyring (GNOME Keyring or another Secret Service provider).
**Test connection** checks the key. If no keyring is available, the key is kept
only until LCRT quits. As a fallback, LCRT reads `OPENAI_API_KEY` from its
environment and never displays it. The key is never written to a file or log.

### Window behavior

On Wayland compositors that support layer-shell protocol v4 or newer, the
caption window is pinned near the bottom of the screen above other windows.
GNOME Wayland, X11 and older compositors use a standard window: transparency
still works, but LCRT cannot keep it on top.

### Limitations

- Audio sources are discovered at launch.
- Offline accuracy and speed depend on the model and CPU. The tiny model is
  English-focused.
- Online modes need a network connection. LCRT reconnects a few times after a
  brief drop, then reports the problem. It never switches to another backend
  or to a paid service on its own.

## Development

The checked-in `rust-toolchain.toml` pins the primary development and CI
toolchain to Rust 1.98.0 with the `rustfmt` and `clippy` components. This is
separate from the workspace's declared Rust 1.85 MSRV in `Cargo.toml`.

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

For source IDs, and a bounded diagnostic run that starts captions itself and
closes after the given time:

```sh
cargo run -p lcrt-app --bin lcrt -- --list-sources
cargo run -p lcrt-app --bin lcrt -- \
  --model models/ggml-tiny.en.bin --smoke-source SOURCE_ID --smoke-seconds 10
```

`--smoke-mode online|translation` runs the same diagnostic against OpenAI and
uses the saved or `OPENAI_API_KEY` key. Closing the window cancels the session
without waiting on a blocked worker, so a stuck native call cannot delay exit.

### Linux audio development

PipeWire capture development requires `libpipewire-0.3-dev`,
`libspa-0.2-dev`, and `pkg-config`. The bounded diagnostic utility enumerates
both microphone sources and system-output monitor targets:

```sh
cargo run -p lcrt-audio-pipewire --bin lcrt-pw-capture -- list
cargo run -p lcrt-audio-pipewire --bin lcrt-pw-capture -- capture <source-id> 3
```

The capture duration is clamped to 1–30 seconds. The utility reports negotiated
format and aggregate sample statistics; it neither records audio to disk nor
silently substitutes synthetic audio when PipeWire fails.

### Native caption UI development

The Ubuntu window uses GTK4 and libadwaita. Install `libgtk-4-dev` and
`libadwaita-1-dev`, then launch its incremental-caption demonstration with:

```sh
cargo run -p lcrt-ui-gtk --bin lcrt-caption-ui
```

The demo drives the same window with scripted partial and final captions. Its
`--smoke-test` mode injects deterministic updates and closes itself; it does
not exercise audio capture or online services.

### Local Whisper development

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
