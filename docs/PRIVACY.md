# LILOPOP privacy

LILOPOP has no telemetry, analytics or crash reporting. It never uploads usage
data or hardware information. It does not save audio or transcripts to disk.

What leaves your computer depends only on the mode and features you choose.

## Offline Captions

Audio is processed on this device. Audio and transcription stay on this
device.

The speech model (Whisper Tiny, multilingual) is a file installed with LILOPOP.
LILOPOP never downloads a model, checks for model updates or contacts any
service to caption offline. In this mode it opens no network connection at
all: the acceptance runs record every socket the app opens, with the network
removed from its process, and found none (see
[SHIP_FAST_ACCEPTANCE.md](SHIP_FAST_ACCEPTANCE.md)).

Offline Captions never fall back to an online service. If the model is
missing or damaged, LILOPOP says so and does not start.

## Offline Translation

Speech is recognized by bundled Whisper Tiny multilingual and translated by
local CTranslate2 / OPUS-MT models. Japanese↔English and Vietnamese↔English
are supported. No key, network access, runtime model download or telemetry is
used. Unsupported pairs are rejected; there is no automatic online fallback.
Vocabulary explanations are disabled in both offline modes.

## Online Captions

Audio is streamed to OpenAI for processing, and API charges may apply to your
OpenAI account.

While a session runs, LILOPOP streams audio from the selected source to OpenAI's
Realtime transcription service over an encrypted connection. It sends audio
only while it detects speech, plus a moment of lead-in (300 ms) and trailing
silence (up to 700 ms). If you pick a spoken language, it is sent as a hint.

## Online Translation

Audio is streamed to OpenAI for processing, and API charges may apply to your
OpenAI account.

While a session runs, LILOPOP streams all audio from the selected source,
including silence, to OpenAI's realtime translation service. The service needs
a continuous stream to translate with low delay. The target language is sent
with it. LILOPOP ignores the translated speech audio that the service returns.

You can translate into two languages at once. The service accepts one target
language per session, so LILOPOP then opens two sessions and sends the same audio
to each. API charges apply for each session. The window says so whenever two
targets are selected. LILOPOP never opens more sessions than targets, and never
more than two.

Hiding a lane with its language chip changes only what you see: that
language's session keeps receiving audio. The service also transcribes the
original speech while its lane is hidden, so the lane can be shown again at
once. **Pause translation** in the chip's
menu closes the session, so no more audio goes to it until you resume it, and
**Remove language** closes it for good.

## Vocabulary explanations

In online modes, when Vocabulary is on and you select caption text, LILOPOP sends the following to
OpenAI's Responses API with `store: false`:

- the selected text (at most 200 characters);
- up to 160 characters of surrounding caption on each side;
- the explanation language.

Answers are cached in memory for the current run only. When Vocabulary is off,
selecting text sends nothing.

## Test connection

Test connection sends one request to list OpenAI models, which checks that
the key works. No audio or text is sent.

## Your OpenAI API key

LILOPOP uses the first key available from these sources:

1. **A key entered in Settings.** Choose **Save securely** to store it in your
   desktop keyring (Secret Service, for example GNOME Keyring). If no keyring
   is available, LILOPOP keeps the key in memory until it quits and tells you so.
   It never falls back to a plain file.
2. **The `OPENAI_API_KEY` environment variable.** LILOPOP uses it if no key has
   been entered or saved. Settings shows "Using environment credential" and
   never displays the value.

The key is sent only to `api.openai.com`, in the `Authorization` header over
TLS with certificate validation. LILOPOP never writes the key to its preferences
file, logs, command lines or URLs. **Clear** removes the saved key from the
keyring. If the keyring can't be reached, Clear says so, and the key stays
saved until you try again.

## Files LILOPOP writes

LILOPOP writes one file, `~/.config/lcrt/preferences.json`, readable only by
you. It holds these settings:

- mode, audio source and languages;
- a custom model path, if you chose one;
- which translation lanes you hid;
- appearance;
- vocabulary settings.

It contains no key, audio or transcript text.

Diagnostic messages go to standard error only. They never contain audio,
transcript text, selected text or credentials.

## Data handled by OpenAI

Audio and text sent to OpenAI are governed by OpenAI's API data usage
policies and your agreement with OpenAI. LILOPOP sends them from your computer
directly to OpenAI, with no intermediary server.
