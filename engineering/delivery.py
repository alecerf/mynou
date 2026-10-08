"""Trusted-base mechanical merge and cleanup; no LLM workers or paid services."""
import uuid
from datetime import timedelta
from urllib.parse import quote

from github import APIError, control_reference, reference
from control import labels, label_update, read_state, recover_native, save
import lease
import qa


DISPATCH_VISIBILITY_GRACE = timedelta(minutes=5)


def default_ci_run(api, commit):
    if not isinstance(commit, str) or not lease.SHA.fullmatch(commit):
        raise ValueError("Default CI requires an exact source commit")
    branch = api.cfg["default_branch"]
    runs = api.pages("actions/workflows/ci.yml/runs?head_sha=" + commit
        + "&branch=" + quote(branch, safe=""), "workflow_runs")
    eligible = [r for r in runs if r.get("head_sha") == commit and r.get("head_branch") == branch
        and r.get("event") in ("push", "workflow_dispatch")]
    return max(eligible, key=lambda r: r["id"]) if eligible else None


def checkpoint_publication(api, identity, commit, status, run=None, error=None):
    head, state = read_state(api)
    lease.owned(state, identity, lease.now())
    detail = {"commit": commit, "state": status, "at": lease.stamp(lease.now())}
    if run is not None:
        detail.update(run_id=run["id"], status=run["status"], conclusion=run.get("conclusion"))
    if error is not None:
        detail["error"] = error
    value = lease.checkpoint(state, lease.now(), identity,
        f"Default-source CI transition: {status} at {commit}; native evidence {detail}. No release publication is inferred.",
        "Inspect exact-source native CI before unrelated delivery; recover dispatch or fix actual failed CI. Actions alone publishes immutable releases.", commit)
    value["checkpoint"]["publication"] = detail
    save(api, head, value, "Checkpoint default-source CI " + status)
    return detail


def dispatch_default_ci(api, identity, commit):
    _, state = read_state(api)
    owner = lease.owned(state, identity, lease.now())
    if owner["role"] not in ("quality", "triage") or owner["commit"] != commit or api.ref(api.cfg["default_branch"]) != commit:
        raise ValueError("Default source or delivery ownership changed; stop before dispatch")
    run = default_ci_run(api, commit)
    if run is not None:
        status = "ci-passed" if run["status"] == "completed" and run.get("conclusion") == "success" else "ci-pending"
        if run["status"] == "completed" and status != "ci-passed":
            status = "ci-failed"
        detail = checkpoint_publication(api, identity, commit, status, run)
        if status == "ci-failed":
            raise ValueError("Exact-source default CI failed; preserve its native run and fix before further delivery")
        return detail
    previous = state["checkpoint"].get("publication", {})
    if previous.get("commit") == commit and previous.get("state") in ("dispatching", "accepted", "unknown"):
        elapsed = lease.now() - lease.time(previous["at"])
        if elapsed < DISPATCH_VISIBILITY_GRACE:
            return dict(previous, state="awaiting-visibility")
    # Record intent before the irreversible API request. A lost response/checkpoint
    # leaves a grace period for native run visibility rather than an immediate retry.
    checkpoint_publication(api, identity, commit, "dispatching")
    lease.owned(read_state(api)[1], identity, lease.now())
    if api.ref(api.cfg["default_branch"]) != commit:
        checkpoint_publication(api, identity, commit, "head-changed")
        raise ValueError("Default branch advanced before CI dispatch; triage exact-source recovery")
    try:
        api.rest("POST", "actions/workflows/ci.yml/dispatches", {"ref": api.cfg["default_branch"]})
    except APIError as error:
        checkpoint_publication(api, identity, commit, "failed", error="HTTP " + str(error.status))
        raise
    except RuntimeError:
        checkpoint_publication(api, identity, commit, "unknown", error="Transport outcome uncertain; inspect native runs before retry")
        raise
    # GitHub accepts a branch ref, not an expected-SHA dispatch condition. Detect
    # an external writer's race; never attribute a different run to this source.
    if api.ref(api.cfg["default_branch"]) != commit:
        checkpoint_publication(api, identity, commit, "head-changed")
        raise ValueError("Default branch changed during dispatch; no exact-source CI result is certified")
    return checkpoint_publication(api, identity, commit, "accepted")


