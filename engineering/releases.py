"""Deliberate releases: only a weekly release PR changes the version, and CI
publishes a version once, when a tag for it is pushed. Python std only."""
import argparse
import base64
from datetime import datetime, timedelta, timezone
import json
import re
import sys

from github import APIError, GitHub
from protocol import instant, labels as label_names, stamp

PRODUCT_DIRS = ("src/", "examples/", ".cargo/")
PRODUCT_FILES = {"build.rs", "rust-toolchain.toml"}
MANIFESTS = {"Cargo.toml", "Cargo.lock"}
NOTES = "docs/releases/"
VERSION = re.compile(r"(?:0|[1-9][0-9]{0,5})\.(?:0|[1-9][0-9]{0,5})\.(?:0|[1-9][0-9]{0,5})")
COMPARE_FILE_LIMIT = 300


def version_line(manifest):
    """Index and value of `version = "..."` in the [package] table of Cargo.toml.

    A bounded line parser keeps this tooling on older Python versions without
    tomllib; anything unexpected fails closed."""
    table = None
    for index, line in enumerate(manifest.splitlines()):
        stripped = line.strip()
        if stripped.startswith("["):
            table = stripped
        elif table == "[package]":
            match = re.fullmatch(r'version\s*=\s*"([^"\\]*)"', stripped)
            if match:
                return index, match[1]
    raise ValueError("Cargo.toml requires a package version")


def version(manifest):
    value = version_line(manifest)[1]
    if not VERSION.fullmatch(value):
        raise ValueError("Cargo.toml requires an exact x.y.z package version")
    return tuple(int(part) for part in value.split("."))


def name(number):
    return ".".join(str(part) for part in number)


def unversioned(manifest):
    """Cargo.toml without its package version, to compare everything else."""
    lines = manifest.splitlines()
    del lines[version_line(manifest)[0]]
    return lines


def shipped(paths, truncated, released, current):
    """Whether the executable can differ from the last release."""
    return (truncated or any(path.startswith(PRODUCT_DIRS) or path in PRODUCT_FILES for path in paths)
            or unversioned(released) != unversioned(current))


def due(latest, shipped_change, now, policy):
    """Whether agents should open the next release PR."""
    if not shipped_change:
        return False
    return latest is None or now >= latest["published_at"] + timedelta(days=policy["minimum_interval_days"])


def check(*, base, head, files, labels, latest, shipped_change, tag_exists, now, policy):
    """Release-policy violations of one pull request; empty when compliant.

    `files` maps every path the PR touches (including a rename's old path) to
    its status."""
    old, new = version(base), version(head)
    requested = "release" in labels
    if old == new:
        return ["A PR labeled `release` must bump the package version."] if requested else []
    if not requested:
        return ["Only a PR labeled `release` may change the package version. Keep the version, "
                "and add user-facing notes in docs/releases/unreleased/<issue>.md instead."]
    errors = []
    if new <= old:
        errors.append("The release version must increase.")
    outside = sorted(path for path in files if path not in MANIFESTS and not path.startswith(NOTES))
    if outside:
        errors.append("A release PR changes only Cargo.toml, Cargo.lock and docs/releases/; "
                      "move these to their own PRs: " + ", ".join(outside[:20]))
    if unversioned(base) != unversioned(head):
        errors.append("A release PR changes nothing in Cargo.toml but the package version.")
    notes = NOTES + name(new) + ".md"
    if files.get(notes) in (None, "removed"):
        errors.append(f"A release PR adds its notes in {notes}.")
    if tag_exists:
        errors.append(f"v{name(new)} already exists; choose the next version.")
    urgent = policy["urgent_label"]
    if urgent in labels or latest is None:
        return errors
    earliest = latest["published_at"] + timedelta(days=policy["minimum_interval_days"])
    if now < earliest:
        errors.append(f"Releases are weekly: the next one may merge from {stamp(earliest)}. "
                      f"Only a security fix or an explicit owner request uses `{urgent}`.")
    if not shipped_change:
        errors.append(f"Nothing shipped changed since {latest['tag']}: engineering, CI and "
                      "documentation changes are not released on their own.")
    return errors


def manifest(api, ref):
    value = api.rest("GET", "contents/Cargo.toml?ref=" + ref)
    if not isinstance(value, dict) or value.get("encoding") != "base64":
        raise ValueError("Cargo.toml is unavailable at " + ref)
    raw = base64.b64decode(value["content"])
    if len(raw) > 64 * 1024:
        raise ValueError("Cargo.toml exceeds its bound")
    return raw.decode("utf-8")


