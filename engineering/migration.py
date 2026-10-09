"""Original fixed-reference cutover administration; no alternate live lease store."""
from copy import deepcopy
import re

from github import APIError, control_reference, ref_commit, reference
import lease

LEGACY = "refs/heads/control/engineering"
TARGET = "refs/notes/mynou-engineering"
KIND = "retired-engineering-control"
TASK_MARKER = "<!-- mynou-task-reconciliation:v1 -->"


class Pending(RuntimeError):
    def __init__(self, head, state):
        self.head, self.state = head, state
        super().__init__("Control is fenced; resume the recorded migration, never acquire legacy state")


def settings(cfg):
    value = cfg.get("control_migration")
    if value is None:
        return None
    if value != {"mode": "fenced-notes-v1", "legacy_ref": LEGACY, "target_ref": TARGET,
                 "issue": 19, "pr": 24} or reference(control_reference(cfg)) != LEGACY:
        raise ValueError("Unsupported or ambiguous control migration policy")
    return value


def optional_ref(api, location):
    try:
        return api.ref(location)
    except APIError as error:
        if error.status != 404:
            raise
        return None


def fenced(api, head, value):
    keys = {"schema", "kind", "legacy_ref", "target_ref", "parent_sha", "state"}
    if not isinstance(value, dict) or set(value) != keys or type(value["schema"]) is not int or value["schema"] != 2:
        raise ValueError("Unexpected legacy fence; preserve it")
    if value["kind"] != KIND or value["legacy_ref"] != LEGACY or value["target_ref"] != TARGET:
        raise ValueError("Legacy fence identifies a different migration")
    if not isinstance(value["parent_sha"], str) or not lease.SHA.fullmatch(value["parent_sha"]):
        raise ValueError("Fence requires an exact historical parent")
    native = api.rest("GET", "git/commits/" + head)
    if native.get("sha") != head or [p.get("sha") for p in native.get("parents", [])] != [value["parent_sha"]]:
        raise ValueError("Fence is not a sole-parent native control commit")
    state = lease.validate(value["state"])
    plan = state["checkpoint"].get("control_migration", {})
    if plan.get("phase") != "fenced" or plan.get("legacy_ref") != LEGACY or plan.get("target_ref") != TARGET:
        raise ValueError("Fence lacks its preserved migration checkpoint")
    return state


def active(api, head, state, legacy_head):
    state = lease.validate(state)
    plan = state["checkpoint"].get("control_migration", {})
    fence = plan.get("fence_sha")
    if plan.get("phase") not in ("active", "retired") or plan.get("legacy_ref") != LEGACY or plan.get("target_ref") != TARGET:
        raise ValueError("Notes does not identify the reviewed active control migration")
    if not isinstance(fence, str) or not lease.SHA.fullmatch(fence):
        raise ValueError("Notes lacks its exact legacy fence")
    if legacy_head is not None and legacy_head != fence:
        raise ValueError("Legacy authority changed or was rolled back; stop")
    fenced(api, fence, api.file(api.cfg["state_path"], fence))
    if api.rest("GET", f"compare/{fence}...{head}").get("status") != "ahead":
        raise ValueError("Notes does not preserve legacy fence/history ancestry")
    return state


def resolve(api):
    location = control_reference(api.cfg)
    if settings(api.cfg) is None:
        head = api.ref(location)
        return location, head, lease.validate(api.file(api.cfg["state_path"], head))
    legacy_head = optional_ref(api, LEGACY)
    notes_head = optional_ref(api, TARGET)
    if legacy_head is not None:
        value = api.file(api.cfg["state_path"], legacy_head)
        if value.get("schema") == 1:
            if notes_head is not None:
                raise ValueError("Candidate or rollback conflicts with live legacy authority")
            return LEGACY, legacy_head, lease.validate(value)
        previous = fenced(api, legacy_head, value)
        if notes_head is None:
            raise Pending(legacy_head, previous)
    elif notes_head is None:
        raise ValueError("Both control references are missing; preserve history, never initialize")
    state = active(api, notes_head, api.file(api.cfg["state_path"], notes_head), legacy_head)
    return TARGET, notes_head, state


