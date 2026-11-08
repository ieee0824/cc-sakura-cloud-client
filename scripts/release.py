"""Package native client builds and verify all release assets before uploading."""
import argparse
import hashlib
import re
import tarfile
import tomllib
import zipfile
from pathlib import Path

TARGETS = {
    'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu',
    'x86_64-apple-darwin', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc',
}


def version(root, tag):
    value = tomllib.loads((root / 'Cargo.toml').read_text())['package']['version']
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?', value):
        raise ValueError('unsupported package version')
    if tag and tag != f'v{value}':
        raise ValueError('release tag must equal v<Cargo.toml package version>')
    return f'v{value}'


def package(root, target, output, tag=None):
    if target not in TARGETS:
        raise ValueError('unsupported target')
    name = f'cc-sakura-cloud-client-{version(root, tag)}-{target}'
    executable = 'cc-sakura.exe' if 'windows' in target else 'cc-sakura'
    files = [(root / 'target' / target / 'release' / executable, executable)]
    files += [(root / file, file) for file in ('LICENSE', 'README.md', 'SECURITY.md')]
    if not all(path.is_file() and not path.is_symlink() for path, _ in files):
        raise ValueError('missing or symlinked release input')
    output.mkdir(parents=True, exist_ok=True)
    archive = output / (name + ('.zip' if 'windows' in target else '.tar.gz'))
    if 'windows' in target:
        with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as bundle:
            for path, filename in files:
                bundle.write(path, f'{name}/{filename}')
    else:
        with tarfile.open(archive, 'w:gz') as bundle:
            for path, filename in files:
                bundle.add(path, arcname=f'{name}/{filename}', recursive=False)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + '.sha256').write_text(f'{digest}  {archive.name}\n')
    return archive


def manifest(root, output, tag):
    release_version = version(root, tag)
    expected = {
        f'cc-sakura-cloud-client-{release_version}-{target}' + ('.zip' if 'windows' in target else '.tar.gz')
        for target in TARGETS
    }
    actual = {path.name for path in output.iterdir()}
    if actual != expected | {name + '.sha256' for name in expected}:
        raise ValueError('release artifacts are incomplete or contain unexpected files')
    lines = []
    for name in sorted(expected):
        archive = output / name
        if archive.is_symlink() or (output / (name + '.sha256')).is_symlink():
            raise ValueError('symlinked release artifact')
        line = f'{hashlib.sha256(archive.read_bytes()).hexdigest()}  {name}\n'
        if (output / (name + '.sha256')).read_text() != line:
            raise ValueError('release checksum mismatch')
        lines.append(line)
    (output / 'SHA256SUMS').write_text(''.join(lines))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('mode', choices=['validate', 'package', 'manifest'])
    parser.add_argument('--target')
    parser.add_argument('--tag')
    args = parser.parse_args()
    project = Path(__file__).resolve().parent.parent
    if args.mode == 'validate':
        print(version(project, args.tag))
    elif args.mode == 'package':
        print(package(project, args.target, project / 'dist', args.tag))
    else:
        manifest(project, project / 'dist', args.tag)