def latest_release(api):
    try:
        value = api.rest("GET", "releases/latest")
    except APIError as error:
        if error.status == 404:
            return None
        raise
    tag = value.get("tag_name")
    if not isinstance(tag, str) or not re.fullmatch("v" + VERSION.pattern, tag):
        raise ValueError("The latest release lacks an exact version tag")
    return {"tag": tag, "published_at": instant(value.get("published_at"))}


def tag_exists(api, tag):
    try:
        api.rest("GET", "git/ref/tags/" + tag)
    except APIError as error:
        if error.status == 404:
            return False
        raise
    return True


def plan_tag(api, number):
    """The tag a merged release PR calls for, after checking that it can be pushed.

    Pushing the tag starts publication, and CI refuses a tag that differs from the
    Cargo.toml version of its commit or whose commit is not on the default branch;
    this plan checks the same things first, so a mistake never reaches CI."""
    pull = api.rest("GET", f"pulls/{number}")
    if "release" not in label_names(pull) or not pull.get("merged"):
        raise ValueError("Only a merged release PR is tagged")
    commit = pull.get("merge_commit_sha")
    if not isinstance(commit, str) or not re.fullmatch("[0-9a-f]{40}", commit):
        raise ValueError("The release PR has no merge commit")
    released = version(manifest(api, commit))
    if released != version(manifest(api, pull["head"]["sha"])):
        raise ValueError("The merged commit does not carry the release PR's version")
    if released <= version(manifest(api, pull["base"]["sha"])):
        raise ValueError("The release PR does not raise the version")
    tag = "v" + name(released)
    if tag_exists(api, tag):
        raise ValueError(f"{tag} already exists; a published or burned version is never retagged")
    relation = api.rest("GET", f"compare/{api.cfg['default_branch']}...{commit}")["status"]
    if relation not in ("identical", "behind"):
        raise ValueError("The release commit is not on the default branch")
    return {"pr": number, "tag": tag, "commit": commit}


def create_tag(api, plan):
    api.rest("POST", "git/refs", {"ref": "refs/tags/" + plan["tag"], "sha": plan["commit"]})


def shipped_since(api, latest, head):
    """Shipped inputs that changed between the latest release tag and head."""
    if latest is None:
        return True
    files = api.rest("GET", f"compare/{latest['tag']}...{head}").get("files") or []
    paths = {f["filename"] for f in files} | {f["previous_filename"] for f in files if f.get("previous_filename")}
    return shipped(paths, len(files) >= COMPARE_FILE_LIMIT, manifest(api, latest["tag"]), manifest(api, head))


def check_pr(api, number, now):
    pull = api.rest("GET", f"pulls/{number}")
    names = label_names(pull)
    files = {}
    for item in api.pages(f"pulls/{number}/files"):
        files[item["filename"]] = item["status"]
        if item.get("previous_filename"):
            files[item["previous_filename"]] = "removed"
    base, head = manifest(api, pull["base"]["sha"]), manifest(api, pull["head"]["sha"])
    latest, change, exists = None, False, False
    if "release" in names and version(base) != version(head):
        latest = latest_release(api)
        change = shipped_since(api, latest, pull["head"]["sha"])
        exists = tag_exists(api, "v" + name(version(head)))
    return check(base=base, head=head, files=files, labels=names, latest=latest,
                 shipped_change=change, tag_exists=exists, now=now, policy=api.cfg["release_policy"])


def status(api, now, open_release_issues=()):
    latest = latest_release(api)
    change = shipped_since(api, latest, api.cfg["default_branch"])
    return {"latest": latest and latest["tag"],
            "published_at": latest and stamp(latest["published_at"]),
            "shipped_changes_since_latest": change,
            "open_release_issues": list(open_release_issues),
            "due": due(latest, change, now, api.cfg["release_policy"]) and not open_release_issues}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check-pr").add_argument("number", type=int)
    commands.add_parser("status")
    tagging = commands.add_parser("tag", help="plan, and with --create push, the tag of a merged release PR")
    tagging.add_argument("number", type=int)
    tagging.add_argument("--create", action="store_true")
    args = parser.parse_args()
    api = GitHub()
    now = datetime.now(timezone.utc)
    if args.command == "tag":
        plan = plan_tag(api, args.number)
        if args.create:
            create_tag(api, plan)
        print(json.dumps(dict(plan, created=args.create), indent=2))
        return
    if args.command == "status":
        print(json.dumps(status(api, now), indent=2))
        return
    violations = check_pr(api, args.number, now)
    print(json.dumps({"pr": args.number, "violations": violations}, indent=2))
    if violations:
        for violation in violations:
            print("::error::" + violation)
        sys.exit(1)


if __name__ == "__main__":
    try:
        main()
    except (APIError, ValueError, RuntimeError, KeyError, TypeError) as error:
        print("Release policy stopped: " + str(error), file=sys.stderr)
        sys.exit(1)
