#!/usr/bin/env python3
"""Fetches the models listed in packaging/models.json and verifies them.

Usage: scripts/fetch-models.py [directory]

Files land in `directory` (default: target/share/lcrt/models, which is where
a development build of LCRT looks for its built-in model). A file already
there is reused only if its SHA-256 matches the manifest; otherwise it is
downloaded again. A download whose size or SHA-256 differs from the
manifest is discarded and the script fails: there is no warning-and-continue.
"""
import hashlib
import json
import os
import sys
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CHUNK = 1 << 20


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as file:
        for block in iter(lambda: file.read(CHUNK), b''):
            digest.update(block)
    return digest.hexdigest()


def download(model, path):
    partial = path + '.part'
    digest = hashlib.sha256()
    size = 0
    # urllib verifies TLS certificates and host names by default.
    with urllib.request.urlopen(model['url'], timeout=60) as response, open(partial, 'wb') as out:
        for block in iter(lambda: response.read(CHUNK), b''):
            digest.update(block)
            size += len(block)
            if size > model['size']:
                break
            out.write(block)
    if size != model['size'] or digest.hexdigest() != model['sha256']:
        os.remove(partial)
        raise SystemExit(
            f"{model['id']}: download does not match the manifest "
            f"({size} bytes, sha256 {digest.hexdigest()}); refusing to use it")
    os.replace(partial, path)


def main():
    directory = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, 'target/share/lcrt/models')
    with open(os.path.join(ROOT, 'packaging/models.json')) as file:
        models = json.load(file)['models']
    os.makedirs(directory, exist_ok=True)
    for model in models:
        path = os.path.join(directory, model['file'])
        if os.path.isfile(path):
            if sha256_of(path) == model['sha256']:
                print(f"{model['id']}: verified {path}")
                continue
            print(f"{model['id']}: {path} does not match the manifest; downloading again",
                  file=sys.stderr)
        download(model, path)
        print(f"{model['id']}: downloaded and verified {path}")


if __name__ == '__main__':
    main()
