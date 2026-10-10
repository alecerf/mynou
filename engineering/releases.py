"""Deliberate releases: only a weekly release PR changes the version and consumes the
unreleased notes, and CI publishes a version once, when a tag for it is pushed,
with a changelog generated from those notes. Python std only."""
import argparse
import base64
from datetime import datetime, timedelta, timezone
import json
import re
import sys

from github import APIError, GitHub, config
from protocol import instant, labels as label_names, stamp

PRODUCT_DIRS = ("src/", "examples/", ".cargo/")
PRODUCT_FILES = {"build.rs", "rust-toolchain.toml"}
MANIFESTS = {"Cargo.toml", "Cargo.lock"}
UNRELEASED = "docs/releases/unreleased.md"
CATEGORIES = ("Added", "Changed", "Deprecated", "Removed", "Fixed", "Security")
ENTRY = re.compile("## (" + "|".join(CATEGORIES) + r"): (\S.*)")
# Under GitHub's release-body limit, leaving room for the link prefixes and the footer, so
# oversized notes fail in the PR that writes them and never burn a version.
NOTES_LIMIT = 100_000
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


def parse_entries(text):
    """(category, title, body lines) of every `## <Category>: <title>` section of the
    unreleased notes. Anything else fails closed, so a release never drops text."""
    entries, current = [], None
    for number, line in enumerate(text.splitlines(), 1):
        if line.startswith("## "):
            match = ENTRY.fullmatch(line)
            if not match:
                raise ValueError(f"Line {number}: use `## <Category>: <title>` with a category of "
                                 + ", ".join(CATEGORIES))
            current = (match[1], match[2].strip().rstrip("."), [])
            entries.append(current)
        elif line.startswith("#"):
            raise ValueError(f"Line {number}: a note holds text, not headings")
        elif current is not None:
            current[2].append(line.rstrip())
        elif line.strip():
            raise ValueError(f"Line {number}: text before the first `## <Category>: <title>`")
    for _, title, body in entries:
        while body and not body[-1]:
            body.pop()
        while body and not body[0]:
            body.pop(0)
        if not body:
            raise ValueError(f"The note '{title}' has no text")
    if not entries:
        raise ValueError(f"{UNRELEASED} holds no notes")
    return entries


def changelog(text, repository, tag):
    """The human changelog of a release: notes grouped by category in a fixed order,
    one bullet per note, with repository-root links made absolute for the release page."""
    entries = parse_entries(text)
    lines = []
    for category in CATEGORIES:
        group = [entry for entry in entries if entry[0] == category]
        if not group:
            continue
        lines += [f"### {category}", ""]
        for _, title, body in group:
            lines.append(f"- **{title}.** {body[0]}")
            lines += ["  " + line if line else "" for line in body[1:]]
            lines.append("")
    root = f"https://github.com/{repository}/blob/{tag}"
    rendered = "\n".join(lines).rstrip("\n")
    return re.sub(r"\]\(/([^)\s]{0,512})\)", lambda link: f"]({root}/{link[1]})", rendered) + "\n"


def notes_problems(api, ref):
    """Why the unreleased notes at `ref` cannot be published; empty when they can."""
    try:
        parse_entries(file_text(api, UNRELEASED, ref, NOTES_LIMIT))
    except ValueError as error:
        return [f"{UNRELEASED}: {error}"]
    return []


def due(latest, shipped_change, now, policy):
    """Whether agents should open the next release PR."""
    if not shipped_change:
        return False
    return latest is None or now >= latest["published_at"] + timedelta(days=policy["minimum_interval_days"])


def check(*, base, head, files, labels, latest, shipped_change, tag_exists, now, policy, commits=1):
    """Release-policy violations of one pull request; empty when compliant.

    `files` maps every path the PR touches (including a rename's old path) to
    its status."""
    old, new = version(base), version(head)
    requested = "release" in labels
    if old == new:
        return ["A PR labeled `release` must bump the package version."] if requested else []
    if not requested:
        return ["Only a PR labeled `release` may change the package version. Keep the version, "
                "and add user-facing notes to docs/releases/unreleased.md instead."]
    errors = []
    if new <= old:
        errors.append("The release version must increase.")
    outside = sorted(path for path in files if path not in MANIFESTS and path != UNRELEASED)
    if outside:
        errors.append("A release PR changes only Cargo.toml, Cargo.lock and docs/releases/unreleased.md; "
                      "move these to their own PRs: " + ", ".join(outside[:20]))
    if unversioned(base) != unversioned(head):
        errors.append("A release PR changes nothing in Cargo.toml but the package version.")
    if files.get(UNRELEASED) != "removed":
        errors.append(f"A release PR deletes {UNRELEASED}: its notes ship in the release.")
    if commits != 1:
        errors.append("A release PR is a single commit: the tag sits on it and the notes are read from its parent.")
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


def file_text(api, path, ref, limit=64 * 1024):
    value = api.rest("GET", f"contents/{path}?ref={ref}")
    if not isinstance(value, dict) or value.get("encoding") != "base64":
        raise ValueError(f"{path} is unavailable at {ref}")
    raw = base64.b64decode(value["content"])
    if len(raw) > limit:
        raise ValueError(f"{path} exceeds its bound")
    return raw.decode("utf-8")


def manifest(api, ref):
    return file_text(api, "Cargo.toml", ref)


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
    parents = api.rest("GET", f"git/commits/{commit}").get("parents") or []
    if len(parents) != 1:
        raise ValueError("The release commit must have exactly one parent")
    parent = parents[0]["sha"]
    if released <= version(manifest(api, parent)):
        raise ValueError("The tag must sit on the commit that raises the version over its parent")
    try:
        parse_entries(file_text(api, UNRELEASED, parent, NOTES_LIMIT))
    except APIError as error:
        if error.status != 404:
            raise
        raise ValueError(f"The release notes {UNRELEASED} are missing before the release commit") from None
    latest = latest_release(api)
    if latest and released <= tuple(int(part) for part in latest["tag"][1:].split(".")):
        raise ValueError(f"The version must exceed the latest release {latest['tag']}")
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
    errors = check(base=base, head=head, files=files, labels=names, latest=latest, shipped_change=change,
                   tag_exists=exists, now=now, policy=api.cfg["release_policy"], commits=pull["commits"])
    # A malformed note must fail in the PR that writes it, not when the release publishes.
    if files.get(UNRELEASED) not in (None, "removed"):
        errors += notes_problems(api, pull["head"]["sha"])
    elif "release" in names and version(base) != version(head) and files.get(UNRELEASED) == "removed":
        errors += notes_problems(api, pull["base"]["sha"])
    return errors


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
    rendering = commands.add_parser("notes", help="render the unreleased notes on stdin as a release changelog")
    rendering.add_argument("--tag", required=True)
    tagging = commands.add_parser("tag", help="plan, and with --create push, the tag of a merged release PR")
    tagging.add_argument("number", type=int)
    tagging.add_argument("--create", action="store_true")
    args = parser.parse_args()
    if args.command == "notes":
        if not re.fullmatch("v" + VERSION.pattern, args.tag):
            raise ValueError("The tag must be v<major>.<minor>.<patch>")
        text = sys.stdin.read(NOTES_LIMIT + 1)
        if len(text) > NOTES_LIMIT:
            raise ValueError("The unreleased notes exceed their bound")
        sys.stdout.write(changelog(text, config()["repository"], args.tag))
        return
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
