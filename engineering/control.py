"""Operational GitHub control commands. Tests and policy checks remain CI-only."""
import argparse
import json
import sys
import uuid

from github import APIError, GitHub, ROOT, control_reference, team_policy
import lease
import migration


def team_cap(cfg):
    policy = team_policy(cfg)
    return None if policy is None else policy["max_active_agents"]


def enable_team(api, state, at, identity, worker):
    """Enable only the exact installed, independently reviewed and CI-verified source."""
    policy = team_policy(api.cfg)
    held = lease.owned(state, identity, at)
    if policy is None or state["schema"] != 1 or held["role"] != "master" \
            or held["worker"] != worker or held["issue"] != policy["issue"]:
        raise ValueError("Team activation requires its configured serial Master owner")
    current = api.ref(api.cfg["default_branch"])
    if held["branch"] != api.cfg["default_branch"] or held["commit"] != current or held["pr"] is None:
        raise ValueError("Team activation requires the exact installed default source and linked PR")
    import qa
    proof = qa.evaluate(api, held["pr"], include_gate=True, allow_merged=True)
    if proof["merge"] != current or proof["issues"] != [policy["issue"]]:
        raise ValueError("Team activation source is not the exact reviewed linked merge")
    qa.ci_ready(api, api.cfg, current)
    from delivery import default_ci_run
    run = default_ci_run(api, current)
    if run is None or run["status"] != "completed" or run.get("conclusion") != "success":
        raise ValueError("Team activation requires successful exact-default CI/publication")
    if api.ref(api.cfg["default_branch"]) != current:
        raise ValueError("Installed source changed during team activation review")
    result = lease.enable_team(state, at, identity, policy["issue"], policy["max_active_agents"])
    result["checkpoint"]["team"].update(policy_commit=current, pr=held["pr"],
        reviews=proof["reviews"], ci=proof["ci"], default_run=run["id"])
    return result


def labels(issue):
    return {v["name"] for v in issue.get("labels", [])}


def label_update(api, number, family, value):
    issue = api.rest("GET", f"issues/{number}")
    kept = sorted(x for x in labels(issue) if not x.startswith(family + ":"))
    api.rest("PATCH", f"issues/{number}", {"labels": kept + [family + ":" + value]})


def read_state(api):
    _, head, state = migration.resolve(api)
    return head, state


def save(api, expected, value, message):
    lease.validate(value)
    location, head, _ = migration.resolve(api)
    if head != expected:
        raise RuntimeError("Execution authority changed; stop and reconstruct state")
    return api.cas_file(location, expected, api.cfg["state_path"], value, message)


def handoff(api, previous, checkpoint, control_sha):
    """Native Issue event wakes trusted-default delivery after durable release."""
    if any(checkpoint.get(key) != previous[key] for key in ["issue", "role", "branch", "commit"]) or lease.time(checkpoint.get("at")) < lease.time(previous["acquired_at"]):
        raise ValueError("Checkpoint the current work before releasing its lease")
    body = ("<!-- mynou-transition:v1 -->\n"
        f"{previous['role']} released execution for #{previous['issue']}. "
        f"Control checkpoint: `{control_sha}`; source: `{previous['commit']}`.\n\n"
        + checkpoint["summary"] + "\n\nNext: " + checkpoint["next_action"])
    api.rest("POST", f"issues/{previous['issue']}/comments", {"body": body})


