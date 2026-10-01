#!/usr/bin/env python3
"""Release-CI candidate: bind registry payloads to the checked-out release tag.

Install this proposed helper as scripts/check-registry-release.py with the
workflow. It reads Cargo's already downloaded, checksum-validated archives;
it performs no resolution, compilation, installation, or network request.
"""
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tarfile
import tomllib


def members(crate, version, cargo_home):
    archives = list((cargo_home / 'registry/cache').glob(f'*/{crate}-{version}.crate'))
    if len(archives) != 1:
        raise RuntimeError(f'expected one registry archive for {crate} {version}: {archives}')
    prefix = f'{crate}-{version}/'
    result = {}
    with tarfile.open(archives[0], 'r:gz') as archive:
        for member in archive:
            if member.isdir():
                continue
            if not member.isfile() or not member.name.startswith(prefix):
                raise RuntimeError(f'unsupported archive member {member.name}')
            relative = member.name[len(prefix):]
            path = PurePosixPath(relative)
            if path.is_absolute() or '..' in path.parts or relative in result:
                raise RuntimeError(f'unsafe or duplicate archive path {relative}')
            result[relative] = archive.extractfile(member).read()
    package = tomllib.loads(result['Cargo.toml'].decode())['package']
    if (package['name'], package['version']) != (crate, version):
        raise RuntimeError(f'wrong archive package identity: {package}')
    if 'Cargo.lock' not in result:
        raise RuntimeError(f'{crate} lacks the packaged lockfile required for this release')
    return result


def compare_source(root, files, required, license_root=None):
    # A unified packaging workspace may legitimately have different Cargo
    # inheritance/path syntax and VCS metadata. Neither is a source hash.
    # Compare every other shipped byte, not just the displayed notices.
    packaging = {'Cargo.toml', 'Cargo.toml.orig', 'Cargo.lock', '.cargo_vcs_info.json'}
    for relative, expected in files.items():
        if relative in packaging:
            continue
        actual = root / relative
        if license_root and relative in {'LICENSE-MIT', 'LICENSE-APACHE'} and not actual.exists():
            actual = license_root / relative
        if not actual.is_file() or actual.read_bytes() != expected:
            raise RuntimeError(f'tag/registry payload differs: {actual}')
    for relative in required:
        if relative not in files:
            raise RuntimeError(f'tag runtime source/notice absent from archive: {root / relative}')


def main():
    checkout, cargo_home = map(Path, sys.argv[1:])
    versions = {
        'faculties': os.environ['FACULTIES_VERSION'],
        'faculties-migrations': os.environ['MIGRATIONS_VERSION'],
        'trible': os.environ['TRIBLE_VERSION'],
    }
    downloaded = {crate: members(crate, version, cargo_home) for crate, version in versions.items()}
    tracked = subprocess.check_output(
        ['git', '-C', str(checkout), 'ls-files', '-z'], text=True,
    ).split('\0')
    required = {p for p in tracked if p.startswith(('src/', 'bootstrap/')) or p == 'build.rs'}
    required.update(('LICENSE-MIT', 'LICENSE-APACHE', 'README.md'))
    compare_source(checkout, downloaded['faculties'], required)
    migration_root = checkout / 'faculties-migrations'
    migration_files = {p.removeprefix('faculties-migrations/') for p in tracked
                       if p.startswith('faculties-migrations/src/')}
    compare_source(migration_root, downloaded['faculties-migrations'], migration_files, checkout)
    print('Registry package identities, packaged lockfiles, and tag payload correspondence PASS')


if __name__ == '__main__':
    main()
