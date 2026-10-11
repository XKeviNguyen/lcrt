"""Recompute short-clip edit counts and predefined useful-phrase timings."""
import json
from pathlib import Path
import unicodedata


def normalize(text):
    return ''.join(c if c.isalnum() else ' ' for c in
                   unicodedata.normalize('NFKC', text).casefold()).split()


def distance(expected, actual):
    row = list(range(len(actual) + 1))
    for i, left in enumerate(expected, 1):
        next_row = [i]
        for j, right in enumerate(actual, 1):
            next_row.append(min(row[j] + 1, next_row[j - 1] + 1,
                                row[j - 1] + (left != right)))
        row = next_row
    return row[-1]


data = json.loads((Path(__file__).resolve().parents[2] /
                   'docs/evidence/streaming-ja-vi/ab.json').read_text())
for run in data['runs']:
    reference = normalize(run['reference'])
    if run['clip'].startswith('ja'):
        reference = ''.join(reference)
    phrase = ''.join(normalize(run['predefined_useful_phrase']))
    for name, result in run['engines'].items():
        final = result.get('transcript', ' '.join(result.get('final_segments', [])))
        actual = normalize(final)
        if run['clip'].startswith('ja'):
            actual = ''.join(actual)
        first = next((round(c['ms'], 1) for c in result['changes']
                      if phrase in ''.join(normalize(c['text']))), None)
        print(run['clip'], name, 'edits=', distance(reference, actual),
              'reference_units=', len(reference), 'useful_ms=', first,
              result['resource'])
