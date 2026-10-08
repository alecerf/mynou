"""Source-bound immutable release evidence; administration uses Python std only."""
import base64
import hashlib
import io
import json
import re
import tomllib
import zipfile

import lease

DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
PROOF_KEYS = {"image", "version", "source", "visibility", "binary_sha256", "repository",
    "repository_id", "repository_visibility", "package_id", "repository_association",
    "verification_user", "verification_cleanup"}


def version(api, commit):
    value = api.rest("GET", "contents/Cargo.toml?ref=" + commit)
    if value.get("encoding") != "base64":
        raise ValueError("Publication version source is unavailable")
    raw = base64.b64decode(value["content"], validate=False)
    if len(raw) > 64 * 1024:
        raise ValueError("Publication version source exceeds its bound")
    result = tomllib.loads(raw.decode("utf-8"))["package"]["version"]
    if not isinstance(result, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", result):
        raise ValueError("Publication requires an exact source version")
    return result


def proof_json(raw, digest):
    if (not isinstance(digest, str) or not DIGEST.fullmatch(digest)
            or "sha256:" + hashlib.sha256(raw).hexdigest() != digest):
        raise ValueError("Native publication artifact digest differs")
    try:
        with zipfile.ZipFile(io.BytesIO(raw)) as archive:
            entries = archive.infolist()
            if (len(entries) != 1 or entries[0].filename != "mynou-image.json"
                    or entries[0].flag_bits & 1 or not 1 <= entries[0].file_size <= 16 * 1024):
                raise ValueError("Publication proof inventory differs")
            with archive.open(entries[0]) as stream:
                data = stream.read(16 * 1024 + 1)
            if len(data) != entries[0].file_size:
                raise ValueError("Publication proof size differs")
            proof = json.loads(data)
    except (zipfile.BadZipFile, UnicodeDecodeError, json.JSONDecodeError, RuntimeError):
        raise ValueError("Publication proof archive is invalid") from None
    if not isinstance(proof, dict) or set(proof) != PROOF_KEYS:
        raise ValueError("Publication proof fields differ")
    return proof


def verified(api, commit, run):
    """Return only allowlisted evidence after native source/run/release fences."""
    if (api.ref(api.cfg["default_branch"]) != commit or run.get("head_sha") != commit
            or run.get("head_branch") != api.cfg["default_branch"]
            or run.get("event") not in ("push", "workflow_dispatch")
            or run.get("status") != "completed" or run.get("conclusion") != "success"
            or type(run.get("id")) is not int or run["id"] <= 0
            or type(run.get("run_attempt")) is not int or run["run_attempt"] <= 0):
        raise ValueError("Publication requires successful exact-default native CI")
    jobs = api.pages(f"actions/runs/{run['id']}/jobs?filter=latest", "jobs")
    required = [*api.cfg["required_workflows"]["Mynou CI"], "release"]
    for name in required:
        matches = [j for j in jobs if j.get("name") == name]
        if len(matches) != 1 or matches[0].get("status") != "completed" or matches[0].get("conclusion") != "success":
            raise ValueError("Publication requires every native build/package/release job")
    number = version(api, commit)
    tag = "v" + number
    native_tag = api.rest("GET", "git/ref/tags/" + tag)
    if (native_tag.get("ref") != "refs/tags/" + tag
            or (native_tag.get("object") or {}).get("sha") != commit
            or (native_tag.get("object") or {}).get("type") != "commit"):
        raise ValueError("Publication tag does not identify the exact source commit")
    release = api.rest("GET", "releases/tags/" + tag)
    body = release.get("body") or ""
    published = release.get("published_at")
    if (release.get("draft") is not False or release.get("prerelease") is not False
            or release.get("immutable") is not True or release.get("tag_name") != tag
            or type(release.get("id")) is not int or release["id"] <= 0
            or not isinstance(published, str) or lease.time(published) > lease.now()
            or "<!-- mynou-ci-release -->" not in body or "Source commit: `" + commit + "`" not in body):
        raise ValueError("Immutable source-bound release publication is unverified")
    names = {f"mynou-v{number}-{suffix}" for suffix in ["linux-x86_64", "macos-arm64", "macos-x86_64"]} | {"SHA256SUMS"}
    assets = release.get("assets")
    if not isinstance(assets, list) or len(assets) != 4 or {a.get("name") for a in assets} != names:
        raise ValueError("Published release asset inventory differs")
    for asset in assets:
        if (type(asset.get("id")) is not int or asset["id"] <= 0
                or type(asset.get("size")) is not int or asset["size"] <= 0
                or asset.get("state") != "uploaded" or not isinstance(asset.get("digest"), str)
                or not DIGEST.fullmatch(asset["digest"])):
            raise ValueError("Published release asset evidence is incomplete")
    if len({a["id"] for a in assets}) != 4:
        raise ValueError("Published release asset identities differ")
    artifact_name = f"mynou-publication-{commit}-{run['id']}-{run['run_attempt']}"
    artifacts = api.pages(f"actions/runs/{run['id']}/artifacts", "artifacts")
    matches = [a for a in artifacts if a.get("name") == artifact_name and a.get("expired") is False]
    if len(matches) != 1:
        raise ValueError("Exact-run publication proof is unavailable")
    artifact = matches[0]
    origin = artifact.get("workflow_run") or {}
    repo = api.rest("GET", "")
    if (origin.get("id") != run["id"] or origin.get("head_sha") != commit
            or origin.get("head_branch") != api.cfg["default_branch"]
            or origin.get("repository_id") != repo.get("id") or origin.get("head_repository_id") != repo.get("id")
            or repo.get("full_name") != api.repo or type(repo.get("id")) is not int or repo["id"] <= 0
            or type(repo.get("private")) is not bool
            or type(artifact.get("id")) is not int or artifact["id"] <= 0
            or type(artifact.get("size_in_bytes")) is not int or not 1 <= artifact["size_in_bytes"] <= 64 * 1024):
        raise ValueError("Publication proof native source identity differs")
    proof = proof_json(api.publication_archive(artifact["id"]), artifact.get("digest"))
    linux = next(a for a in assets if a["name"] == f"mynou-v{number}-linux-x86_64")
    image_pattern = re.escape("ghcr.io/" + api.repo) + r"@sha256:[0-9a-f]{64}"
    if (proof.get("source") != commit or proof.get("version") != number or proof.get("repository") != api.repo
            or type(proof.get("repository_id")) is not int or proof["repository_id"] != repo["id"]
            or proof.get("repository_visibility") != ("private" if repo.get("private") is True else "public")
            or proof.get("visibility") != "private" or proof.get("verification_user") != "1001:1001"
            or proof.get("verification_cleanup") != "completed" or type(proof.get("package_id")) is not int
            or proof["package_id"] <= 0 or proof.get("repository_association") not in ("reported-match", "not_exposed")
            or proof.get("binary_sha256") != linux["digest"].removeprefix("sha256:")
            or not isinstance(proof.get("image"), str) or not re.fullmatch(image_pattern, proof["image"])
            or "`" + proof["image"] + "`" not in body):
        raise ValueError("Private image/source/executable/cleanup evidence differs")
    fresh = api.rest("GET", f"actions/runs/{run['id']}")
    if (api.ref(api.cfg["default_branch"]) != commit or fresh.get("head_sha") != commit
            or fresh.get("run_attempt") != run["run_attempt"] or fresh.get("status") != "completed"
            or fresh.get("conclusion") != "success"):
        raise ValueError("Native source/run changed during publication observation")
    return {"commit": commit, "state": "published", "version": number, "at": published,
        "release_id": release["id"], "immutable": True, "tag_commit": commit,
        "run_id": run["id"], "run_attempt": run["run_attempt"], "status": "completed", "conclusion": "success",
        "assets": [{k: a[k] for k in ["id", "name", "size", "digest"]} for a in assets],
        "registry_proof": proof, "proof_artifact": {"id": artifact["id"], "digest": artifact["digest"]}}