def product_planning(state, issues, policy, at):
    """Select one native planning review after new evidence, never every wake."""
    if not policy:
        return None
    anchor = next((i for i in issues if i["number"] == policy["issue"]), None)
    if anchor is None:
        return None
    count = sum(
        i["number"] != anchor["number"]
        and "status:ready" in labels(i)
        and bool(labels(i) & {"origin:agent", "origin:ux", "origin:reliability"})
        and bool(labels(i) & {"enhancement", "bug", "accessibility"})
        and i.get("issue_dependencies_summary", {}).get("blocked_by") == 0
        for i in issues
    )
    if count >= policy["ready_minimum"]:
        return None
    armed = bool(labels(anchor) & {"status:ready", "status:needs-triage"})
    if not armed and "status:blocked" in labels(anchor):
        published = state["checkpoint"].get("publication", {})
        if published.get("state") == "published":
            try:
                published_at = lease.time(published.get("at"))
                armed = lease.time(anchor.get("updated_at")) < published_at <= at
            except (ValueError, TypeError):
                armed = False
    if not armed:
        return None
    return {"action": "plan-product", "issue": anchor["number"], "role": "product",
        "ready_product_items": count,
        "instruction": "Acquire one Product lease on the configured planning Issue; review known evidence, maintain bounded native proposals, checkpoint and return the Issue to Blocked. Do not invent work."}


def priority(issue):
    names = labels(issue)
    p = next((n for n in range(4) if f"priority:p{n}" in names), 2)
    return p, "origin:user" not in names, issue["number"]


def select_team(state, issues, at, planning=None):
    """Assign Issues up to capacity; drain when the next priority is exclusive."""
    items = []
    if state["lease"] is not None and not lease.valid(state, at):
        items.append({"action": "recover", "issue": state["lease"]["issue"], "serial": True})
    for held in sorted(lease.expired(state, at), key=lambda w: w["issue"]):
        items.append({"action": "recover", "issue": held["issue"], "lease": held["id"]})
    running = lease.live(state, at)
    if items:
        return {"action": "recover", "issue": items[0]["issue"], "capacity": 0,
            "valid_workers": len(running), "assignments": items,
            "instruction": "Inspect and preserve the listed expired execution records before assigning unrelated work."}
    serial = state["lease"]
    taken = {w["issue"] for w in state["workers"].values()}
    if serial is not None:
        taken.add(serial["issue"])
    managed = [i for i in issues if i.get("state") == "open" and "pull_request" not in i
        and "agent-work" in labels(i) and i["number"] not in taken]
    queue = [("recover", i) for i in sorted(managed, key=lambda i: i["number"])
        if labels(i) & {"status:in-progress", "status:review"}]
    queue += [("triage", i) for i in sorted(managed, key=lambda i: i["number"])
        if state["attempts"].get(str(i["number"]), {}).get("unchanged", 0) >= 3
        and not labels(i) & {"status:in-progress", "status:review"}]
    planned = product_planning(state, managed, planning, at)
    ready = [i for i in managed if "status:blocked" not in labels(i)
        and not labels(i) & {"status:in-progress", "status:review"}
        and state["attempts"].get(str(i["number"]), {}).get("unchanged", 0) < 3
        and (not planning or i["number"] != planning["issue"])]
    if planned and not any("origin:user" in labels(i) or "priority:p0" in labels(i)
            or bool(labels(i) & {"origin:security", "risk:critical"}) for i in ready):
        anchor = next(i for i in managed if i["number"] == planned["issue"])
        queue.append(("plan-product", anchor))
    queue += [("triage" if "status:ready" not in labels(i) else "work", i) for i in sorted(ready, key=priority)]
    assignments, used = [], {a for w in running for a in w["areas"]}
    free = slots = state["max_active_agents"] - len(running)
    blocked = any(w["exclusive"] for w in running) or serial is not None and serial["role"] not in {"quality", "triage"}
    for action, issue in queue:
        try:
            areas, exclusive = lease.scope(labels(issue))
        except ValueError:
            continue  # Malformed area labels fail closed for that Issue only.
        if blocked or free <= 0:
            break
        if exclusive:
            # Never starve exclusive work by starting lower-priority parallel work.
            if serial is None and not running and not assignments:
                assignments.append({"action": action, "issue": issue["number"], "areas": areas, "exclusive": True})
            break
        if used & set(areas):
            continue
        assignments.append({"action": action, "issue": issue["number"], "areas": areas, "exclusive": False})
        used |= set(areas)
        free -= 1
    items += assignments
    head = {"capacity": max(slots, 0), "valid_workers": len(running), "assignments": items}
    if items:
        return dict(head, action=items[0]["action"], issue=items[0]["issue"],
            instruction="Recover listed expired leases first, then acquire one Issue lease per assignment; the server re-checks areas and capacity.")
    if blocked or running and not queue:
        return dict(head, action="busy", instruction="Do no unrelated work; an exclusive or running worker owns the team.")
    if not queue:
        return dict(head, action="idle", instruction="No executable valuable backlog or new planning evidence. Do not invent work or repeatedly rescan.")
    return dict(head, action="no-capacity", instruction="Worker cap reached or areas overlap; wait for a release without polling.")


