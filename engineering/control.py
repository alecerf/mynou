"""Operational GitHub control commands. Tests and policy checks remain CI-only."""
import argparse
import json
import sys
import uuid

from github import APIError, GitHub, ROOT, control_reference
import lease


def labels(issue):
    return {v["name"] for v in issue.get("labels", [])}


def label_update(api, number, family, value):
    issue = api.rest("GET", f"issues/{number}")
    kept = sorted(x for x in labels(issue) if not x.startswith(family + ":"))
    api.rest("PATCH", f"issues/{number}", {"labels": kept + [family + ":" + value]})


def read_state(api):
    cfg = api.cfg
    head = api.ref(control_reference(cfg))
    return head, lease.validate(api.file(cfg["state_path"], head))


def save(api, expected, value, message):
    lease.validate(value)
    return api.cas_file(control_reference(api.cfg), expected, api.cfg["state_path"], value, message)


def handoff(api, previous, checkpoint, control_sha):
    """Native Issue event wakes trusted-default delivery after durable release."""
    if any(checkpoint.get(key) != previous[key] for key in ["issue", "role", "branch", "commit"]) or lease.time(checkpoint.get("at")) < lease.time(previous["acquired_at"]):
        raise ValueError("Checkpoint the current work before releasing its lease")
    body = ("<!-- mynou-transition:v1 -->\n"
        f"{previous['role']} released execution for #{previous['issue']}. "
        f"Control checkpoint: `{control_sha}`; source: `{previous['commit']}`.\n\n"
        + checkpoint["summary"] + "\n\nNext: " + checkpoint["next_action"])
    api.rest("POST", f"issues/{previous['issue']}/comments", {"body": body})


def select(state, issues, at):
    if lease.valid(state, at):
        return {"action": "busy", "lease": state["lease"], "instruction": "Do no engineering work; exit without polling."}
    if state["lease"] is not None:
        return {"action": "recover", "issue": state["lease"]["issue"], "instruction": "Inspect preserved Issue, branch, PR and CI before reclaiming."}
    managed = [i for i in issues if i.get("state") == "open" and "pull_request" not in i and "agent-work" in labels(i)]
    active = [i for i in managed if labels(i) & {"status:in-progress", "status:review"}]
    if active:
        return {"action": "recover", "issue": min(active, key=lambda i: i["number"])["number"], "instruction": "Finish/recover existing delivery before unrelated work."}
    circuit = [i for i in managed if state["attempts"].get(str(i["number"]), {}).get("unchanged", 0) >= 3]
    if circuit:
        return {"action": "triage", "issue": min(circuit, key=lambda i: i["number"])["number"], "instruction": "Circuit breaker requires a changed approach; never repeat unchanged implementation."}
    ready = [i for i in managed if "status:blocked" not in labels(i)]
    if not ready:
        return {"action": "idle", "instruction": "No executable valuable backlog. Do not invent work or repeatedly rescan."}
    def priority(issue):
        names = labels(issue)
        p = next((n for n in range(4) if f"priority:p{n}" in names), 2)
        return p, "origin:user" not in names, issue["number"]
    chosen = min(ready, key=priority)
    attempt = state["attempts"].get(str(chosen["number"]), {})
    if attempt.get("unchanged", 0) >= 3:
        return {"action": "triage", "issue": chosen["number"], "instruction": "Circuit breaker requires a changed approach before implementation resumes."}
    return {"action": "triage" if not labels(chosen) & {"status:ready"} else "work",
        "issue": chosen["number"], "instruction": "Acquire one role lease and execute one bounded transition."}


