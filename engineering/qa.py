"""Head-bound logical QA gate using native reviews, CI and historical leases."""
import json
import re
import time as waiting

import lease
from github import control_reference

RECORD = re.compile(r"```json\s*\n(.*?)\n```", re.S)
KEYS = {"schema", "role", "head_sha", "base_sha", "lease_checkpoint", "verdict",
    "summary", "reviewed_paths", "acceptance", "evidence", "findings"}


def linked_issues(body):
    return sorted({int(n) for n in re.findall(r"\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s+#([1-9][0-9]*)\b", body or "", re.I)})


def record(body, role):
    marker = f"<!-- mynou-{role}:v1 -->"
    if marker not in (body or ""):
        return None
    if len(body) > 16_384 or body.count(marker) != 1:
        raise ValueError("Review record is oversized or ambiguous")
    matches = RECORD.findall(body.split(marker, 1)[1])
    if len(matches) != 1:
        raise ValueError("Review requires exactly one JSON record")
    value = json.loads(matches[0])
    if not isinstance(value, dict) or set(value) != KEYS or value["schema"] != 1 or value["role"] != role:
        raise ValueError("Invalid logical review schema or role")
    if any(not isinstance(value[k], str) or not lease.SHA.fullmatch(value[k]) for k in ["head_sha", "base_sha", "lease_checkpoint"]):
        raise ValueError("Review requires exact commit bindings")
    if value["verdict"] not in ("passed", "rejected") or not isinstance(value["summary"], str) or not 1 <= len(value["summary"]) <= 2000:
        raise ValueError("Invalid review verdict or summary")
    for key in ["reviewed_paths", "acceptance", "evidence", "findings"]:
        values = value[key]
        if not isinstance(values, list) or len(values) > 256 or any(not isinstance(s, str) or not 1 <= len(s) <= 1000 for s in values):
            raise ValueError("Invalid review details")
    if not value["acceptance"] or not value["evidence"] or not value["reviewed_paths"]:
        raise ValueError("Review lacks actual acceptance, evidence or diff coverage")
    if (value["verdict"] == "passed" and value["findings"]) or (value["verdict"] == "rejected" and not value["findings"]):
        raise ValueError("Review verdict disagrees with its findings")
    return value


def latest(reviews, role, pr, trusted):
    chosen = None
    for review in sorted(reviews, key=lambda r: (r.get("submitted_at") or "", r["id"])):
        if review.get("user", {}).get("login") not in trusted or review.get("commit_id") != pr["head"]["sha"]:
            continue
        value = record(review.get("body"), role)
        if value is None or value["head_sha"] != pr["head"]["sha"] or value["base_sha"] != pr["base"]["sha"]:
            continue
        chosen = (review, value)
    if chosen is None:
        raise ValueError(f"Current {role} review is missing")
    review, value = chosen
    if review["state"] == "DISMISSED" or value["verdict"] != "passed":
        raise ValueError(f"Current {role} review rejects delivery or was dismissed")
    return review, value


def scope(value, paths):
    if set(value["reviewed_paths"]) != set(paths):
        raise ValueError("Review does not cover the exact complete current diff")


def historical_lease(state, review, value, role, pr, issues):
    lease.validate(state)
    held = state["lease"]
    if held is None or held["role"] != role or held["commit"] != pr["head"]["sha"] or held["pr"] != pr["number"] or held["issue"] not in issues:
        raise ValueError("Review is not bound to a distinct scoped specialist lease")
    submitted = lease.time(review["submitted_at"])
    if not lease.time(held["acquired_at"]) <= submitted < lease.time(held["expires_at"]):
        raise ValueError("Review was not submitted while its specialist lease was valid")


def ci_ready(api, cfg, head, include_gate=False):
    runs = api.pages("actions/runs?head_sha=" + head, "workflow_runs")
    evidence = []
    for workflow, names in cfg["required_workflows"].items():
        matching = [r for r in runs if r["name"] == workflow and r["head_sha"] == head]
        if not matching:
            raise ValueError("Required CI workflow is missing: " + workflow)
        run = max(matching, key=lambda r: (r["created_at"], r["id"], r.get("run_attempt", 1)))
        jobs = api.pages(f"actions/runs/{run['id']}/jobs?filter=latest", "jobs")
        required = list(names)
        if include_gate and workflow == "Engineering checks":
            required.append("agent-qa-review")
        for name in required:
            matches = [j for j in jobs if j["name"] == name]
            if len(matches) != 1 or matches[0]["status"] != "completed" or matches[0]["conclusion"] != "success":
                raise ValueError("Required CI job is not green: " + name)
        evidence.append(run["html_url"])
    return evidence


