"""CI-only authenticated reuse of completed Actions audit coverage."""
import base64
from datetime import datetime, timezone
import hashlib
import io
import json
import os
from pathlib import Path
import re
import urllib.parse
import zipfile

FILES = ("engineering/secret_audit.py", "engineering/log_coverage.py",
         ".github/workflows/security-audit.yml")
WORKFLOW = ".github/workflows/security-audit.yml"
MAX_RECEIPT = 8 * 1024 * 1024
MAX_ENTRIES = 20000
MAX_CANDIDATES = 12
ENTRY_KEYS = {"run_id", "attempt", "head_sha", "workflow_id",
              "head_repository_id", "state", "members"}


class CoverageError(Exception):
    pass


def require(condition):
    if not condition:
        raise CoverageError("Audit coverage proof is invalid or unavailable")


def identifier(value, zero=False):
    require(type(value) is int and (0 if zero else 1) <= value < 2 ** 63)
    return value


def commit(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value))
    return value


def run_identity(run):
    repository = run.get("head_repository")
    repository_id = repository.get("id") if isinstance(repository, dict) else 0
    return {"run_id": identifier(run["id"]), "head_sha": commit(run["head_sha"]),
            "workflow_id": identifier(run["workflow_id"]),
            "head_repository_id": identifier(repository_id, zero=True)}


def policy_digest(files):
    require(set(files) == set(FILES))
    digest = hashlib.sha256(b"mynou-log-coverage-v1\0")
    for name in FILES:
        payload = files[name]
        require(isinstance(payload, bytes) and len(payload) <= 128 * 1024)
        digest.update(name.encode("ascii") + b"\0")
        digest.update(len(payload).to_bytes(8, "big"))
        digest.update(payload)
    return digest.hexdigest()


def local_policy():
    root = Path(__file__).resolve().parents[1]
    return policy_digest({name: (root / name).read_bytes() for name in FILES})


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result)
        result[key] = value
    return result


def receipt_json(data, digest):
    require(isinstance(digest, str)
            and re.fullmatch(r"sha256:[0-9a-f]{64}", digest)
            and len(data) <= MAX_RECEIPT
            and "sha256:" + hashlib.sha256(data).hexdigest() == digest)
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        items = archive.infolist()
        require(len(items) == 1)
        item = items[0]
        require(item.filename == "actions-audit.json" and not item.is_dir()
                and not item.flag_bits & 1 and 0 < item.file_size <= MAX_RECEIPT)
        with archive.open(item) as stream:
            payload = stream.read(MAX_RECEIPT + 1)
        require(len(payload) == item.file_size)
    return json.loads(payload, object_pairs_hook=unique_object,
                      parse_constant=lambda _: require(False))


def entries_from(report, repository_id, policy, producer):
    require(type(report) is dict and type(report.get("schema")) is int and report["schema"] == 2
            and report.get("status") == "passed" and type(report.get("finding_count")) is int
            and report["finding_count"] == 0
            and report.get("findings") == [] and report.get("coverage_gaps") == [])
    proof = report.get("coverage")
    require(type(proof) is dict and set(proof) == {
        "schema", "repository_id", "policy", "producer", "attempts"})
    require(type(proof["schema"]) is int and proof["schema"] == 1
            and type(proof["repository_id"]) is int and proof["repository_id"] == repository_id
            and proof["policy"] == policy and proof["producer"] == producer)
    recorded = proof["producer"]
    require(type(recorded) is dict and set(recorded) == set(producer))
    identifier(recorded["run_id"])
    identifier(recorded["workflow_id"])
    commit(recorded["head_sha"])
    require(type(recorded["attempt"]) is int and 1 <= recorded["attempt"] <= 100)
    rows = proof["attempts"]
    require(type(rows) is list and len(rows) <= MAX_ENTRIES)
    result = {}
    for entry in rows:
        require(type(entry) is dict and set(entry) == ENTRY_KEYS)
        identifier(entry["run_id"])
        identifier(entry["workflow_id"])
        identifier(entry["head_repository_id"], zero=True)
        commit(entry["head_sha"])
        require(type(entry["attempt"]) is int and 1 <= entry["attempt"] <= 100)
        require(type(entry["state"]) is str and entry["state"] in {"scanned", "no-runner-execution"}
                and type(entry["members"]) is int and 0 <= entry["members"] <= 10000
                and (entry["state"] != "no-runner-execution" or entry["members"] == 0))
        key = (entry["run_id"], entry["attempt"])
        require(key not in result)
        result[key] = entry
    scanned = sum(row["state"] == "scanned" for row in rows)
    members = sum(row["members"] for row in rows)
    for name in ("attempts_examined", "archives_scanned", "archives_reused",
                 "members_scanned", "members_reused", "attempts_without_execution",
                 "attempts_reused"):
        require(type(report.get(name)) is int and 0 <= report[name] < 2 ** 63)
    require(report["attempts_examined"] == len(rows)
            and report["archives_scanned"] + report["archives_reused"] == scanned
            and report["members_scanned"] + report["members_reused"] == members
            and report["attempts_without_execution"] == len(rows) - scanned
            and report["attempts_reused"] <= len(rows))
    return result