def recover_native(api, state, at):
    held = state["lease"]
    if held is None or lease.valid(state, at):
        raise ValueError("No expired lease to recover")
    issue = api.rest("GET", f"issues/{held['issue']}")
    preserved = api.rest("GET", "git/commits/" + held["commit"])
    try:
        head = api.ref(held["branch"])
    except APIError as error:
        if error.status != 404:
            raise
        head = api.ref(api.cfg["default_branch"])
    prs = api.pages("pulls?state=all")
    relevant = [p for p in prs if p["number"] == held["pr"] or p["head"]["ref"] == held["branch"]]
    ancestry = api.rest("GET", f"compare/{held['commit']}...{head}")
    merged = [p for p in relevant if p.get("merged_at") and p["head"]["sha"] == held["commit"]
        and p["head"]["ref"] == held["branch"] and p["base"]["ref"] == api.cfg["default_branch"]]
    if ancestry["status"] not in ("ahead", "identical") and not merged:
        raise ValueError("Interrupted commit is not preserved on the branch/default or an exact merged PR; preserve useful work first")
    runs = api.pages("actions/runs?head_sha=" + head, "workflow_runs")
    evidence = {"issue": True, "branch": True, "pr": True, "ci": True, "commit_preserved": preserved["sha"] == held["commit"],
        "issue_state": issue["state"], "branch_head": head,
        "preservation": "exact native merged PR head" if merged else "branch/default ancestry",
        "prs": [{"number": p["number"], "state": p["state"], "head": p["head"]["sha"], "merged_at": p.get("merged_at")} for p in relevant],
        "runs": [{"id": r["id"], "status": r["status"], "conclusion": r["conclusion"]} for r in runs[:10]]}
    return lease.recover(state, at, evidence)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["wake", "status", "acquire", "checkpoint", "release", "recover", "attempt", "qa-gate", "deliver", "branch-sweep", "branch-cleanup"])
    parser.add_argument("--role")
    parser.add_argument("--issue", type=int)
    parser.add_argument("--worker", default="codex-worker")
    parser.add_argument("--branch")
    parser.add_argument("--commit")
    parser.add_argument("--pr", type=int)
    parser.add_argument("--lease")
    parser.add_argument("--summary")
    parser.add_argument("--next")
    parser.add_argument("--approach")
    parser.add_argument("--fingerprint")
    parser.add_argument("--wait-seconds", type=int, default=0)
    args = parser.parse_args()
    api = GitHub()
    if args.command == "qa-gate":
        import qa
        if not args.pr:
            raise ValueError("PR number is required")
        print(json.dumps(qa.wait_for_review(api, args.pr, args.wait_seconds), indent=2))
        return
    if args.command in ("deliver", "branch-sweep"):
        import delivery
        print(json.dumps(delivery.run(api, args.command == "branch-sweep"), indent=2))
        return
    head, state = read_state(api)
    at = lease.now()
    if args.command == "branch-cleanup":
        import delivery
        held = lease.owned(state, args.lease, at)
        if held["role"] not in ("quality", "triage") or not args.branch or not args.commit:
            raise ValueError("Cleanup requires Quality/Triage ownership and an exact branch head")
        print(json.dumps(delivery.cleanup_branch(api, args.branch, args.commit, held), indent=2))
        return
    if args.command == "status":
        print(json.dumps({"control_sha": head, "state": state}, indent=2))
        return
    if args.command == "wake":
        # Admission happens before any backlog/domain investigation.
        issues = [] if state["lease"] is not None else api.pages("issues?state=open&labels=agent-work")
        action = select(state, issues, at)
        if action["action"] == "idle":
            import qa
            prs = api.pages("pulls?state=open")
            if prs:
                action = {"action": "triage-pr", "pr": min(prs, key=lambda p: p["number"])["number"], "instruction": "Recover open PR and create/link a meaningful Issue if missing; inspect CI before new work."}
        print(json.dumps(dict(action, control_sha=head), indent=2))
        return
    identity = args.lease
    if args.command == "acquire":
        roles = json.loads((ROOT / "engineering/roles.json").read_text())["roles"]
        if args.role not in roles and not (args.role or "").startswith("temporary-"):
            raise ValueError("Select a registered or documented temporary specialist")
        if not args.issue or not args.branch or not args.commit:
            raise ValueError("Issue, branch and durable commit are required")
        issue = api.rest("GET", f"issues/{args.issue}")
        if issue["state"] != "open" or "agent-work" not in labels(issue):
            raise ValueError("Lease requires an open managed Issue")
        blockers = api.pages(f"issues/{args.issue}/dependencies/blocked_by")
        if any(i["state"] == "open" for i in blockers) and args.role not in ("master", "triage"):
            raise ValueError("Open native dependencies block implementation")
        api.rest("GET", "git/commits/" + args.commit)
        identity = str(uuid.uuid4())
        value = lease.acquire(state, at, identity, args.worker, args.role, args.issue, args.branch, args.commit, args.pr)
    elif args.command == "checkpoint":
        if args.commit:
            api.rest("GET", "git/commits/" + args.commit)
        value = lease.checkpoint(state, at, identity, args.summary, args.next, args.commit, args.pr)
    elif args.command == "release":
        previous = lease.owned(state, identity, at)
        if any(state["checkpoint"].get(key) != previous[key] for key in ["issue", "role", "branch", "commit"]) or lease.time(state["checkpoint"].get("at")) < lease.time(previous["acquired_at"]):
            raise ValueError("Checkpoint this work before releasing its lease")
        value = lease.release(state, at, identity)
    elif args.command == "recover":
        value = recover_native(api, state, at)
    else:
        value = lease.attempt(state, at, identity, args.approach, args.fingerprint)
    sha = save(api, head, value, "Engineering " + args.command)
    # A label failure never pretends the already durable lease transition failed.
    try:
        if args.command == "acquire":
            label_update(api, args.issue, "role", args.role if args.role in roles else "master")
            label_update(api, args.issue, "status", "review" if args.role == "qa" else "in-progress")
        elif args.command == "attempt" and value["lease"] is None:
            label_update(api, state["lease"]["issue"], "status", "blocked")
        if args.command == "release":
            handoff(api, state["lease"], value["checkpoint"], sha)
    except (APIError, RuntimeError, ValueError):
        print("Native metadata/handoff update incomplete; lease/checkpoint is durable. Recover metadata on the next CI, scheduled or worker wake.", file=sys.stderr)
    print(json.dumps({"control_sha": sha, "lease_id": identity, "state": value}, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (APIError, ValueError, RuntimeError, KeyError, TypeError) as error:
        print("Engineering command stopped: " + str(error), file=sys.stderr)
        sys.exit(1)