def unresolved(api, pr_number):
    owner, name = api.repo.split("/", 1)
    cursor = None
    for _ in range(10):
        value = api.graphql("""query($owner:String!,$name:String!,$number:Int!,$cursor:String) {
          repository(owner:$owner,name:$name) { pullRequest(number:$number) {
            reviewThreads(first:100,after:$cursor) { nodes { isResolved } pageInfo { hasNextPage endCursor } }
          } }
        }""", {"owner": owner, "name": name, "number": pr_number, "cursor": cursor})
        threads = value["repository"]["pullRequest"]["reviewThreads"]
        if any(not t["isResolved"] for t in threads["nodes"]):
            return True
        if not threads["pageInfo"]["hasNextPage"]:
            return False
        cursor = threads["pageInfo"]["endCursor"]
    raise ValueError("Review thread pagination limit reached")


def review_proof(api, cfg, role, reviews, pr, paths, issues):
    review, value = latest(reviews, role, pr, cfg["trusted_reviewers"])
    scope(value, paths)
    state = api.file(cfg["state_path"], value["lease_checkpoint"])
    historical_lease(state, review, value, role, pr, issues)
    control = api.ref(control_reference(cfg))
    ancestry = api.rest("GET", f"compare/{value['lease_checkpoint']}...{control}")
    if ancestry["status"] not in ("ahead", "identical"):
        raise ValueError("Review lease is not part of durable control history")
    return {"review_id": review["id"], "role": role, "lease_checkpoint": value["lease_checkpoint"]}


def evaluate(api, pr_number, cfg=None, include_gate=False):
    cfg = cfg or api.cfg
    pr = api.rest("GET", f"pulls/{pr_number}")
    if pr["state"] != "open" or pr["draft"] or pr["base"]["ref"] != cfg["default_branch"]:
        raise ValueError("Only a ready open PR against the default branch can pass")
    if pr["head"]["repo"]["full_name"] != cfg["repository"]:
        raise ValueError("External fork delivery requires explicit trusted review of its boundary")
    issues = linked_issues(pr.get("body"))
    if not issues:
        raise ValueError("PR must link a meaningful Issue with a closing keyword")
    reviews = api.pages(f"pulls/{pr_number}/reviews")
    # Read records first: an absent review consumes no repeated expensive CI queries.
    latest(reviews, "qa", pr, cfg["trusted_reviewers"])
    paths = [f["filename"] for f in api.pages(f"pulls/{pr_number}/files")]
    if not paths or len(paths) > 256:
        raise ValueError("PR diff requires 1–256 reviewed files")
    proof = [review_proof(api, cfg, "qa", reviews, pr, paths, issues)]
    risks = set()
    for number in issues:
        issue = api.rest("GET", f"issues/{number}")
        if "agent-work" not in {l["name"] for l in issue["labels"]}:
            raise ValueError("Linked Issue is outside the managed engineering backlog")
        risks.update(l["name"] for l in issue["labels"])
    if not risks & {"risk:low", "risk:medium", "risk:high", "risk:critical"}:
        raise ValueError("Linked work lacks a triaged risk classification")
    sensitive = any(p == "AGENTS.md" or p.startswith(("engineering/", ".agents/", ".github/", "src/crypto", "src/tls", "src/pki")) for p in paths)
    if sensitive or risks & {"risk:high", "risk:critical"}:
        proof.append(review_proof(api, cfg, "security", reviews, pr, paths, issues))
    if unresolved(api, pr_number):
        raise ValueError("Unresolved review conversations block delivery")
    evidence = ci_ready(api, cfg, pr["head"]["sha"], include_gate)
    return {"pr": pr_number, "head": pr["head"]["sha"], "base": pr["base"]["sha"],
        "issues": issues, "reviews": proof, "ci": evidence}


def wait_for_review(api, pr_number, seconds):
    deadline = waiting.monotonic() + min(max(seconds, 0), 600)
    while True:
        try:
            return evaluate(api, pr_number)
        except ValueError as error:
            if waiting.monotonic() >= deadline:
                raise
            print("QA gate waiting: " + str(error), flush=True)
            waiting.sleep(min(30, max(0, deadline - waiting.monotonic())))
