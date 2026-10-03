#!/usr/bin/env python3
"""Build-time only: verify pinned OPUS sources and wheels, convert to CPU int8.

No pip, runtime downloads, remote code, or unpinned dependencies.
Usage: scripts/prepare-translation.py [output] [artifact-cache]
"""
import hashlib
import json
import platform
from pathlib import Path
import shutil
import sys
import urllib.request
import zipfile

REPO = Path(__file__).resolve().parent.parent


def fetch(item, cache):
    path = cache / item['file']
    def valid():
        if not path.is_file() or path.stat().st_size != item['size']:
            return False
        with path.open('rb') as source:
            return hashlib.file_digest(source, 'sha256').hexdigest() == item['sha256']
    if not path.exists():
        temporary = path.with_suffix('.part')
        with urllib.request.urlopen(item['url'], timeout=60) as source, temporary.open('wb') as out:
            shutil.copyfileobj(source, out)
        temporary.replace(path)
    if not valid():
        raise RuntimeError(f"Checksum mismatch: {item['file']}")
    return path


def extract(path, destination):
    with zipfile.ZipFile(path) as archive:
        for member in archive.infolist():
            resolved = (destination / member.filename).resolve()
            if not resolved.is_relative_to(destination.resolve()):
                raise RuntimeError('Unsafe archive member')
        archive.extractall(destination)


def stage_bundle(output, destination):
    """Copy only manifest-owned assets; old cache entries cannot enter a package."""
    destination.mkdir(parents=True)
    models = json.loads((REPO / 'packaging/translation-models.json').read_text())['models']
    for name in ['runtime', *(item['pair'] for item in models)]:
        shutil.copytree(output / name, destination / name)
    shutil.copyfile(output / 'worker.py', destination / 'worker.py')


def main():
    output = Path(sys.argv[1]) if len(sys.argv) > 1 else REPO / 'target/share/lcrt/translation'
    cache = Path(sys.argv[2]) if len(sys.argv) > 2 else REPO / 'target/translation-artifacts'
    output.mkdir(parents=True, exist_ok=True)
    cache.mkdir(parents=True, exist_ok=True)
    if platform.machine() != 'x86_64':
        raise RuntimeError('Offline translation packaging currently supports Ubuntu AMD64')
    abi = f'cp{sys.version_info.major}{sys.version_info.minor}'
    wheels = json.loads((REPO / 'packaging/translation-runtime.json').read_text())['wheels']
    chosen = [wheel for wheel in wheels if f'-{abi}-{abi}-' in wheel['file']]
    if len(chosen) != 4:
        raise RuntimeError('Packaging supports Ubuntu AMD64 with Python 3.12 or 3.14')
    runtime = output / 'runtime'
    shutil.rmtree(runtime, ignore_errors=True)
    runtime.mkdir()
    for wheel in chosen:
        extract(fetch(wheel, cache), runtime)
    sys.path.insert(0, str(runtime))
    import ctranslate2
    for item in json.loads((REPO / 'packaging/translation-models.json').read_text())['models']:
        archive = fetch(item, cache)
        source = cache / (item['pair'] + '-source')
        shutil.rmtree(source, ignore_errors=True)
        source.mkdir()
        extract(archive, source)
        destination = output / item['pair']
        ctranslate2.converters.OpusMTConverter(str(source)).convert(
            str(destination), quantization='int8', force=True)
        for name in ['source.spm', 'target.spm', 'LICENSE', 'README.md']:
            shutil.copyfile(source / name, destination / name)
        print(f"{item['pair']}: verified and converted to int8", flush=True)
        shutil.rmtree(source)
    shutil.copyfile(REPO / 'scripts/offline-translate.py', output / 'worker.py')


if __name__ == '__main__':
    main()
