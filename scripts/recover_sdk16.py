from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from urllib.request import urlopen


MANIFEST = json.loads(Path('.github/sdk16-original-artifacts.json').read_text())
REPOSITORY = MANIFEST['repository']
TAG = MANIFEST['tag']
RUN = MANIFEST['run_id']
FILES = {entry['filename']: entry for entry in MANIFEST['files']}


def api(path: str) -> dict:
    return json.loads(subprocess.check_output(['gh', 'api', f'repos/{REPOSITORY}/{path}']))


def verify_source() -> None:
    if os.environ['GITHUB_REPOSITORY'] != REPOSITORY:
        raise ValueError('Recovery repository differs from original build')
    run = api(f'actions/runs/{RUN}')
    if (run['event'], run['head_sha'], run['head_branch'], run['path'], run['run_attempt']) != (
        'push', MANIFEST['source_sha'], TAG, '.github/workflows/release-pypi.yml', MANIFEST['attempt']
    ):
        raise ValueError('Original release run identity changed')
    expected = {
        'build / verify-tag-on-main', 'build / Build sdist',
        'build / Build linux-aarch64 wheel', 'build / Build linux-x86_64 wheel',
        'build / Build macos-x86_64 wheel', 'build / Build macos-arm64 wheel',
    }
    successful = {job['name'] for job in api(f'actions/runs/{RUN}/jobs')['jobs'] if job['conclusion'] == 'success'}
    if not expected <= successful:
        raise ValueError('Original release builds did not all succeed')
    tag = api(f'git/ref/tags/{TAG}')['object']
    if (tag['type'], tag['sha']) != ('tag', MANIFEST['tag_object']):
        raise ValueError('Original annotated release tag changed')
    target = api(f"git/tags/{tag['sha']}")['object']
    if (target['type'], target['sha']) != ('commit', MANIFEST['source_sha']):
        raise ValueError('Original release source changed')
    actual = {item['id']: item for item in api(f'actions/runs/{RUN}/artifacts')['artifacts']}
    if set(actual) != {entry['id'] for entry in MANIFEST['artifacts']}:
        raise ValueError('Original artifact inventory changed')
    for entry in MANIFEST['artifacts']:
        item = actual[entry['id']]
        if (item['name'], item['digest'], item['expired']) != (entry['name'], entry['zip_digest'], False):
            raise ValueError('Original artifact identity changed')


def verify_files() -> None:
    paths = {path.name: path for path in Path('original-dist').iterdir()}
    if set(paths) != set(FILES):
        raise ValueError('Recovery distribution inventory differs from original build')
    for name, entry in FILES.items():
        path = paths[name]
        if path.is_symlink() or not path.is_file():
            raise ValueError('Recovery distribution is not a regular file')
        with path.open('rb') as stream:
            digest = hashlib.file_digest(stream, 'sha256').hexdigest()
        if (path.stat().st_size, digest) != (entry['size'], entry['sha256']):
            raise ValueError(f'Original distribution digest mismatch: {name}')


def registry() -> set[str]:
    with urlopen('https://pypi.org/pypi/cc-transcript/14.45.16/json') as response:
        urls = json.load(response)['urls']
    names = {entry['filename'] for entry in urls}
    if len(names) != len(urls) or not names <= FILES.keys():
        raise ValueError('PyPI release contains unexpected files')
    for entry in urls:
        expected = FILES[entry['filename']]
        if (entry['size'], entry['digests']['sha256']) != (expected['size'], expected['sha256']):
            raise ValueError('Published original distribution differs')
    present = {name for name, entry in FILES.items() if entry['publication'] == 'ALREADY_PRESENT_PRESERVE'}
    if not present <= names:
        raise ValueError('Original published Mac wheels are missing')
    return names


def prepare() -> None:
    verify_source()
    verify_files()
    missing = FILES.keys() - registry()
    destination = Path('missing-dist')
    destination.mkdir()
    for name in sorted(missing):
        shutil.copyfile(Path('original-dist') / name, destination / name)
    with Path(os.environ['GITHUB_OUTPUT']).open('a') as output:
        output.write(f'missing_count={len(missing)}\n')
    print(json.dumps({'original_run': RUN, 'source': MANIFEST['source_sha'], 'missing_files': sorted(missing)}))


def release() -> None:
    verify_source()
    verify_files()
    if registry() != FILES.keys():
        raise ValueError('PyPI release is incomplete')
    body = Path('recovery-provenance.txt')
    body.write_text(
        f'Original source: {MANIFEST["source_sha"]}\n\n'
        f'Original build: https://github.com/{REPOSITORY}/actions/runs/{RUN}\n\n'
        f'Artifact recovery: https://github.com/{REPOSITORY}/actions/runs/{os.environ["GITHUB_RUN_ID"]}\n\n'
        'Distribution bytes match the original build. Missing-file publish attestations identify the recovery run.\n\n'
        + '\n'.join(f'{entry["id"]}: {entry["zip_digest"]}' for entry in MANIFEST['artifacts']) + '\n'
    )
    subprocess.run([
        'gh', 'release', 'create', TAG, '--verify-tag', '--title', TAG,
        '--notes-file', str(body), '--latest', '--repo', REPOSITORY,
        *[str(Path('original-dist') / name) for name in sorted(FILES)],
    ], check=True)


match sys.argv[1]:
    case 'prepare':
        prepare()
    case 'release':
        release()
    case mode:
        raise ValueError(f'Unknown recovery mode: {mode}')