def repair_checkpoint(api, expected, worker):
    """Restore durable records that a pre-fix stale recovery replaced.

    Admits only that exact shape: retired legacy, an exact notes head whose sole
    parent passes full active admission, one recovery transition that released
    the parent's expired lease and kept no durable record. Every other field of
    the recovered state is preserved; the write is the usual sole-parent CAS.
    """
    if settings(api.cfg) is None or optional_ref(api, LEGACY) is not None:
        raise ValueError("Checkpoint repair applies only to retired notes authority")
    head = api.ref(TARGET)
    if head != expected:
        raise ValueError("Notes authority changed; refresh before repair")
    path = api.cfg["state_path"]
    broken = lease.validate(api.file(path, head))
    native = api.rest("GET", "git/commits/" + head)
    parents = [p.get("sha") for p in native.get("parents", [])]
    if native.get("sha") != head or len(parents) != 1 or not lease.SHA.fullmatch(parents[0] or ""):
        raise ValueError("Repair requires a sole-parent native notes commit")
    prior = active(api, parents[0], api.file(path, parents[0]), None)
    recovered = broken["checkpoint"]
    if (broken["lease"] is not None or prior["lease"] is None or lease.durable(recovered)
            or "recovery" not in recovered or broken["generation"] != prior["generation"] + 1
            or broken["attempts"] != prior["attempts"] or lease.time(prior["lease"]["expires_at"]) > lease.time(recovered["at"])
            or any(recovered.get(k) != prior["lease"][k] for k in ["issue", "role", "branch", "commit", "pr"])):
        raise ValueError("Notes head is not a recovery that dropped durable checkpoint records")
    value = deepcopy(broken)
    value["generation"] += 1
    value["checkpoint"] = {**lease.durable(prior["checkpoint"]), **recovered}
    active(api, head, value, None)
    if api.ref(TARGET) != head:
        raise ValueError("Notes authority changed during repair")
    sha = api.cas_file(TARGET, head, path, value, f"Restore durable checkpoint records ({worker})")
    return {"control_sha": sha, "lease_id": None}


def owned(api, expected, worker, identity, role):
    location, head, state = resolve(api)
    held = lease.owned(state, identity, lease.now())
    if head != expected or held["worker"] != worker or held["role"] != role or held["issue"] != 19:
        raise ValueError("Migration ownership/head/scope changed")
    return location, state, held


def source_ready(api, held):
    """Installation review is distinct from actual post-merge cutover proof."""
    import qa
    current = api.ref(api.cfg["default_branch"])
    if held["branch"] != api.cfg["default_branch"] or held["commit"] != current or held["pr"] != 24:
        raise ValueError("Cutover requires ownership at the exact installed default source/PR24")
    proof = qa.evaluate(api, 24, include_gate=True, allow_merged=True)
    if proof["merge"] != current or proof["issues"] != [19]:
        raise ValueError("Installed cutover source is not the reviewed linked merge")
    qa.ci_ready(api, api.cfg, current)
    from delivery import default_ci_run
    run = default_ci_run(api, current)
    if run is None or run["status"] != "completed" or run.get("conclusion") != "success":
        raise ValueError("Installed default CI/publication is not successful")
    return current