def recover_publication(api, head, state, prs):
    current = api.ref(api.cfg["default_branch"])
    previous = state["checkpoint"].get("publication", {})
    if previous.get("commit") and previous["commit"] != current and previous.get("state") != "ci-passed":
        run = default_ci_run(api, previous["commit"])
        if run is None or run["status"] != "completed" or run.get("conclusion") != "success":
            raise ValueError("Unfinished exact-source publication has a changed default head; return to Triage")
    for pr in prs[:20]:
        if not pr.get("merged_at") or pr.get("merge_commit_sha") != current or pr["base"]["ref"] != api.cfg["default_branch"]:
            continue
        if (pr["head"].get("repo") or {}).get("full_name") != api.repo:
            continue
        issues = [api.rest("GET", f"issues/{n}") for n in qa.linked_issues(pr.get("body"))]
        managed = next((i for i in issues if "agent-work" in labels(i)), None)
        if managed is None:
            continue
        run = default_ci_run(api, current)
        complete = run is not None and run["status"] == "completed" and run.get("conclusion") == "success"
        # A published immutable release is also a terminal observation. Requiring
        # fresh native CI here avoids trusting a stale record while leaving an
        # unrelated active-work handoff untouched on repeated scheduler wakes.
        if complete and (previous.get("commit") != current or previous.get("state") in ("ci-passed", "published")):
            return None
        if run is not None and run["status"] != "completed" and previous.get("run_id") == run["id"]:
            return {"action": "publication-pending", "commit": current, "run": run["id"]}
        identity = str(uuid.uuid4())
        owned = lease.acquire(state, lease.now(), identity, "github-actions-publication-recovery", "triage",
            managed["number"], api.cfg["default_branch"], current, pr["number"])
        save(api, head, owned, "Acquire exact-source publication recovery")
        try:
            detail = dispatch_default_ci(api, identity, current)
            return {"action": "publication-recovery", "pr": pr["number"], "publication": detail}
        finally:
            h, s = read_state(api)
            if lease.valid(s, lease.now()) and s["lease"]["id"] == identity:
                save(api, h, lease.release(s, lease.now(), identity), "Release publication recovery")
    return None


def branch_decision(name, head, cfg, comparison, open_prs, open_issues, held, merged_prs=(), protected=False):
    authority = reference(control_reference(cfg))
    if protected or name == cfg["default_branch"] or authority == "refs/heads/" + name or name.startswith("control/") or name.startswith("release/"):
        return False, "protected or durable branch"
    if held is not None and held["branch"] == name:
        return False, "execution lease references branch"
    if any(p["head"]["ref"] == name for p in open_prs):
        return False, "open PR references branch"
    if any(name in ((i.get("body") or "") + " " + i.get("title", "")) for i in open_issues):
        return False, "open Issue preserves branch"
    merged = any(p.get("merged_at") and p["head"]["ref"] == name and p["head"]["sha"] == head for p in merged_prs)
    if comparison not in ("ahead", "identical") and not merged:
        return False, "unique or ambiguous unmerged work"
    return True, "exact merged head or commit preserved on default"


def cleanup_branch(api, name, expected, held):
    cfg = api.cfg
    branches = api.pages("branches")
    source = next((b for b in branches if b["name"] == name), None)
    if source is None:
        return {"branch": name, "result": "already absent"}
    if source["commit"]["sha"] != expected:
        return {"branch": name, "result": "preserved: head changed"}
    prs = api.pages("pulls?state=all")
    issues = api.pages("issues?state=open")
    issue_refs = [i for i in issues if "pull_request" not in i]
    if len(issue_refs) > 20:
        return {"branch": name, "result": "preserved: Issue-reference audit needs a narrower scope"}
    for issue in issue_refs:
        comments = api.pages(f"issues/{issue['number']}/comments")
        if any(name in (c.get("body") or "") for c in comments):
            return {"branch": name, "result": "preserved: open Issue comment references branch"}
    default = api.ref(cfg["default_branch"])
    comparison = api.rest("GET", f"compare/{expected}...{default}")
    allowed, reason = branch_decision(name, expected, cfg, comparison["status"],
        [p for p in prs if p["state"] == "open"], issue_refs, held,
        [p for p in prs if p.get("merged_at")], source["protected"])
    if not allowed:
        return {"branch": name, "result": "preserved: " + reason}
    # No live API offers an expected-SHA conditional ref deletion. Preserve a
    # freshly audited record in the owning Issue, and use this only under the
    # serialized delivery lease. Native delete_branch_on_merge is preferred.
    if held is None or held["role"] not in ("quality", "triage"):
        raise ValueError("Branch deletion requires an owned Quality/Triage lease")
    lease.owned(read_state(api)[1], held["id"], lease.now())
    api.rest("POST", f"issues/{held['issue']}/comments", {"body":
        f"<!-- mynou-branch-audit:v1 -->\nBranch `{name}` at `{expected}` is eligible for cleanup: {reason}. "
        f"Default `{default}`, native PRs, open Issue bodies/comments and execution ownership were inspected. "
        "Deletion still requires a fresh head and lease fence."})
    current = read_state(api)[1]
    if api.ref(name) != expected:
        return {"branch": name, "result": "preserved: head changed before cleanup"}
    owner = lease.owned(current, held["id"], lease.now())
    if owner["role"] not in ("quality", "triage") or owner["branch"] == name:
        raise ValueError("Current execution ownership protects this branch")
    api.rest("DELETE", "git/refs/heads/" + name)
    return {"branch": name, "head": expected, "result": "deleted", "reason": reason}