def select(state, issues, at, planning=None):
    if lease.is_team(state):
        return select_team(state, issues, at, planning)
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
    planned = product_planning(state, managed, planning, at)
    ready = [i for i in managed if "status:blocked" not in labels(i)
        and (not planning or i["number"] != planning["issue"])]
    if planned and not any(
        "origin:user" in labels(i) or "priority:p0" in labels(i)
        or bool(labels(i) & {"origin:security", "risk:critical"}) for i in ready
    ):
        return planned
    if not ready:
        return {"action": "idle", "instruction": "No executable valuable backlog or new planning evidence. Do not invent work or repeatedly rescan."}
    chosen = min(ready, key=priority)
    attempt = state["attempts"].get(str(chosen["number"]), {})
    if attempt.get("unchanged", 0) >= 3:
        return {"action": "triage", "issue": chosen["number"], "instruction": "Circuit breaker requires a changed approach before implementation resumes."}
    return {"action": "triage" if not labels(chosen) & {"status:ready"} else "work",
        "issue": chosen["number"], "instruction": "Acquire one role lease; continue ready transitions under execution_mode, checkpointing and releasing between roles."}


def recovery_evidence(api, held):
    """Inspect native work preservation for expired or deliberately released work."""
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
    default_merge = []
    if ancestry["status"] not in ("ahead", "identical") and not merged:
        import qa
        parents = {p["sha"] for p in preserved.get("parents", [])}
        default_merge = [p for p in relevant if p["number"] == held["pr"] and p.get("merged_at")
            and p.get("merge_commit_sha") == held["commit"] and p["base"]["ref"] == api.cfg["default_branch"]
            and (p["head"].get("repo") or {}).get("full_name") == api.repo
            and p["head"]["ref"] == held["branch"] and p["head"]["sha"] in parents
            and held["issue"] in qa.linked_issues(p.get("body"))]
        if default_merge:
            default = api.ref(api.cfg["default_branch"])
            comparison = api.rest("GET", f"compare/{held['commit']}...{default}")
            if comparison["status"] not in ("ahead", "identical"):
                default_merge = []
    if ancestry["status"] not in ("ahead", "identical") and not merged and not default_merge:
        raise ValueError("Interrupted commit is not preserved on the branch/default or an exact merged PR; preserve useful work first")
    runs = api.pages("actions/runs?head_sha=" + head, "workflow_runs")
    evidence = {"issue": True, "branch": True, "pr": True, "ci": True, "commit_preserved": preserved["sha"] == held["commit"],
        "issue_state": issue["state"], "branch_head": head,
        "preservation": "exact native default merge and source parent" if default_merge else "exact native merged PR head" if merged else "branch/default ancestry",
        "prs": [{"number": p["number"], "state": p["state"], "head": p["head"]["sha"], "merged_at": p.get("merged_at")} for p in relevant],
        "runs": [{"id": r["id"], "status": r["status"], "conclusion": r["conclusion"]} for r in runs[:10]]}
    return evidence