def fence(api, expected, worker, identity, task_comment):
    location, state, held = owned(api, expected, worker, identity, "master")
    if location != LEGACY or settings(api.cfg) is None:
        raise ValueError("Only the reviewed legacy authority can be fenced")
    policy = source_ready(api, held)
    task = task_proof(api, task_comment, policy)
    if optional_ref(api, TARGET) is not None:
        raise ValueError("Notes already exists; preserve it rather than overwrite")
    value = lease.checkpoint(state, lease.now(), identity,
        "Legacy admission fenced; complete state/history preserved.",
        "Activate fixed notes; checkpoint/renew the owned fence or release it for native recovery.")
    value["checkpoint"]["control_migration"] = {"phase": "fenced", "legacy_ref": LEGACY,
        "target_ref": TARGET, "policy_commit": policy, "origin_sha": expected,
        "task_receipt": task_comment, "task_id": task["task_id"]}
    # Recheck the canonical owner after all read-only source/gate investigation.
    owned(api, expected, worker, identity, "master")
    record = {"schema": 2, "kind": KIND, "legacy_ref": LEGACY, "target_ref": TARGET,
        "parent_sha": expected, "state": value}
    result = api.cas_file(LEGACY, expected, api.cfg["state_path"], record, "Fence legacy engineering authority")
    if api.ref(LEGACY) != result:
        raise ValueError("Fence ownership changed after write")
    fenced(api, result, api.file(api.cfg["state_path"], result))
    return {"control_sha": result, "control_ref": LEGACY, "phase": "fenced", "lease_id": identity}


def pending_owner(api, expected, worker, identity):
    if settings(api.cfg) is None or api.ref(LEGACY) != expected:
        raise ValueError("Migration fence changed")
    state = fenced(api, expected, api.file(api.cfg["state_path"], expected))
    held = lease.owned(state, identity, lease.now())
    plan = state["checkpoint"]["control_migration"]
    if held["worker"] != worker or held["role"] != "master" or held["issue"] != 19 or held["pr"] != 24 or held["branch"] != api.cfg["default_branch"] or held["commit"] != plan["policy_commit"]:
        raise ValueError("Only the exact owned scoped Master may advance a fence")
    if optional_ref(api, TARGET) is not None:
        raise ValueError("Notes already exists; read canonical state, never replay activation")
    return state, held


def save_fence(api, expected, state, message):
    """Preserve schema2 admission and sole-parent non-force arbitration."""
    record = {"schema": 2, "kind": KIND, "legacy_ref": LEGACY, "target_ref": TARGET,
        "parent_sha": expected, "state": lease.validate(state)}
    result = api.cas_file(LEGACY, expected, api.cfg["state_path"], record, message)
    if api.ref(LEGACY) != result or optional_ref(api, TARGET) is not None:
        raise ValueError("Fence ownership or authority changed after write")
    if fenced(api, result, api.file(api.cfg["state_path"], result)) != state:
        raise ValueError("Fence state was not confirmed")
    return {"control_sha": result, "control_ref": LEGACY, "phase": "fenced",
        "lease_id": None if state["lease"] is None else state["lease"]["id"]}


def checkpoint_fence(api, expected, worker, identity, summary, next_action):
    state, _ = pending_owner(api, expected, worker, identity)
    value = lease.checkpoint(state, lease.now(), identity, summary, next_action)
    pending_owner(api, expected, worker, identity)
    return save_fence(api, expected, value, "Checkpoint renewable fenced migration")


def release_fence(api, expected, worker, identity):
    state, held = pending_owner(api, expected, worker, identity)
    cp = state["checkpoint"]
    if any(cp.get(k) != held[k] for k in ["issue", "role", "branch", "commit", "pr"]) or lease.time(cp.get("at")) < lease.time(held["acquired_at"]):
        raise ValueError("Checkpoint this fenced phase before release")
    at = lease.now()
    value = lease.checkpoint(state, at, identity,
        "Fenced migration deliberately paused; remote progress and owner preserved.",
        "Recover the released fence from native Issue, branch/commit, PR and CI before activation.")
    value["checkpoint"]["control_migration"]["released_lease"] = deepcopy(held)
    value = lease.release(value, at, identity)
    pending_owner(api, expected, worker, identity)
    return save_fence(api, expected, value, "Release checkpointed fenced migration")