def cleanup_pr(api, pr):
    if not pr.get("merged_at"):
        raise ValueError("Unmerged PR cannot be cleaned up as delivered")
    for number in qa.linked_issues(pr.get("body")):
        issue = api.rest("GET", f"issues/{number}")
        if "agent-work" not in labels(issue):
            raise ValueError("Cleanup requires a managed linked Issue")
        api.rest("PATCH", f"issues/{number}", {"state": "closed", "state_reason": "completed"})
        label_update(api, number, "status", "done")
    # Repair dependencies before optional branch work can fail. Later wakes also
    # reconcile independently of whether this parent's status is already Done.
    held = read_state(api)[1]["lease"]
    reconcile_dependencies(api, held)
    return cleanup_branch(api, pr["head"]["ref"], pr["head"]["sha"], held)


def reconcile_dependencies(api, held, candidates=None):
    if held is None or held["role"] not in ("quality", "triage"):
        raise ValueError("Dependency repair requires owned delivery/Quality execution")
    changed = []
    issues = candidates if candidates is not None else api.pages("issues?state=open&labels=agent-work,status:blocked")
    for issue in issues[:20]:
        if "pull_request" in issue or issue["state"] != "open" or "status:blocked" not in labels(issue):
            continue
        blockers = api.pages(f"issues/{issue['number']}/dependencies/blocked_by")
        if blockers and all(i["state"] == "closed" for i in blockers):
            lease.owned(read_state(api)[1], held["id"], lease.now())
            label_update(api, issue["number"], "status", "ready")
            changed.append(issue["number"])
    return changed


def recover_dependencies(api, head, state):
    for issue in api.pages("issues?state=open&labels=agent-work,status:blocked")[:20]:
        if "pull_request" in issue:
            continue
        blockers = api.pages(f"issues/{issue['number']}/dependencies/blocked_by")
        if not blockers or any(i["state"] != "closed" for i in blockers):
            continue
        identity = str(uuid.uuid4())
        commit = api.ref(api.cfg["default_branch"])
        owned = lease.acquire(state, lease.now(), identity, "github-actions-dependency-recovery", "quality",
            issue["number"], api.cfg["default_branch"], commit)
        save(api, head, owned, "Recover interrupted native dependency metadata")
        try:
            changed = reconcile_dependencies(api, owned["lease"], [issue])
            h, s = read_state(api)
            s = lease.checkpoint(s, lease.now(), identity,
                f"Inspected closed native blockers and repaired readiness for Issues {changed}; parent Done status does not hide interrupted cleanup.",
                "Finish any remaining merged-PR cleanup, then execute the highest-priority ready work.")
            save(api, h, s, "Checkpoint native dependency recovery")
            return {"action": "dependency-recovery", "issues": changed}
        finally:
            h, s = read_state(api)
            if lease.valid(s, lease.now()) and s["lease"]["id"] == identity:
                save(api, h, lease.release(s, lease.now(), identity), "Release dependency recovery")
    return None