class Coverage:
    def __init__(self, api, full=False, policy=None):
        self.api = api
        self.full = full
        self.policy = policy
        self.repository_id = None
        self.workflow_id = None
        self.baseline = None
        self.entries = {}

    def remote_policy(self, sha):
        files = {}
        for name in FILES:
            try:
                item = self.api.get("/contents/" + name + "?ref=" + sha)
            except Exception as error:
                if getattr(error, "status", None) == 404:
                    # Old scanner revisions have no coverage helper: incompatible
                    # policy, never evidence authorizing reuse.
                    return None
                raise
            require(item.get("type") == "file" and item.get("encoding") == "base64"
                    and type(item.get("size")) is int and 0 <= item["size"] <= 128 * 1024
                    and isinstance(item.get("content"), str)
                    and len(item["content"]) <= 180000)
            files[name] = base64.b64decode(item["content"].replace("\n", ""), validate=True)
            require(len(files[name]) == item["size"])
        return policy_digest(files)

    def load(self):
        api = self.api
        repository = api.get("")
        require(repository.get("full_name") == api.repository)
        self.repository_id = identifier(repository["id"])
        default = repository["default_branch"]
        require(isinstance(default, str) and 0 < len(default) <= 255)
        ref = api.get("/git/ref/heads/" + urllib.parse.quote(default, safe=""))
        require(ref.get("ref") == "refs/heads/" + default
                and ref["object"]["type"] == "commit")
        default_sha = commit(ref["object"]["sha"])
        workflow = api.get("/actions/workflows/security-audit.yml")
        require(workflow.get("path") == WORKFLOW and workflow.get("state") == "active")
        self.workflow_id = identifier(workflow["id"])
        self.policy = self.policy or local_policy()
        if self.full:
            return
        path = ("/actions/workflows/" + str(self.workflow_id)
                + "/runs?status=success&branch=" + urllib.parse.quote(default, safe="")
                + "&per_page=" + str(MAX_CANDIDATES))
        runs = api.get(path)["workflow_runs"]
        require(type(runs) is list and len(runs) <= MAX_CANDIDATES)
        policies = {}
        for run in runs:
            head_repository = run.get("head_repository") or {}
            if (run.get("workflow_id") != self.workflow_id
                    or run.get("head_branch") != default
                    or head_repository.get("id") != self.repository_id
                    or head_repository.get("full_name") != api.repository
                    or run.get("event") not in {"push", "schedule", "workflow_dispatch"}
                    or run.get("status") != "completed"
                    or run.get("conclusion") != "success"):
                continue
            binding = run_identity(run)
            sha = binding["head_sha"]
            if sha not in policies:
                ancestry = api.get("/compare/" + sha + "..." + default_sha + "?per_page=1")
                if (ancestry.get("status") not in {"ahead", "identical"}
                        or ancestry.get("merge_base_commit", {}).get("sha") != sha):
                    policies[sha] = None
                else:
                    policies[sha] = self.remote_policy(sha)
            if policies[sha] != self.policy:
                continue
            number = run.get("run_attempt")
            require(type(number) is int and 1 <= number <= 100)
            producer = {"run_id": binding["run_id"], "attempt": number,
                        "head_sha": sha, "workflow_id": self.workflow_id}
            prefix = "/actions/runs/" + str(binding["run_id"])
            jobs = list(api.pages(prefix + "/attempts/" + str(number) + "/jobs", "jobs"))
            for name in ("source-history", "actions-logs"):
                matching = [job for job in jobs if job.get("name") == name]
                require(len(matching) == 1 and matching[0].get("status") == "completed"
                        and matching[0].get("conclusion") == "success")
            name = "security-actions-" + str(binding["run_id"]) + "-" + str(number)
            artifacts = [item for item in api.pages(prefix + "/artifacts", "artifacts")
                         if item.get("name") == name]
            require(len(artifacts) == 1)
            artifact = artifacts[0]
            artifact_run = artifact.get("workflow_run") or {}
            require(artifact.get("expired") is False
                    and type(artifact.get("size_in_bytes")) is int
                    and 0 < artifact["size_in_bytes"] <= MAX_RECEIPT
                    and artifact_run.get("id") == binding["run_id"]
                    and artifact_run.get("repository_id") == self.repository_id
                    and artifact_run.get("head_repository_id") == self.repository_id
                    and artifact_run.get("head_sha") == sha
                    and artifact_run.get("head_branch") == default)
            expires = datetime.fromisoformat(artifact["expires_at"].replace("Z", "+00:00"))
            require(expires.tzinfo is not None and expires > datetime.now(timezone.utc))
            require(isinstance(artifact.get("digest"), str)
                    and re.fullmatch(r"sha256:[0-9a-f]{64}", artifact["digest"]))
            artifact_id = identifier(artifact["id"])
            data = api.get("/actions/artifacts/" + str(artifact_id) + "/zip",
                           archive=True, limit=MAX_RECEIPT)
            report = receipt_json(data, artifact.get("digest"))
            self.entries = entries_from(report, self.repository_id, self.policy, producer)
            self.baseline = producer
            return
        # No compatible default-source receipt exists: actual full bootstrap.
        # An applicable but missing/invalid receipt above never silently falls back.

    def covered(self, binding, attempt):
        entry = self.entries.get((binding["run_id"], attempt))
        if entry is None:
            return None
        require(all(entry[name] == value for name, value in binding.items()))
        # A deleted/unknown head repository cannot certify reuse by identity.
        return dict(entry) if binding["head_repository_id"] else None

    def receipt(self, entries):
        def environment_id(name):
            value = os.environ.get(name, "")
            require(re.fullmatch(r"[0-9]{1,18}", value))
            return identifier(int(value))
        producer = {"run_id": environment_id("GITHUB_RUN_ID"),
                    "attempt": environment_id("GITHUB_RUN_ATTEMPT"),
                    "head_sha": commit(os.environ.get("GITHUB_SHA")),
                    "workflow_id": self.workflow_id}
        require(producer["attempt"] <= 100 and len(entries) <= MAX_ENTRIES)
        return {"schema": 1, "repository_id": self.repository_id,
                "policy": self.policy, "producer": producer, "attempts": entries}
