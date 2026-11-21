"""Pack immutable source or a verified development artifact. Runs locally only."""
import argparse
import hashlib
import json
from pathlib import Path
import stat
import subprocess
import sys
import zipfile

ROOT = Path(__file__).resolve().parents[1]
SDK_PIN = '1a11afc14693cc6463e0b95ae7d61991b4a60427'
TARGETS = ('x86_64-pc-windows-msvc', 'aarch64-apple-darwin')


def sha(data):
    return hashlib.sha256(data).hexdigest()


def digest_blob(data):
    return hashlib.sha1(f'blob {len(data)}\0'.encode() + data).hexdigest()


def files_under(directory):
    for path in sorted((ROOT / directory).rglob('*')):
        assert not path.is_symlink(), path
        if path.is_file() and '__pycache__' not in path.parts:
            yield path.relative_to(ROOT).as_posix()


def validate_source(manifest):
    provenance = json.loads((ROOT / 'UPSTREAM.json').read_bytes())
    source = json.loads((ROOT / 'upstream/package.json').read_bytes())
    assert manifest['platforms'] == ['macOS', 'Windows']
    assert manifest['minHostVersion'] == '0.3.0-beta.12'
    assert manifest['artifacts'] == manifest['files'] == {}
    assert manifest['repository'] == 'https://github.com/almatkai/obsidian'
    assert manifest['preferences'] == source.get('preferences', [])
    assert len(manifest['commands']) == len(source['commands']) == 13
    for installed, original in zip(manifest['commands'], source['commands'], strict=True):
        assert installed == {**original, 'platforms': original.get('platforms', source['platforms']), 'runtime': 'rust',
                             'concurrency': 'serialize', 'refreshMs': None}
    expected_paths = {name for name in provenance['gitBlobs'] if name != '../LICENSE'}
    assert {path.removeprefix('upstream/') for path in files_under('upstream')} == expected_paths
    for path, blob in provenance['gitBlobs'].items():
        local = ROOT / ('LICENSE' if path == '../LICENSE' else f'upstream/{path}')
        data = local.read_bytes()
        assert digest_blob(data) == blob['sha1'] and len(data) == blob['bytes'] and blob['mode'] == '100644', local
    resources = list(files_under('resources'))
    assert {Path(path).name for path in resources} == {path.name for path in (ROOT / 'upstream/assets').iterdir()}
    assert manifest['icon'] in resources
    for path in resources:
        assert (ROOT / path).read_bytes() == (ROOT / 'upstream/assets' / Path(path).name).read_bytes()
    assert (ROOT / 'licenses/UPSTREAM-OBSIDIAN-LICENSE').read_bytes() == (ROOT / 'upstream/LICENSE').read_bytes()
    assert f'#{SDK_PIN}"' in (ROOT / 'Cargo.lock').read_text(encoding='utf-8')
    subprocess.run([sys.executable, str(ROOT / 'scripts/dependency-licenses.py'), '--check'],
                   cwd=ROOT, check=True)


def validate_architecture(data, windows):
    if windows:
        if not (data[:2] == b'MZ' and len(data) >= 64):
            raise ValueError('Expected Windows PE')
        offset = int.from_bytes(data[60:64], 'little')
        if not (64 <= offset < len(data) - 6 and data[offset:offset+6] == b'PE\0\0d\x86'):
            raise ValueError('Expected PE x64')
    else:
        if not (len(data) >= 32 and data[:4] == bytes.fromhex('cffaedfe') and int.from_bytes(data[4:8], 'little') == 0x0100000c):
            raise ValueError('Expected ARM64 Mach-O')


def write_zip(path, content, executable=None):
    with zipfile.ZipFile(path, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in sorted(content.items()):
            entry = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            entry.create_system = 3
            entry.external_attr = (stat.S_IFREG | (0o755 if name == executable else 0o644)) << 16
            entry.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(entry, data)
    with zipfile.ZipFile(path) as archive:
        assert sorted(archive.namelist()) == sorted(content)
        for name, data in content.items():
            assert sha(archive.read(name)) == sha(data)
            assert stat.S_IMODE(archive.getinfo(name).external_attr >> 16) == (0o755 if name == executable else 0o644)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--source', action='store_true')
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--target', choices=TARGETS)
    parser.add_argument('--output', type=Path, default=ROOT / 'dist')
    args = parser.parse_args()
    if args.source and (args.binary or args.target) or not args.source and not (args.binary and args.target):
        parser.error('Choose --source or supply both --binary and --target')
    canonical = (ROOT / 'extension.json').read_bytes()
    manifest = json.loads(canonical)
    validate_source(manifest)
    names = ['LICENSE', 'NOTICE', 'UPSTREAM.json', *files_under('resources'), *files_under('licenses')]
    payload = {name: (ROOT / name).read_bytes() for name in names}
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if args.source:
        names = ['.gitignore', '.gitattributes', 'Cargo.toml', 'Cargo.lock', 'README.md', 'PORT-REPORT.md', 'STORE-RECORD.json',
                 *files_under('scripts'), *files_under('src'), *files_under('tests'), *files_under('upstream')]
        payload.update({name: (ROOT / name).read_bytes() for name in names})
        payload['extension.json'] = canonical
        executable = None
        archive = output / 'obsidian-portable-source.zip'
        report = dict(source=True, sdkCommit=SDK_PIN, canonicalManifestSha256=sha(canonical),
                      files={name: sha(data) for name, data in sorted(payload.items())})
    else:
        binary = args.binary.resolve()
        windows = args.target.startswith('x86_64-pc-windows')
        executable = 'bin/tezbar-obsidian' + ('.exe' if windows else '')
        payload[executable] = binary.read_bytes()
        validate_architecture(payload[executable], windows)
        info = json.loads(subprocess.check_output([str(binary), '--protocol-info'], timeout=15))
        assert (info['id'], info['version'], info['protocol']) == (manifest['id'], manifest['version'], 1)
        assert info['commands'] == [command['name'] for command in manifest['commands']]
        if info['fixtureSupport'] is not False:
            raise ValueError('Build development packages with fixture support disabled')
        manifest['files'] = {name: sha(data) for name, data in sorted(payload.items())}
        payload['extension.json'] = (json.dumps(manifest, ensure_ascii=False, indent=2) + '\n').encode()
        archive = output / f'obsidian-{args.target}-development.zip'
        report = dict(target=args.target, executable=executable, executableSha256=sha(payload[executable]),
                      files=manifest['files'], developmentOnly=True, platformSupported=True,
                      canonicalManifestSha256=sha(canonical), limitation=(
                          'Local development artifact. Live Obsidian and GPUI acceptance remain pending.'))
    write_zip(archive, payload, executable)
    assert (ROOT / 'extension.json').read_bytes() == canonical
    report.update(archive=archive.name, sha256=sha(archive.read_bytes()))
    archive.with_suffix('.zip.sha256').write_bytes(f"{report['sha256']}  {archive.name}\n".encode())
    archive.with_suffix('.zip.artifact.json').write_bytes((json.dumps(report, indent=2) + '\n').encode())
    print(json.dumps({key: value for key, value in report.items() if key != 'files'}, indent=2))


if __name__ == '__main__':
    main()
