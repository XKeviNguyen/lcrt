"""Focused integrity checks; no model downloads needed."""
import importlib.util
from pathlib import Path
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


if __name__ == '__main__':
    unittest.main()