def recover_native(api, state, at, identity=None):
    if identity is not None:
        held = state.get("workers", {}).get(identity)
        if held is None or lease.time(held["expires_at"]) > at:
            raise ValueError("No expired worker lease to recover")
        return lease.recover_worker(state, at, identity, recovery_evidence(api, held))
    held = state["lease"]
    if held is None or lease.valid(state, at):
        raise ValueError("No expired lease to recover")
    return lease.recover(state, at, recovery_evidence(api, held))


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
    try:
        head, state = read_state(api)
    except migration.Pending as pending:
        if args.command == "checkpoint":
            held = lease.owned(pending.state, args.lease, lease.now())
            if (args.commit is not None and args.commit != held["commit"]) or (args.pr is not None and args.pr != held["pr"]):
                raise ValueError("Fenced checkpoints preserve the installed source scope")
            print(json.dumps(migration.checkpoint_fence(api, pending.head, args.worker,
                args.lease, args.summary, args.next), indent=2))
            return
        if args.command == "release":
            print(json.dumps(migration.release_fence(api, pending.head, args.worker, args.lease), indent=2))
            return
        if args.command not in ("wake", "status"):
            raise
        print(json.dumps({"action": "busy" if lease.valid(pending.state, lease.now()) else "recover-fence",
            "control_sha": pending.head, "lease": pending.state["lease"],
            "instruction": "Exit when busy; otherwise recover the native interrupted fence before domain work."}))
        return
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
        busy = lease.valid(state, at) if not lease.is_team(state) else (
            any(w["exclusive"] for w in lease.live(state, at))
            or state["lease"] is not None and state["lease"]["role"] not in {"quality", "triage"})
        issues = [] if busy or state["lease"] is not None and not lease.is_team(state) or lease.expired(state, at) else api.pages("issues?state=open&labels=agent-work")
        action = select(state, issues, at, api.cfg.get("product_planning"))
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
        if lease.is_team(state):
            if team_cap(api.cfg) is None or state["max_active_agents"] > team_cap(api.cfg):
                raise ValueError("Team mode is not enabled by reviewed configuration")
            value = lease.acquire_worker(state, at, identity, args.worker, args.role, args.issue,
                args.branch, args.commit, args.pr, sorted(labels(issue)))
        else:
            value = lease.acquire(state, at, identity, args.worker, args.role, args.issue, args.branch, args.commit, args.pr)
    elif args.command == "recover":
        value = recover_native(api, state, at, identity)
    else:
        worker = identity is not None and identity in state.get("workers", {})
        if args.command == "checkpoint":
            if args.commit:
                api.rest("GET", "git/commits/" + args.commit)
            value = (lease.checkpoint_worker if worker else lease.checkpoint)(state, at, identity, args.summary, args.next, args.commit, args.pr)
        elif args.command == "release":
            previous = lease.holder(state, identity, at)
            record = previous["checkpoint"] if worker else state["checkpoint"]
            if record is None or any(record.get(key) != previous[key] for key in ["issue", "role", "branch", "commit"]) or lease.time(record.get("at")) < lease.time(previous["acquired_at"]):
                raise ValueError("Checkpoint this work before releasing its lease")
            value = (lease.release_worker if worker else lease.release)(state, at, identity)
        else:
            previous = lease.holder(state, identity, at)
            value = (lease.attempt_worker if worker else lease.attempt)(state, at, identity, args.approach, args.fingerprint)
    sha = save(api, head, value, "Engineering " + args.command)
    # A label failure never pretends the already durable lease transition failed.
    try:
        if args.command == "acquire":
            label_update(api, args.issue, "role", args.role if args.role in roles else "master")
            label_update(api, args.issue, "status", "review" if args.role == "qa" else "in-progress")
        elif args.command == "attempt" and (previous["id"] not in value.get("workers", {}) if worker else value["lease"] is None):
            label_update(api, previous["issue"], "status", "blocked")
        if args.command == "release":
            handoff(api, previous, record, sha)
    except (APIError, RuntimeError, ValueError):
        print("Native metadata/handoff update incomplete; lease/checkpoint is durable. Recover metadata on the next CI, scheduled or worker wake.", file=sys.stderr)
    print(json.dumps({"control_sha": sha, "lease_id": identity, "state": value}, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (APIError, ValueError, RuntimeError, KeyError, TypeError) as error:
        print("Engineering command stopped: " + str(error), file=sys.stderr)
        sys.exit(1)

