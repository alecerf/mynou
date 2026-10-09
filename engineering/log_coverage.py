"""CI-only authenticated reuse of completed Actions audit coverage."""
import ast
import base64
from datetime import datetime, timedelta, timezone
import hashlib
import io
import json
import os
from pathlib import Path
import re
import urllib.parse
import zipfile

SCANNER = "engineering/secret_audit.py"
WORKFLOW = ".github/workflows/security-audit.yml"
MAX_RECEIPT = 16 * 1024 * 1024
MAX_SOURCE = 128 * 1024
MAX_ENTRIES = 60000
MAX_CANDIDATES = 12
ENTRY_KEYS = {"run_id", "attempt", "head_sha", "workflow_id",
              "head_repository_id", "state", "members"}
STATES = {"scanned", "no-runner-execution", "expired"}
PROOF_KEYS = {"schema", "repository_id", "policy", "producer", "inventory_at",
              "since", "deep", "deep_at", "complete", "attempts"}
TIME = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ")
# GitHub accepts reruns for 30 days after a run starts. A deep audit lists every
# run that could still gain an attempt after the last complete deep audit.
RERUN_WINDOW = timedelta(days=32)
# Routine audits re-list only recent runs; the next deep audit catches older reruns.
RECENT_WINDOW = timedelta(hours=6)
DEEP_INTERVAL = timedelta(hours=20)


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


def moment(value):
    require(isinstance(value, str) and TIME.fullmatch(value))
    return datetime.fromisoformat(value[:-1] + "+00:00")


def instant(value):
    return value.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def run_identity(run):
    repository = run.get("head_repository")
    repository_id = repository.get("id") if isinstance(repository, dict) else 0
    return {"run_id": identifier(run["id"]), "head_sha": commit(run["head_sha"]),
            "workflow_id": identifier(run["workflow_id"]),
            "head_repository_id": identifier(repository_id, zero=True)}


def policy_digest(rules, version):
    """Receipts stay reusable until the detection rules or scanner version change."""
    require(type(version) is int and 0 < version < 2 ** 31 and type(rules) is dict
            and 0 < len(rules) <= 64 and all(type(name) is str for name in rules))
    digest = hashlib.sha256(b"mynou-log-coverage-v2\0" + str(version).encode("ascii") + b"\0")
    for name in sorted(rules):
        pattern = rules[name]
        require(re.fullmatch(r"[a-z0-9-]{1,64}", name)
                and type(pattern) is bytes and 0 < len(pattern) <= 4096)
        digest.update(name.encode("ascii") + b"\0" + len(pattern).to_bytes(8, "big") + pattern)
    return digest.hexdigest()


def policy_from_source(data):
    """The literal RULES/SCANNER_VERSION policy of one scanner revision, or None."""
    if type(data) is not bytes or len(data) > MAX_SOURCE:
        return None
    try:
        tree = ast.parse(data.decode("utf-8"))
    except (SyntaxError, UnicodeDecodeError, ValueError, MemoryError, RecursionError):
        return None
    values = {}
    for node in tree.body:
        if (isinstance(node, ast.Assign) and len(node.targets) == 1
                and isinstance(node.targets[0], ast.Name)
                and node.targets[0].id in ("RULES", "SCANNER_VERSION")):
            name = node.targets[0].id
            if name in values:
                return None
            try:
                values[name] = ast.literal_eval(node.value)
            except (ValueError, TypeError, SyntaxError, MemoryError, RecursionError):
                return None
    if set(values) != {"RULES", "SCANNER_VERSION"}:
        return None
    try:
        return policy_digest(values["RULES"], values["SCANNER_VERSION"])
    except CoverageError:
        return None


def local_policy():
    return policy_from_source((Path(__file__).resolve().parent / "secret_audit.py").read_bytes())


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
    require(type(report) is dict and type(report.get("schema")) is int and report["schema"] == 3
            and report.get("status") in ("passed", "partial")
            and type(report.get("finding_count")) is int and report["finding_count"] == 0
            and report.get("findings") == [] and report.get("coverage_gaps") == [])
    proof = report.get("coverage")
    require(type(proof) is dict and set(proof) == PROOF_KEYS)
    require(type(proof["schema"]) is int and proof["schema"] == 2
            and type(proof["repository_id"]) is int and proof["repository_id"] == repository_id
            and proof["policy"] == policy and proof["producer"] == producer)
    recorded = proof["producer"]
    require(type(recorded) is dict and set(recorded) == set(producer))
    identifier(recorded["run_id"])
    identifier(recorded["workflow_id"])
    commit(recorded["head_sha"])
    require(type(recorded["attempt"]) is int and 1 <= recorded["attempt"] <= 100)
    inventory = moment(proof["inventory_at"])
    since = None if proof["since"] is None else moment(proof["since"])
    deep_at = None if proof["deep_at"] is None else moment(proof["deep_at"])
    require(type(proof["deep"]) is bool and type(proof["complete"]) is bool
            and proof["complete"] == (report["status"] == "passed")
            and type(report.get("attempts_deferred")) is int
            and (report["attempts_deferred"] == 0) == proof["complete"]
            and (since is None or since <= inventory)
            and (deep_at is None or deep_at <= inventory)
            and (not (proof["deep"] and proof["complete"]) or deep_at == inventory))
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
        require(type(entry["state"]) is str and entry["state"] in STATES
                and type(entry["members"]) is int and 0 <= entry["members"] <= 10000
                and (entry["state"] == "scanned" or entry["members"] == 0))
        key = (entry["run_id"], entry["attempt"])
        require(key not in result)
        result[key] = entry
    require(type(report.get("attempts_recorded")) is int
            and report["attempts_recorded"] == len(rows))
    return result, {"inventory_at": inventory, "since": since, "deep": proof["deep"],
                    "deep_at": deep_at, "complete": proof["complete"]}