def run(api, sweep=False):
    head, state = read_state(api)
    at = lease.now()
    if lease.valid(state, at):
        return {"action": "busy", "instruction": "Mechanical delivery waits for the current engineering role to release."}
    if sweep:
        if state["lease"] is not None:
            return {"action": "recovery-needed", "issue": state["lease"]["issue"],
                "instruction": "Read-only sweep preserves expired state; recover through a delivery/worker wake."}
        return branch_audit(api)
    if state["lease"] is not None:
        recovered = recover_native(api, state, at)
        head = save(api, head, recovered, "Recover interrupted delivery lease")
        state = recovered
    prs = api.pages("pulls?state=all")
    publication = recover_publication(api, head, state, prs)
    if publication is not None:
        return publication
    repaired = recover_dependencies(api, head, state)
    if repaired is not None:
        return repaired
    candidates = [p for p in prs if p["state"] == "open" and not p["draft"] and qa.linked_issues(p.get("body"))]
    result = []
    # One useful transition per wake. No concurrently active work items.
    for candidate in sorted(candidates, key=lambda p: p["number"])[:5]:
        try:
            proof = qa.evaluate(api, candidate["number"], include_gate=True)
        except ValueError as error:
            result.append({"pr": candidate["number"], "blocked": str(error)})
            continue
        identity = str(uuid.uuid4())
        held = lease.acquire(state, lease.now(), identity, "github-actions-delivery", "triage",
            proof["issues"][0], candidate["head"]["ref"], proof["head"], candidate["number"])
        head = save(api, head, held, "Acquire serialized objective delivery")
        try:
            # Refresh every gate after acquisition; review or CI may have changed.
            proof = qa.evaluate(api, candidate["number"], include_gate=True)
            current = api.rest("GET", f"pulls/{candidate['number']}")
            lease.owned(read_state(api)[1], identity, lease.now())
            if current["mergeable"] is not True or api.ref(api.cfg["default_branch"]) != proof["base"]:
                raise ValueError("PR integration or base changed; revalidate before merge")
            merged = api.rest("PUT", f"pulls/{candidate['number']}/merge", {"sha": proof["head"], "merge_method": "merge"})
            if not merged.get("merged"):
                raise ValueError("GitHub did not confirm the merge")
            head, held = read_state(api)
            held = lease.checkpoint(held, lease.now(), identity,
                f"PR #{candidate['number']} merged at {merged['sha']}; exact QA/CI gates passed.",
                "Finish native Issue/dependency/branch cleanup, then resume backlog.", merged["sha"])
            head = save(api, head, held, "Checkpoint successful native PR merge")
            publication = dispatch_default_ci(api, identity, merged["sha"])
            final = api.rest("GET", f"pulls/{candidate['number']}")
            cleanup = cleanup_pr(api, final)
            # The currently held execution branch is kept by cleanup_branch;
            # release then a later sweep/native GitHub deletion removes it.
            result.append({"pr": candidate["number"], "merged": merged["sha"], "publication": publication, "cleanup": cleanup})
        finally:
            current_head, current_state = read_state(api)
            if lease.valid(current_state, lease.now()) and current_state["lease"]["id"] == identity:
                save(api, current_head, lease.release(current_state, lease.now(), identity), "Release mechanical delivery lease")
        return {"action": "delivered", "results": result}
    # Recover cleanup after a worker disappeared immediately following merge.
    for pr in [p for p in prs if p.get("merged_at") and qa.linked_issues(p.get("body"))][:10]:
        issues = [api.rest("GET", f"issues/{n}") for n in qa.linked_issues(pr.get("body"))]
        unfinished = any("agent-work" in labels(i) and "status:done" not in labels(i) for i in issues)
        if unfinished:
            identity = str(uuid.uuid4())
            held = lease.acquire(state, lease.now(), identity, "github-actions-cleanup", "quality", issues[0]["number"],
                pr["head"]["ref"], pr["merge_commit_sha"], pr["number"])
            head = save(api, head, held, "Recover post-merge cleanup")
            try:
                cleanup = cleanup_pr(api, pr)
                return {"action": "cleanup", "pr": pr["number"], "result": cleanup}
            finally:
                h, s = read_state(api)
                save(api, h, lease.release(s, lease.now(), identity), "Release recovered cleanup")
    return {"action": "idle-or-blocked", "results": result}


def branch_audit(api):
    """Incremental discovery only; never recover, merge, close or delete."""
    prs = api.pages("pulls?state=all")
    branches = api.pages("branches")
    open_issues = [i for i in api.pages("issues?state=open") if "pull_request" not in i]
    default = api.ref(api.cfg["default_branch"])
    audit = []
    for b in branches[:20]:
        if b["name"] == api.cfg["default_branch"] or b["name"].startswith("control/"):
            continue
        comparison = api.rest("GET", f"compare/{b['commit']['sha']}...{default}")
        candidate, reason = branch_decision(b["name"], b["commit"]["sha"], api.cfg, comparison["status"],
            [p for p in prs if p["state"] == "open"], open_issues, None, prs, b["protected"])
        audit.append({"branch": b["name"], "head": b["commit"]["sha"], "candidate": candidate, "reason": reason})
    return {"action": "quality-audit", "branches": audit,
        "instruction": "Candidates require a complete fresh PR/Issue/comment/head audit and an owned Quality/Triage lease before cleanup."}