def released_fence_owner(api, expected, state):
    """Prove deliberate release against its actual still-held native parent."""
    record = api.file(api.cfg["state_path"], expected)
    previous = fenced(api, record["parent_sha"], api.file(api.cfg["state_path"], record["parent_sha"]))
    held = previous["lease"]
    cp, plan = state["checkpoint"], state["checkpoint"]["control_migration"]
    if held is None or plan.get("released_lease") != held or not lease.valid(previous, lease.time(cp.get("at"))):
        raise ValueError("Released fence lacks a valid exact-parent owner handoff")
    if state["generation"] != previous["generation"] + 2 or state["attempts"] != previous["attempts"]:
        raise ValueError("Released fence does not preserve its native transition")
    if any(cp.get(k) != held[k] for k in ["issue", "role", "branch", "commit", "pr"]) or lease.time(cp["at"]) < lease.time(held["acquired_at"]):
        raise ValueError("Released checkpoint does not bind the held phase")
    if held["role"] != "master" or held["issue"] != 19 or held["pr"] != 24 or held["branch"] != api.cfg["default_branch"] or held["commit"] != plan["policy_commit"]:
        raise ValueError("Released fence source scope changed")
    return held


def activate(api, expected, worker, identity):
    state, held = pending_owner(api, expected, worker, identity)
    source_ready(api, held)
    value = lease.checkpoint(state, lease.now(), identity,
        "Fixed notes activated with the complete legacy fence/control/review ancestry.",
        "Prove actual notes lease commands, reconcile the existing native task, then Quality may retire legacy.")
    value["checkpoint"]["control_migration"].update(phase="active", fence_sha=expected)
    import json
    content = json.dumps(value, indent=2) + "\n"
    if len(content.encode()) > 64 * 1024:
        raise ValueError("Migrated state exceeds the bounded size")
    tree = api.rest("POST", "git/trees", {"tree": [
        {"path": api.cfg["state_path"], "mode": "100644", "type": "blob", "content": content}]})
    commit = api.rest("POST", "git/commits", {"message": "Activate fenced notes authority",
        "tree": tree["sha"], "parents": [expected]})
    new_head = commit.get("sha")
    if not isinstance(new_head, str) or not lease.SHA.fullmatch(new_head):
        raise ValueError("Activation commit lacks exact identity")
    pending_owner(api, expected, worker, identity)
    created = api.rest("POST", "git/refs", {"ref": TARGET, "sha": new_head})
    if ref_commit(created, TARGET) != new_head:
        raise ValueError("Activation response disagrees; reconstruct remote state")
    location, actual, actual_state = resolve(api)
    if location != TARGET or actual != new_head or actual_state != value:
        raise ValueError("Notes activation was not confirmed")
    return {"control_sha": actual, "control_ref": TARGET, "phase": "active", "lease_id": identity}


def recover_fence(api, expected, worker, comment_id):
    if settings(api.cfg) is None or api.ref(LEGACY) != expected:
        raise ValueError("Migration fence changed")
    state = fenced(api, expected, api.file(api.cfg["state_path"], expected))
    # Valid admission exits before Issue/source/PR/CI work.
    if lease.valid(state, lease.now()):
        raise ValueError("A valid fenced owner cannot be reclaimed")
    if optional_ref(api, TARGET) is not None:
        raise ValueError("Notes already exists; recover its canonical state instead")
    import control
    if state["lease"] is None:
        previous = released_fence_owner(api, expected, state)
        evidence = control.recovery_evidence(api, previous)
        if not all(evidence.get(k) is True for k in ["issue", "branch", "pr", "ci", "commit_preserved"]):
            raise ValueError("Released fence requires complete native preservation")
        recovered = deepcopy(state)
        recovered["checkpoint"]["recovery"] = evidence
    else:
        previous = state["lease"]
        recovered = control.recover_native(api, state, lease.now())
    value = lease.acquire(recovered, lease.now(), f"comment-{comment_id}", worker, "master",
        19, previous["branch"], previous["commit"], 24)
    value = lease.checkpoint(value, lease.now(), value["lease"]["id"],
        "Interrupted fenced work preserved from native Issue, branch/commit, PR and CI.",
        "Continue owned fenced checkpoints; activate only with installed source/gates unchanged.")
    value["checkpoint"]["control_migration"] = deepcopy(state["checkpoint"]["control_migration"])
    value["checkpoint"]["control_migration"].pop("released_lease", None)
    source_ready(api, value["lease"])
    if api.ref(LEGACY) != expected or optional_ref(api, TARGET) is not None:
        raise ValueError("Interrupted fence changed before recovery write")
    return save_fence(api, expected, value, "Recover preserved fenced migration")

