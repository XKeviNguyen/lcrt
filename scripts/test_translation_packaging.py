"""Focused integrity checks; no model downloads needed."""
import importlib.util
from pathlib import Path
import json
import os
import signal
import subprocess
import sys
import time
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location('prepare', Path(__file__).with_name('prepare-translation.py'))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class IntegrityTests(unittest.TestCase):
    def test_cached_checksum_mismatch_is_fatal(self):
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            (cache / 'weights.zip').write_bytes(b'bad')
            item = {'file': 'weights.zip', 'size': 3, 'sha256': '0' * 64, 'url': 'must-not-download'}
            with self.assertRaisesRegex(RuntimeError, 'Checksum mismatch'):
                prepare.fetch(item, cache)

    def test_archive_cannot_write_outside_the_destination(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / 'unsafe.zip'
            with zipfile.ZipFile(archive, 'w') as output:
                output.writestr('../escaped.txt', 'untrusted')
            with self.assertRaisesRegex(RuntimeError, 'Unsafe archive'):
                prepare.extract(archive, root / 'models')
            self.assertFalse((root / 'escaped.txt').exists())

    def test_staging_excludes_obsolete_assets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / 'output'
            output.mkdir()
            names = ['runtime', *(item['pair'] for item in json.loads(
                (prepare.REPO / 'packaging/translation-models.json').read_text())['models'])]
            for name in names + ['obsolete-pair']:
                (output / name).mkdir()
                (output / name / 'asset').write_text('fixture')
            (output / 'worker.py').write_text('fixture')
            (output / 'obsolete.bin').write_text('fixture')
            prepare.stage_bundle(output, root / 'stage')
            self.assertEqual(set(path.name for path in (root / 'stage').iterdir()),
                             set(names + ['worker.py']))

    def worker_root(self, directory):
        root = Path(directory)
        runtime = root / 'runtime'
        runtime.mkdir()
        (runtime / 'ctranslate2.py').write_text(
            "class Translator:\n def __init__(self,*a,**kw): raise RuntimeError('corrupt model')\n")
        (runtime / 'sentencepiece.py').write_text('')
        return root

    def test_corrupt_selected_model_never_reports_ready(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.worker_root(directory)
            result = subprocess.run([sys.executable, '-I',
                str(prepare.REPO / 'scripts/offline-translate.py'), str(root),
                str(os.getpid()), 'ja-en'], capture_output=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn(b'ready', result.stdout)

    def test_worker_dies_when_its_parent_exits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.worker_root(directory)
            code = """import os,subprocess,sys
child=subprocess.Popen([sys.executable,'-I',sys.argv[1],sys.argv[2],str(os.getpid())],stdin=subprocess.PIPE,stdout=subprocess.PIPE)
assert b'ready' in child.stdout.readline()
print(child.pid,flush=True)
sys.stdin.readline()
"""
            parent = subprocess.Popen([sys.executable, '-c', code,
                str(prepare.REPO / 'scripts/offline-translate.py'), str(root)],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            child = int(parent.stdout.readline())
            try:
                parent.communicate(timeout=5)
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline:
                    status = Path(f'/proc/{child}/stat')
                    if not status.exists() or status.read_text().split()[2] == 'Z':
                        break
                    time.sleep(.01)
                else:
                    self.fail('Native translation child survived parent exit')
            finally:
                try:
                    os.kill(child, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                parent.kill()
                parent.wait()


if __name__ == '__main__':
    unittest.main()
