"""Select measurement stages from the complete PR diff or dispatch inputs."""
import argparse
from fnmatch import fnmatchcase
import json
import os
from pathlib import Path
import subprocess

STAGES = ("torrent", "tunnel", "growth", "representative")
SHARED = (
    "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/**",
    ".github/workflows/measurements.yml",
    ".github/scripts/measurement_plan.py", ".github/scripts/cargo_artifacts.py",
    ".github/scripts/tests/test_measurement_plan.py",
    ".github/scripts/tests/test_cargo_artifacts.py",
)
PATHS = {
    "torrent": (
        ".github/scripts/torrent_load.py",
        "src/torrent.rs", "src/torrent/**", "src/tunnel.rs", "src/tunnel_http.rs",
        "src/tunnel_network.rs", "src/netstack.rs", "src/netstack/**",
    ),
    "tunnel": (
        ".github/scripts/tunnel_load.py",
        "src/tunnel.rs", "src/tunnel_http.rs", "src/tunnel_http_tests.rs",
        "src/tunnel_load_tests.rs", "src/tunnel_network.rs", "src/netstack.rs",
        "src/netstack/**",
    ),
    "growth": (
        ".github/scripts/inventory_growth.py",
        "src/growth.rs",
    ),
}


def pr_plan(paths):
    paths = tuple(paths)
    shared = any(fnmatchcase(path, pattern) for path in paths for pattern in SHARED)
    return {stage: stage != "representative" and (shared or any(
        fnmatchcase(path, pattern) for path in paths for pattern in PATHS[stage]
    )) for stage in STAGES}


def dispatch_plan(inputs):
    selected = {}
    for stage in STAGES:
        value = inputs.get(stage, stage in {"torrent", "tunnel"})
        if isinstance(value, bool):
            selected[stage] = value
        elif value in ("true", "false"):
            selected[stage] = value == "true"
        else:
            raise ValueError("invalid boolean input: " + stage)
    return selected


def changed_paths(base, head):
    # Disabling rename detection selects both the removed and added paths.
    result = subprocess.check_output([
        "git", "diff", "--name-only", "--no-renames", "-z", f"{base}...{head}", "--",
    ])
    return [os.fsdecode(path) for path in result.split(b"\0") if path]


def plan(event_name, event):
    if event_name == "pull_request":
        pr = event["pull_request"]
        return pr_plan(changed_paths(pr["base"]["sha"], pr["head"]["sha"]))
    if event_name == "workflow_dispatch":
        return dispatch_plan(event.get("inputs", {}))
    raise ValueError("unsupported measurement event: " + event_name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", type=Path, default=os.environ.get("GITHUB_EVENT_PATH"))
    parser.add_argument("--event-name", default=os.environ.get("GITHUB_EVENT_NAME"))
    parser.add_argument("--github-output", type=Path, default=os.environ.get("GITHUB_OUTPUT"))
    args = parser.parse_args()
    selected = plan(args.event_name, json.loads(args.event.read_text()))
    outputs = {**selected, "any": any(selected.values())}
    text = "".join(f"{key}={str(value).lower()}\n" for key, value in outputs.items())
    print(text, end="")
    if args.github_output:
        with args.github_output.open("a") as output:
            output.write(text)


if __name__ == "__main__":
    main()