def transport_proof(api, proof_sha, current):
    """A real exact-parent canonical checkpoint, not an editable claim of success."""
    state = api.file(api.cfg["state_path"], proof_sha)
    active(api, proof_sha, state, None)
    command = state["checkpoint"].get("control_migration", {}).get("transport_comment_id")
    if type(command) is not int or command < 1:
        raise ValueError("Notes transport checkpoint is missing")
    native = api.rest("GET", f"issues/comments/{command}")
    import commands
    trusted_comment(api, native, api.cfg["control_commands"]["issue"])
    envelope = commands.payload(native["body"])
    if envelope["command"] != "checkpoint":
        raise ValueError("Transport evidence is not a canonical checkpoint command")
    commit = api.rest("GET", "git/commits/" + proof_sha)
    if commit.get("sha") != proof_sha or [p.get("sha") for p in commit.get("parents", [])] != [envelope["expected_sha"]]:
        raise ValueError("Notes checkpoint does not bind the actual command parent")
    origin = api.file(api.cfg["state_path"], envelope["expected_sha"])
    held = lease.owned(origin, envelope["args"]["lease"], lease.time(native["created_at"]))
    if held["worker"] != envelope["args"]["worker"]:
        raise ValueError("Transport checkpoint is not owned")
    if api.rest("GET", f"compare/{proof_sha}...{current}").get("status") not in ("ahead", "identical"):
        raise ValueError("Transport proof is not part of current notes history")


def trusted_comment(api, value, issue):
    actors = api.cfg["control_commands"]["trusted_actors"]
    user = value.get("user", {})
    if user.get("type") != "User" or user.get("login") not in actors or type(user.get("id")) is not int or actors[user["login"]] != user["id"]:
        raise ValueError("Untrusted cutover evidence author")
    if value.get("issue_url") != f"https://api.github.com/repos/{api.cfg['repository']}/issues/{issue}" or value.get("updated_at") != value.get("created_at"):
        raise ValueError("Cutover evidence was edited or belongs elsewhere")


def task_proof(api, comment_id, policy):
    """Trusted worker attestation to an actual Task response, not a Task API."""
    import json
    native = api.rest("GET", f"issues/comments/{comment_id}")
    trusted_comment(api, native, 19)
    body = native.get("body")
    marker = TASK_MARKER + "\n" + chr(96) * 3 + "json\n"
    if not isinstance(body, str) or len(body) > 3000 or not body.startswith(marker) or not body.endswith("\n" + chr(96) * 3):
        raise ValueError("Task reconciliation attestation is missing")
    from commands import pairs, Rejected
    value = json.loads(body[len(marker):-4], object_pairs_hook=pairs,
        parse_constant=lambda _: (_ for _ in ()).throw(Rejected("invalid-task-json-constant")))
    if not isinstance(value, dict) or set(value) != {"schema", "task_id", "policy_commit", "prompt_sha", "enabled", "cadence", "response_digest"}:
        raise ValueError("Invalid task reconciliation attestation")
    prompt = api.rest("GET", f"contents/engineering/worker-prompt.md?ref={policy}")
    if type(value["schema"]) is not int or value["schema"] != 1 or value["policy_commit"] != policy or value["prompt_sha"] != prompt.get("sha") or value["enabled"] is not True or value["cadence"] != "hourly":
        raise ValueError("Task attestation does not bind the installed hourly admission policy")
    if not isinstance(value["task_id"], str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,160}", value["task_id"]) or not isinstance(value["response_digest"], str) or not re.fullmatch(r"[0-9a-f]{64}", value["response_digest"]):
        raise ValueError("Task attestation lacks native task identity/response digest")
    return value


