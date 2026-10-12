"""Fail closed before building a native runtime from changed inputs."""
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


class NativeIntegrityTests(unittest.TestCase):
    def rejected_build(self, change_patch):
        repo = Path(__file__).resolve().parent.parent
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ['scripts/build-sherpa-native.sh',
                         'packaging/native/runtime.json',
                         'packaging/native/sherpa-initial-context.patch']:
                target = root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(repo / name, target)
            native = root / 'native'
            native.mkdir()
            (native / 'source.tar.gz').write_bytes(b'tampered source')
            if change_patch:
                (root / 'packaging/native/sherpa-initial-context.patch').write_bytes(
                    b'tampered patch')
            result = subprocess.run(['bash', str(root / 'scripts/build-sherpa-native.sh'),
                                     str(native)], capture_output=True, text=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((native / 'build').exists())
            return result.stdout + result.stderr

    def test_changed_patch_is_rejected_before_native_build(self):
        self.assertIn('Native patch does not match its pinned SHA-256',
                      self.rejected_build(True))

    def test_cached_changed_source_is_rejected_before_extraction(self):
        self.assertIn('FAILED', self.rejected_build(False))


if __name__ == '__main__':
    unittest.main()
