"""Select CI work from the complete PR diff, using only git and Python stdlib."""
import json
import os
from pathlib import Path
import re
import subprocess

KEYS = ("tests", "build", "tooling")
RUST_CONFIG = {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain", "build.rs"}
CONTROL = {".github/scripts/ci_changes.py", ".github/workflows/changes.yml"}
RELEASE_FILES = {"Cargo.toml", "Cargo.lock", "docs/releases/unreleased.md"}
VERSION_LINE = re.compile(r'[+-]version = "[0-9]+\.[0-9]+\.[0-9]+"')


def release(pull_request):
    """A release only builds and publishes a tree trunk already validated: no
    tests and no start check. The branch rules require the CodeQL app on pull
    requests, so the quick Actions scan stays there."""
    return {"tests": False, "build": True, "tooling": False, "release": True,
            "languages": ["actions"] if pull_request else []}


def select(paths, full=False, pull_request=True):
    work = dict.fromkeys(KEYS, full)
    languages = {"actions"} if full or pull_request else set()
    if full:
        languages.update(("rust", "javascript-typescript"))
    for path in paths:
        rust = path.endswith(".rs") or path in RUST_CONFIG or path.startswith(".cargo/")
        product = (rust and not path.startswith("tests/")) or path.startswith("src/")
        control = path in CONTROL
        ci = path == ".github/workflows/ci.yml"
        native_fixture = path in {".github/scripts/crypto_fixtures.py",
                                  ".github/scripts/host_route_observer.py",
                                  ".github/scripts/host_io_observer.py", ".github/scripts/host_io_observer.c"}
        work["tests"] |= product or path.startswith("tests/") or ci or control or native_fixture
        work["build"] |= product or ci or control or path == ".github/scripts/publish-release.sh"
        work["tooling"] |= path.startswith((".github/scripts/", ".github/workflows/", ".github/actions/"))
        security = control or path.startswith(".github/codeql") or path == ".github/workflows/codeql.yml"
        if rust or security:
            languages.add("rust")
        javascript = path.endswith((".js", ".mjs", ".cjs", ".jsx", ".ts", ".tsx"))
        js_config = Path(path).name in {"package.json", "package-lock.json", "tsconfig.json"}
        if javascript or js_config or security:
            languages.add("javascript-typescript")
        if path.startswith((".github/workflows/", ".github/actions/")) or control:
            languages.add("actions")
        # Known non-product paths are cheap. New/unclassified inputs get the
        # full product checks rather than silently losing coverage.
        known = (product or path.startswith(("tests/", ".cargo/", ".github/", "docs/"))
                 or ("/" not in path and path.endswith(".md")) or path == ".gitignore")
        if not known:
            work.update(dict.fromkeys(KEYS, True))
    # Packaging needs a native executable, so it follows build, not test-only changes.
    return {**work, "release": False, "languages": sorted(languages)}


def changed_paths(base, head):
    # No rename detection: both the deleted and added names must participate.
    # NUL separation preserves spaces/newlines; no API pagination/300-file limit.
    raw = subprocess.check_output(["git", "diff", "--name-only", "--no-renames", "-z", f"{base}...{head}", "--"])
    return [os.fsdecode(path) for path in raw.split(b"\0") if path]


def version_only(base, head, paths):
    """Whether a diff only raises the package version and consumes the notes."""
    if "Cargo.toml" not in paths or not set(paths) <= RELEASE_FILES:
        return False
    raw = subprocess.check_output(["git", "diff", "-U0", "--no-renames", f"{base}...{head}", "--", "Cargo.toml", "Cargo.lock"])
    lines = [line for line in raw.decode().splitlines()
             if line.startswith(("+", "-")) and not line.startswith(("+++", "---"))]
    return bool(lines) and all(VERSION_LINE.fullmatch(line) for line in lines)


def main():
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    kind = os.environ["GITHUB_EVENT_NAME"]
    paths, span = [], None
    tag = kind == "push" and os.environ.get("GITHUB_REF_TYPE") == "tag"
    try:
        if kind == "pull_request":
            pr = event["pull_request"]
            span = pr["base"]["sha"], pr["head"]["sha"]
            paths = changed_paths(*span)
        elif kind == "push" and not tag:
            # Push baselines must cover every commit since the preceding push.
            span = event["before"], event["after"]
            raw = subprocess.check_output(["git", "diff", "--name-only", "--no-renames", "-z", *span, "--"])
            paths = [os.fsdecode(path) for path in raw.split(b"\0") if path]
        # A version tag publishes the release commit, whose parent trunk
        # validated; the decide job checks that the tagged commit raised the version.
        if tag or (span and version_only(*span, paths)):
            work = release(kind == "pull_request")
        else:
            full = kind not in {"pull_request", "push"}
            work = select(paths, full=full, pull_request=kind == "pull_request")
    except (KeyError, OSError, subprocess.CalledProcessError):
        print("::warning::Cannot determine the complete diff; running all checks.")
        work = select([], full=True)
    with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
        for key in KEYS:
            print(f"{key}={str(work[key]).lower()}", file=stream)
        print(f"release={str(work['release']).lower()}", file=stream)
        print("languages=" + json.dumps(work["languages"]), file=stream)
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as stream:
        print("CI selection: `" + json.dumps(work) + "`", file=stream)


if __name__ == "__main__":
    main()
