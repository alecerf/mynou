"""Select CI work from the complete PR diff, using only git and Python stdlib."""
import json
import os
from pathlib import Path
import subprocess

KEYS = ("tests", "build", "tooling")
RUST_CONFIG = {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain", "build.rs"}
CONTROL = {".github/scripts/ci_changes.py", ".github/workflows/changes.yml"}


def select(paths, full=False, pull_request=True):
    work = dict.fromkeys(KEYS, full)
    languages = {"actions"} if full or pull_request else set()
    if full:
        languages.update(("rust", "javascript-typescript"))
    for path in paths:
        rust = path.endswith(".rs") or path in RUST_CONFIG or path.startswith(".cargo/")
        product = (rust and not path.startswith("tests/")) or path.startswith(("src/", "examples/"))
        control = path in CONTROL
        ci = path == ".github/workflows/ci.yml"
        native_fixture = path in {".github/scripts/crypto_fixtures.py",
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
    return {**work, "languages": sorted(languages)}


def changed_paths(base, head):
    # No rename detection: both the deleted and added names must participate.
    # NUL separation preserves spaces/newlines; no API pagination/300-file limit.
    raw = subprocess.check_output(["git", "diff", "--name-only", "--no-renames", "-z", f"{base}...{head}", "--"])
    return [os.fsdecode(path) for path in raw.split(b"\0") if path]


def main():
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    kind = os.environ["GITHUB_EVENT_NAME"]
    paths = []
    try:
        if kind == "pull_request":
            pr = event["pull_request"]
            paths = changed_paths(pr["base"]["sha"], pr["head"]["sha"])
        elif kind == "push" and os.environ.get("GITHUB_REF_TYPE") != "tag":
            # Push baselines must cover every commit since the preceding push.
            raw = subprocess.check_output(["git", "diff", "--name-only", "--no-renames", "-z", event["before"], event["after"], "--"])
            paths = [os.fsdecode(path) for path in raw.split(b"\0") if path]
        full = kind not in {"pull_request", "push"} or os.environ.get("GITHUB_REF_TYPE") == "tag"
        work = select(paths, full=full, pull_request=kind == "pull_request")
    except (KeyError, OSError, subprocess.CalledProcessError):
        print("::warning::Cannot determine the complete diff; running all checks.")
        work = select([], full=True)
    with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
        for key in KEYS:
            print(f"{key}={str(work[key]).lower()}", file=stream)
        print("languages=" + json.dumps(work["languages"]), file=stream)
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as stream:
        print("CI selection: `" + json.dumps(work) + "`", file=stream)


if __name__ == "__main__":
    main()