class Coverage:
    def __init__(self, api, full=False, policy=None):
        self.api = api
        self.full = full
        self.policy = policy
        self.repository_id = None
        self.workflow_id = None
        self.baseline = None
        self.proof = None
        self.entries = {}

    def remote_policy(self, sha):
        try:
            item = self.api.get("/contents/" + SCANNER + "?ref=" + sha)
        except Exception as error:
            if getattr(error, "status", None) == 404:
                # Old scanner revisions have no coverage helper: incompatible
                # policy, never evidence authorizing reuse.
                return None
            raise
        require(item.get("type") == "file" and item.get("encoding") == "base64"
                and type(item.get("size")) is int and 0 <= item["size"] <= MAX_SOURCE
                and isinstance(item.get("content"), str)
                and len(item["content"]) <= 180000)
        data = base64.b64decode(item["content"].replace("\n", ""), validate=True)
        require(len(data) == item["size"])
        return policy_from_source(data)

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
        require(isinstance(self.policy, str) and re.fullmatch(r"[0-9a-f]{64}", self.policy))
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
            # Scheduled audits skip the unrelated source-history job.
            jobs = [job for job in api.pages(prefix + "/attempts/" + str(number) + "/jobs", "jobs")
                    if job.get("name") == "actions-logs"]
            require(len(jobs) == 1 and jobs[0].get("status") == "completed"
                    and jobs[0].get("conclusion") == "success")
            name = "security-actions-" + str(binding["run_id"]) + "-" + str(number)
            artifacts = [item for item in api.pages(prefix + "/artifacts", "artifacts")
                         if item.get("name") == name]
            require(len(artifacts) == 1)
            artifact = artifacts[0]
            artifact_run = artifact.get("workflow_run") or {}
            require(type(artifact.get("expired")) is bool
                    and type(artifact.get("size_in_bytes")) is int
                    and 0 < artifact["size_in_bytes"] <= MAX_RECEIPT
                    and artifact_run.get("id") == binding["run_id"]
                    and artifact_run.get("repository_id") == self.repository_id
                    and artifact_run.get("head_repository_id") == self.repository_id
                    and artifact_run.get("head_sha") == sha
                    and artifact_run.get("head_branch") == default)
            expires = datetime.fromisoformat(artifact["expires_at"].replace("Z", "+00:00"))
            require(expires.tzinfo is not None)
            if artifact["expired"] or expires <= datetime.now(timezone.utc):
                # Retention removed the newest receipt. Rebuilding coverage only
                # adds scanning; failing every later audit would block it forever.
                return
            require(isinstance(artifact.get("digest"), str)
                    and re.fullmatch(r"sha256:[0-9a-f]{64}", artifact["digest"]))
            artifact_id = identifier(artifact["id"])
            data = api.get("/actions/artifacts/" + str(artifact_id) + "/zip",
                           archive=True, limit=MAX_RECEIPT)
            report = receipt_json(data, artifact.get("digest"))
            self.entries, self.proof = entries_from(report, self.repository_id, self.policy, producer)
            self.baseline = producer
            return
        # No compatible default-source receipt exists: actual full bootstrap.
        # An applicable but missing/invalid receipt above never silently falls back.

    def scope(self, now):
        """Listing lower bound (None lists every run) and whether the audit is deep."""
        if self.proof is None:
            return None, True
        deep_at = self.proof["deep_at"]
        if deep_at is None or now - deep_at >= DEEP_INTERVAL:
            return (None if deep_at is None else deep_at - RERUN_WINDOW), True
        return self.proof["inventory_at"] - RECENT_WINDOW, False

    def covered(self, binding, attempt):
        entry = self.entries.get((binding["run_id"], attempt))
        if entry is None:
            return None
        require(all(entry[name] == value for name, value in binding.items()))
        # A deleted/unknown head repository cannot certify reuse by identity.
        return dict(entry) if binding["head_repository_id"] else None

    def carried(self, listed, deep):
        """Baseline evidence for runs a routine audit did not re-list.

        A deep audit lists every run that can still gain an attempt, so anything
        it did not list is settled and leaves the receipt."""
        if deep:
            return []
        return [dict(entry) for (run_id, _), entry in self.entries.items()
                if run_id not in listed]

    def receipt(self, entries, now, since, deep, complete):
        def environment_id(name):
            value = os.environ.get(name, "")
            require(re.fullmatch(r"[0-9]{1,18}", value))
            return identifier(int(value))
        producer = {"run_id": environment_id("GITHUB_RUN_ID"),
                    "attempt": environment_id("GITHUB_RUN_ATTEMPT"),
                    "head_sha": commit(os.environ.get("GITHUB_SHA")),
                    "workflow_id": self.workflow_id}
        require(producer["attempt"] <= 100 and len(entries) <= MAX_ENTRIES)
        inventory = instant(now)
        if deep and complete:
            deep_at = inventory
        elif self.proof is not None and self.proof["deep_at"] is not None:
            deep_at = instant(self.proof["deep_at"])
        else:
            deep_at = None
        return {"schema": 2, "repository_id": self.repository_id, "policy": self.policy,
                "producer": producer, "inventory_at": inventory,
                "since": None if since is None else instant(since), "deep": deep,
                "deep_at": deep_at, "complete": complete, "attempts": entries}
