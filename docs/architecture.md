# Application Architecture

LILOPOP keeps portable application behavior in `lcrt-core` and implements host
technology behind small adapter boundaries.

The V1 data path is:

```text
platform audio adapter -> AudioCapture -> CaptionPipeline -> Transcriber
                                              |
                                         CaptionState
                                              |
                                          CaptionSink -> native UI adapter
```

## Portable core

- `audio`: validated PCM chunks, source metadata, and the bounded capture port.
- `transcription`: incremental/final transcript updates and the replaceable STT
  port.
- `caption`: monotonically revisioned caption state and immutable UI snapshots.
- `ui`: a non-blocking native presentation sink boundary.
- `config`: explicit pipeline limits and shutdown responsiveness settings.
- `pipeline`: lifecycle, error propagation, structured tracing, and guaranteed
  audio-stop attempts.

The core contains no PipeWire, GTK, Wayland, WASAPI, or speech-engine APIs.
CI compile-checks this crate for Ubuntu ARM64 and Windows x64 targets. These
checks prove portable Rust type-checking only: they do not link a distributable
application, exercise platform adapters, or validate runtime behavior.

## Platform adapters

Linux V1 adapters own PipeWire capture, local whisper.cpp integration, and
GTK4/libadwaita presentation. The `lcrt-app` binary enumerates sources and owns
only lifecycle orchestration: a controller thread starts a cancellable pipeline
worker while the GTK main thread remains dedicated to presentation. Adapters
translate native failures into actionable typed boundary errors. Windows
adapters can later implement the same ports with WASAPI and a native Windows UI
without changing caption domain logic.

Audio adapters must bound their queues and honor the requested poll timeout.
Transcription work must not run on a UI thread. The GTK sink coalesces partial
captions into replaceable state, retains at most one unobserved final across an
utterance rollover, and keeps lifecycle/error state independent of caption
pressure. It performs no speech inference.

On Stop, the portable pipeline ends PipeWire capture before flushing the final
STT window. This prevents new audio from filling queues during inference while
still allowing the last buffered utterance to become a final caption.

The Linux UI selects overlay placement by compositor capability. When the
Wayland compositor advertises `zwlr_layer_shell_v1` protocol v4 or newer, the
window uses the overlay layer, a bottom anchor, no exclusive zone, and on-demand
keyboard focus. Explicit width and height controls preserve resizing for layer
surfaces, which have no compositor resize handles. Other compositors retain the
same translucent GTK presentation as a normal window and surface that
limitation to the user instead of claiming always-on-top behavior.

## Deferred lifecycle work

The application controller cancels an active pipeline and detaches its worker
when the window closes, so UI closure never waits on native inference. A worker
permanently blocked in model loading or inference cannot be force-terminated
safely; cancellable model loading is a later STT lifecycle milestone.

Also deferred: PipeWire source refresh, capture-error final transcript
flushing, V2 translation, and packaging.

### Local translation

Offline Translation wraps the Whisper adapter with an independent CPU worker
using CTranslate2 and int8 Helsinki OPUS-MT models. Source updates return
immediately. One replaceable pending speech window and one in-flight window
bound work; the worker does not retranslate accumulated history. Live target
changes use the existing target control and revision numbers to discard results
from before a pause, removal, or resume. Stop kills and reaps the local child.
The Rust adapter bounds requests to 2 KiB, responses to 8 KiB, and inference to
10 seconds. Final translation drain is limited to one second.

The package contains the worker, verified runtime wheels, and four converted
models. Conversion and external retrieval happen only at build time. Offline
paths never construct an online transcriber, and vocabulary cloud lookups are
disabled while an offline session is active.