def retire(api, expected, worker, identity, proof_sha, task_comment):
    location, state, held = owned(api, expected, worker, identity, "quality")
    if location != TARGET:
        raise ValueError("Legacy branch is still active; retirement forbidden")
    policy = source_ready(api, held)
    transport_proof(api, proof_sha, expected)
    task = task_proof(api, task_comment, policy)
    fence_head = state["checkpoint"]["control_migration"]["fence_sha"]
    legacy_head = optional_ref(api, LEGACY)
    if legacy_head is None:
        plan = state["checkpoint"]["control_migration"]
        if plan.get("phase") == "retired":
            raise ValueError("Retirement is already complete; no replay")
        if plan.get("retirement") != "prepared" or plan.get("task_receipt") != task_comment or plan.get("notes_proof_sha") != proof_sha:
            raise ValueError("Legacy absence lacks preserved retirement intent")
    elif legacy_head != fence_head:
        raise ValueError("Legacy fence changed; never blindly delete")
    if any(p["head"]["ref"] == "control/engineering" for p in api.pages("pulls?state=open")):
        raise ValueError("An open PR references the legacy branch")
    other_issues = api.pages("issues?state=open")
    if any(i.get("number") != 19 and "pull_request" not in i and
            "control/engineering" in ((i.get("title") or "") + "\n" + (i.get("body") or ""))
            for i in other_issues):
        raise ValueError("Another open Issue references legacy control; preserve ambiguity")
    # Record retirement intent in notes before an unconditioned REST deletion.
    value = lease.checkpoint(state, lease.now(), identity,
        "Verified notes ancestry/transport and actual-task attestation; legacy retirement intent recorded.",
        "Confirm legacy ref absence, finalize migration acceptance and close Issue19.")
    value["checkpoint"]["control_migration"].update(retirement="prepared", task_receipt=task_comment,
        task_id=task["task_id"], notes_proof_sha=proof_sha)
    head = api.cas_file(TARGET, expected, api.cfg["state_path"], value, "Prepare verified legacy retirement")
    owned(api, head, worker, identity, "quality")
    if api.ref(api.cfg["default_branch"]) != policy:
        raise ValueError("Installed source changed before retirement")
    if legacy_head is not None and api.ref(LEGACY) != fence_head:
        raise ValueError("Legacy branch changed before deletion")
    # GitHub REST has no conditional delete. Fresh exact fences are the strongest
    # supported guard; out-of-band administrators remain outside this lease.
    if legacy_head is not None:
        api.rest("DELETE", "git/refs/heads/control/engineering")
    if optional_ref(api, LEGACY) is not None:
        raise ValueError("Legacy deletion was not confirmed")
    location, actual, current = resolve(api)
    if location != TARGET or actual != head:
        raise ValueError("Notes ownership changed after retirement; preserve intent and stop")
    value = lease.checkpoint(current, lease.now(), identity,
        "Legacy control branch retired; notes remains sole durable authority with all historical proof.",
        "Finish native Issue19 acceptance/cleanup and resume managed work.")
    value["checkpoint"]["control_migration"].update(phase="retired", retirement="confirmed")
    result = api.cas_file(TARGET, head, api.cfg["state_path"], value, "Confirm legacy control retirement")
    if resolve(api)[1] != result:
        raise ValueError("Retirement checkpoint ownership changed")
    return {"control_sha": result, "control_ref": TARGET, "phase": "retired", "lease_id": identity}
