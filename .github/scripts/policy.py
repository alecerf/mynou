"""Deterministic repository, release and credential checks; Python std only."""
import ast
import json
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[2]
TOKEN = re.compile(rb"(?<![A-Za-z0-9])(?:gh[psoru]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{60,255}|sk-(?:proj-)?[A-Za-z0-9_-]{40,255})(?![A-Za-z0-9])")


def check_config(cfg):
    assert cfg['schema'] == 1
    assert re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9-]{0,38}/[A-Za-z0-9_][A-Za-z0-9_.-]{0,99}', cfg['repository'])
    assert cfg['default_branch'] == 'trunk'
    release = cfg['release_policy']
    assert type(release['minimum_interval_days']) is int and 1 <= release['minimum_interval_days'] <= 90
    assert release['urgent_label'] == 'release-now'


def check_workflows(root):
    workflows = root / '.github/workflows'
    for path in sorted(workflows.glob('*.yml')):
        assert 'pull_request_target' not in path.read_text(), 'Never run PR code with base privileges: ' + path.name
    ci = (workflows / 'ci.yml').read_text()
    assert "tags: ['v*']" in ci and 'branches: [trunk]' not in ci
    assert 'GITHUB_REF_NAME" != "v$VERSION"' in ci and 'compare/trunk...$GITHUB_SHA' in ci
    assert '$GITHUB_SHA^:Cargo.toml' in ci and '$GITHUB_SHA^:docs/releases/unreleased.md' in ci
    assert '.github/scripts/releases.py notes' in ci
    assert "needs.package.outputs.publish == 'true'" in ci
    policy = (workflows / 'release-policy.yml').read_text()
    assert '.github/scripts/releases.py check-pr' in policy and ': write' not in policy
    audit = (workflows / 'security-audit.yml').read_text()
    assert 'source-history:' in audit and '.github/scripts/secret_audit.py history' in audit
    assert '.github/scripts/secret_audit.py logs' in audit


def check(root=ROOT):
    check_config(json.loads((root / '.github/release-policy.json').read_text()))
    cargo = tomllib.loads((root / 'Cargo.toml').read_text())
    for table in [cargo, *cargo.get('target', {}).values()]:
        assert not any(table.get(key) for key in ['dependencies', 'dev-dependencies', 'build-dependencies'])
    tracked = subprocess.check_output(['git', 'ls-files', '-z'], cwd=root).split(b'\0')
    for name in filter(None, tracked):
        path = root / name.decode()
        if not path.is_file():
            continue
        if path.suffix == '.py':
            ast.parse(path.read_text(), filename=str(path))
        if path.stat().st_size <= 2 * 1024 * 1024:
            assert TOKEN.search(path.read_bytes()) is None, 'Credential-pattern finding in ' + name.decode()
    check_workflows(root)
    print('Repository policy passed: zero Cargo dependencies; release and security workflow guards; no high-confidence token findings.')


if __name__ == '__main__':
    check()
