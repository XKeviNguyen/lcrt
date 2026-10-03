#!/usr/bin/env python3
"""Local-only bounded JSON-lines worker; never downloads or calls a service."""
import ctypes
import signal
import json
import os
from pathlib import Path
import sys
import unicodedata

# Linux adapter: terminate native inference even when GTK exits before cleanup.
expected_parent = int(sys.argv[2]) if len(sys.argv) > 2 else os.getppid()
if ctypes.CDLL(None, use_errno=True).prctl(1, signal.SIGKILL, 0, 0, 0) != 0:
    raise RuntimeError('Cannot bind translation worker lifetime')
if os.getppid() != expected_parent:
    sys.exit(1)

ROOT = Path(sys.argv[1])
sys.path.insert(0, str(ROOT / 'runtime'))
import ctranslate2
import sentencepiece

PAIRS = {'ja-en', 'en-ja', 'vi-en', 'en-vi'}
models = {}


def load(pair):
    if pair not in PAIRS:
        raise ValueError('Unsupported offline translation pair')
    if pair not in models:
        # Only active pairs remain resident. Two targets is the session limit.
        if len(models) >= 2:
            models.clear()
        path = ROOT / pair
        models[pair] = (
            ctranslate2.Translator(str(path), device='cpu', compute_type='int8',
                                  inter_threads=1, intra_threads=min(4, len(os.sched_getaffinity(0)))),
            sentencepiece.SentencePieceProcessor(model_file=str(path / 'source.spm')),
            sentencepiece.SentencePieceProcessor(model_file=str(path / 'target.spm')),
        )
    return models[pair]


def translate(pair, text):
    if not text.strip() or len(text.encode('utf-8')) > 2048:
        raise ValueError('Invalid translation chunk')
    translator, source, target = load(pair)
    tokens = source.encode(unicodedata.normalize('NFKC', text), out_type=str)
    if pair == 'en-vi':
        tokens.insert(0, '>>vie<<')
    result = translator.translate_batch([tokens], beam_size=1, max_input_length=256,
                                        max_decoding_length=256)[0]
    decoded = target.decode(result.hypotheses[0])
    return "".join(decoded.split()) if pair.endswith("-ja") else decoded


if __name__ == '__main__':
    for pair in sys.argv[3:]:
        load(pair)
    print(json.dumps({'ready': True}), flush=True)
    while True:
        line = sys.stdin.buffer.readline(8193)
        if not line:
            break
        try:
            if len(line) > 8192 or not line.endswith(b'\n'):
                raise ValueError('Oversized worker request')
            request = json.loads(line)
            text = translate(request['pair'], request['text'])
            print(json.dumps({'text': text}, ensure_ascii=False), flush=True)
        except Exception:
            # No paths, transcript, or dependency internals in user errors.
            print(json.dumps({'error': 'Local translation failed. Reinstall LILOPOP if this persists.'}), flush=True)
