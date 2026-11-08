import importlib.util
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

spec = importlib.util.spec_from_file_location('release', Path(__file__).resolve().parents[1] / 'scripts/release.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def test_archives_checksums_and_complete_target_set(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'Cargo.toml').write_text('[package]\nversion = "0.1.0"\n')
            for file in ('LICENSE', 'README.md', 'SECURITY.md'):
                (root / file).write_text(file)
            for target in release.TARGETS:
                exe = 'cc-sakura.exe' if 'windows' in target else 'cc-sakura'
                binary = root / 'target' / target / 'release' / exe
                binary.parent.mkdir(parents=True)
                binary.write_bytes(b'synthetic binary')
                binary.chmod(0o755)
                archive = release.package(root, target, root / 'dist', 'v0.1.0')
                if 'windows' in target:
                    with zipfile.ZipFile(archive) as bundle:
                        names = bundle.namelist()
                else:
                    with tarfile.open(archive) as bundle:
                        names = bundle.getnames()
                        self.assertEqual(bundle.getmember(names[0]).mode & 0o111, 0o111)
                self.assertEqual({Path(name).name for name in names}, {exe, 'LICENSE', 'README.md', 'SECURITY.md'})
            release.manifest(root, root / 'dist', 'v0.1.0')
            self.assertEqual(len((root / 'dist/SHA256SUMS').read_text().splitlines()), 5)
            (root / 'dist/SHA256SUMS').unlink()
            next((root / 'dist').glob('*.sha256')).write_text('tampered')
            with self.assertRaises(ValueError):
                release.manifest(root, root / 'dist', 'v0.1.0')

    def test_rejects_wrong_tag_and_incomplete_artifacts(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'Cargo.toml').write_text('[package]\nversion = "0.1.0"\n')
            with self.assertRaises(ValueError):
                release.version(root, 'v0.2.0')
            (root / 'dist').mkdir()
            with self.assertRaises(ValueError):
                release.manifest(root, root / 'dist', 'v0.1.0')


if __name__ == '__main__':
    unittest.main()
