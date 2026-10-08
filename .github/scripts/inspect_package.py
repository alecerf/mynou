"""CI-only bounded, read-only diagnosis of the Mynou container package."""
import json
import os
from pathlib import Path
import re
import tomllib

from publish_container import CommandError, command


REPOSITORY = "alecerf/mynou"
PACKAGE_PATH = "/users/alecerf/packages/container/mynou"


def read_json(path, limit, run):
    try:
        value = run(["gh", "api", "--method", "GET", path])
    except CommandError as error:
        match = re.search(r"\bHTTP (401|403|404)\b", error.stderr)
        status = match.group(1) if match else "unavailable"
        raise RuntimeError("Package inspection failed: " + status
                           + "; not proof of absence or package permissions") from None
    if len(value) > limit:
        raise ValueError("Package inspection response exceeds its bound")
    try:
        return json.loads(value)
    except ValueError:
        raise ValueError("Package inspection returned malformed JSON") from None


def inspect_package(version, source, run=command):
    if (os.environ.get("GITHUB_REPOSITORY") != REPOSITORY
            or os.environ.get("GITHUB_EVENT_NAME") != "pull_request"
            or os.environ.get("MYNOU_DIAGNOSTIC_HEAD_REPOSITORY") != REPOSITORY
            or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version)
            or not re.fullmatch(r"[0-9a-f]{40}", source)):
        raise ValueError("Inspection requires a same-repository PR and exact source/version")
    package = read_json(PACKAGE_PATH, 65_536, run)
    if not isinstance(package, dict):
        raise ValueError("Package inspection requires an object")
    owner = package.get("owner")
    repository = package.get("repository")
    repository = repository if isinstance(repository, dict) else {}
    full_name = repository.get("full_name")
    repository_state = ("matches" if full_name == REPOSITORY else
                        "different" if isinstance(full_name, str) and full_name else "missing")
    visibility = package.get("visibility")
    visibility = visibility if visibility in ("private", "public", "internal") else "unknown"
    versions = read_json(PACKAGE_PATH + "/versions?per_page=100", 524_288, run)
    if not isinstance(versions, list) or len(versions) > 100:
        raise ValueError("Package version inspection exceeds one bounded page")
    wanted = {version, "sha-" + source}
    observed = []
    for entry in versions:
        if not isinstance(entry, dict):
            raise ValueError("Package version inspection requires objects")
        metadata = entry.get("metadata")
        container = metadata.get("container") if isinstance(metadata, dict) else None
        tags = container.get("tags") if isinstance(container, dict) else None
        if tags is None:
            continue
        if not isinstance(tags, list) or len(tags) > 1_000 or any(not isinstance(tag, str) for tag in tags):
            raise ValueError("Malformed or oversized registry tag list")
        selected = wanted.intersection(tags)
        if selected:
            digest = entry.get("name")
            if not isinstance(digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
                raise ValueError("Matching tags lack a valid content digest")
            observed.extend({"tag": tag, "digest": digest} for tag in sorted(selected))
    return {
        "image": "ghcr.io/" + REPOSITORY,
        "visibility": visibility,
        "package_identity_matches": (package.get("name") == "mynou"
                                     and package.get("package_type") == "container"
                                     and isinstance(owner, dict) and owner.get("login") == "alecerf"),
        "repository_metadata": repository_state,
        "repository_fields": {key: key in repository for key in ("id", "name", "full_name", "owner")},
        "listed_versions": len(versions),
        "versions_complete": len(versions) < 100,
        "observed_tags": sorted(observed, key=lambda item: (item["tag"], item["digest"])),
        "publication_verified": False,
    }


def main():
    with Path("Cargo.toml").open("rb") as stream:
        version = tomllib.load(stream)["package"]["version"]
    proof = inspect_package(version, os.environ["MYNOU_DIAGNOSTIC_SOURCE"])
    print(json.dumps(proof, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, KeyError) as error:
        raise SystemExit("Registry diagnosis failed: " + str(error)) from None
