# LCRT privacy

LCRT has no telemetry, analytics or crash reporting. It never uploads usage
data or hardware information. It does not save audio or transcripts to disk.

What leaves your computer depends only on the mode and features you choose.

## Offline Captions

Audio is processed on this device. Audio and transcription stay on this
device.

The speech model (Whisper base, multilingual) is a file installed with LCRT.
LCRT never downloads a model, checks for model updates or contacts any
service to caption offline. In this mode it opens no network connection at
all: the acceptance runs record every socket the app opens, with the network
removed from its process, and found none (see
[V2_ACCEPTANCE.md](V2_ACCEPTANCE.md)).

Offline Captions never fall back to an online service. If the model is
missing or damaged, LCRT says so and does not start.

There is no Offline Translation yet; Translation uses OpenAI, as described
below.

## Online Captions

Audio is streamed to OpenAI for processing, and API charges may apply to your
OpenAI account.

While a session runs, LCRT streams audio from the selected source to OpenAI's
Realtime transcription service over an encrypted connection. It sends audio
only while it detects speech, plus a moment of lead-in (300 ms) and trailing
silence (up to 700 ms). If you pick a spoken language, it is sent as a hint.

## Translation

Audio is streamed to OpenAI for processing, and API charges may apply to your
OpenAI account.

While a session runs, LCRT streams all audio from the selected source,
including silence, to OpenAI's realtime translation service. The service needs
a continuous stream to translate with low delay. The target language is sent
with it. LCRT ignores the translated speech audio that the service returns.

You can translate into two languages at once. The service accepts one target
language per session, so LCRT then opens two sessions and sends the same audio
to each. API charges apply for each session. The window says so whenever two
targets are selected. LCRT never opens more sessions than targets, and never
more than two.

Hiding a lane with its language chip changes only what you see: that
language's session keeps receiving audio. The service also transcribes the
original speech while its lane is hidden, so the lane can be shown again at
once. **Pause translation** in the chip's
menu closes the session, so no more audio goes to it until you resume it, and
**Remove language** closes it for good.

## Vocabulary explanations

When Vocabulary is on and you select caption text, LCRT sends the following to
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

LCRT uses the first key available from these sources:

1. **A key entered in Settings.** Choose **Save securely** to store it in your
   desktop keyring (Secret Service, for example GNOME Keyring). If no keyring
   is available, LCRT keeps the key in memory until it quits and tells you so.
   It never falls back to a plain file.
2. **The `OPENAI_API_KEY` environment variable.** LCRT uses it if no key has
   been entered or saved. Settings shows "Using environment credential" and
   never displays the value.

The key is sent only to `api.openai.com`, in the `Authorization` header over
TLS with certificate validation. LCRT never writes the key to its preferences
file, logs, command lines or URLs. **Clear** removes the saved key from the
keyring. If the keyring can't be reached, Clear says so, and the key stays
saved until you try again.

## Files LCRT writes

LCRT writes one file, `~/.config/lcrt/preferences.json`, readable only by
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
policies and your agreement with OpenAI. LCRT sends them from your computer
directly to OpenAI, with no intermediary server.
