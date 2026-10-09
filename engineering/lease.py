"""Pure serialized lease transitions; only canonical Git control holds live state."""
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import re

TTL = timedelta(minutes=45)
SHA = re.compile(r"[0-9a-f]{40}")


def now():
    return datetime.now(timezone.utc)


def stamp(value):
    return value.astimezone(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def time(value):
    if not isinstance(value, str) or not re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", value):
        raise ValueError("A UTC timestamp is required")
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def branch(value):
    if not isinstance(value, str) or not value or len(value) > 160 or any(
        x in value for x in ["..", "@{", "//", "\\", " ", "~", "^", ":", "?", "*", "[", "\n", "\r"]
    ) or value.startswith(("/", ".")) or value.endswith(("/", ".", ".lock")):
        raise ValueError("Invalid execution branch")
    return value


LEASE_KEYS = frozenset(["id", "worker", "role", "issue", "acquired_at", "heartbeat_at", "expires_at", "branch", "commit", "pr"])
BASE_KEYS = frozenset(["schema", "max_active_agents", "generation", "lease", "checkpoint", "attempts"])
TEAM_KEYS = BASE_KEYS | {"workers"}
WORKER_KEYS = LEASE_KEYS | {"areas", "exclusive", "checkpoint"}
CHECKPOINT_KEYS = frozenset(["at", "issue", "role", "branch", "commit", "pr", "summary", "next_action"])
MAX_TEAM = 8
MAX_WORKERS = 16
AREA = re.compile(r"[a-z][a-z0-9-]{0,39}")


def check_lease(held, keys):
    if not isinstance(held, dict) or set(held) != keys:
        raise ValueError("Invalid lease fields")
    for key in ["id", "worker", "role"]:
        if not isinstance(held[key], str) or not 1 <= len(held[key]) <= 160:
            raise ValueError("Invalid lease identity or role")
    if type(held["issue"]) is not int or held["issue"] < 1 or (held["pr"] is not None and (type(held["pr"]) is not int or held["pr"] < 1)):
        raise ValueError("Invalid lease Issue or PR")
    branch(held["branch"])
    if not isinstance(held["commit"], str) or not SHA.fullmatch(held["commit"]):
        raise ValueError("Lease requires an exact durable commit")
    acquired, heartbeat, expires = (time(held[k]) for k in ["acquired_at", "heartbeat_at", "expires_at"])
    if acquired > heartbeat or not timedelta(0) < expires - heartbeat <= TTL:
        raise ValueError("Invalid lease lifetime")


def check_worker(key, held):
    if not isinstance(held, dict) or set(held) != WORKER_KEYS:
        raise ValueError("Invalid worker lease fields")
    check_lease({k: held[k] for k in LEASE_KEYS}, LEASE_KEYS)
    areas = held["areas"]
    if held["id"] != key or type(held["exclusive"]) is not bool or not isinstance(areas, list) or len(areas) > 8 \
            or areas != sorted(set(areas)) or any(not isinstance(a, str) or not AREA.fullmatch(a) for a in areas):
        raise ValueError("Invalid worker identity, exclusivity or areas")
    if held["exclusive"] != (not areas or "control" in areas):
        raise ValueError("Worker exclusivity must follow its area scope")
    note = held["checkpoint"]
    if note is not None:
        if not isinstance(note, dict) or set(note) != CHECKPOINT_KEYS or note["issue"] != held["issue"] \
                or not all(isinstance(note[k], str) and 1 <= len(note[k]) <= 2000 for k in ["summary", "next_action"]):
            raise ValueError("Invalid worker checkpoint")
        time(note["at"])
        branch(note["branch"])
        if not SHA.fullmatch(note["commit"]) or not isinstance(note["role"], str) or not 1 <= len(note["role"]) <= 160:
            raise ValueError("Invalid worker checkpoint source")


def validate(state):
    if not isinstance(state, dict):
        raise ValueError("Unexpected control state fields; preserve rather than overwrite")
    team = type(state.get("schema")) is int and state["schema"] == 3
    if set(state) != (TEAM_KEYS if team else BASE_KEYS):
        raise ValueError("Unexpected control state fields; preserve rather than overwrite")
    if team:
        # Schema 3 is opt-in and unreadable to schema-1 tooling, which fails closed.
        if type(state["max_active_agents"]) is not int or not 2 <= state["max_active_agents"] <= MAX_TEAM \
                or type(state["generation"]) is not int or not 0 <= state["generation"] < 1_000_000_000:
            raise ValueError("Invalid state schema, concurrency or generation")
    elif state["schema"] != 1 or state["max_active_agents"] != 1 or type(state["generation"]) is not int or not 0 <= state["generation"] < 1_000_000_000:
        raise ValueError("Invalid state schema, concurrency or generation")
    if not isinstance(state["checkpoint"], dict) or not isinstance(state["attempts"], dict) or len(state["attempts"]) > 256:
        raise ValueError("Invalid or oversized checkpoint/attempt data")
    held = state["lease"]
    if held is not None:
        check_lease(held, LEASE_KEYS)
    if team:
        workers = state["workers"]
        if not isinstance(workers, dict) or len(workers) > MAX_WORKERS:
            raise ValueError("Invalid or oversized worker table")
        for key, value in workers.items():
            check_worker(key, value)
        if len({w["issue"] for w in workers.values()}) != len(workers):
            raise ValueError("One lease per Issue")
        if held is not None and any(w["id"] == held["id"] or w["issue"] == held["issue"] for w in workers.values()):
            raise ValueError("Serial and worker leases must have distinct identities and Issues")
        handoffs = state["checkpoint"].get("worker_handoffs", {})
        if not isinstance(handoffs, dict) or len(handoffs) > 256:
            raise ValueError("Invalid or oversized worker handoffs")
    return state


def valid(state, at):
    validate(state)
    return state["lease"] is not None and time(state["lease"]["expires_at"]) > at


def owned(state, identity, at):
    if not valid(state, at) or state["lease"]["id"] != identity:
        raise ValueError("Lease is missing, expired or belongs to another worker; stop work")
    return state["lease"]


def acquire(state, at, identity, worker, role, issue, execution_branch, commit, pr=None):
    validate(state)
    if state["lease"] is not None:
        raise ValueError("Existing lease requires release or explicit stale-work recovery")
    if state["schema"] == 3:
        if role not in {"quality", "triage"}:
            raise ValueError("The singleton team lease is reserved for mechanical delivery")
        if expired(state, at):
            raise ValueError("Recover expired worker records before delivery")
        if any(w["exclusive"] or w["issue"] == issue for w in live(state, at)):
            raise ValueError("Exclusive work or an Issue worker blocks delivery")
    result = deepcopy(state)
    result["generation"] += 1
    result["lease"] = {"id": identity, "worker": worker, "role": role, "issue": issue,
        "acquired_at": stamp(at), "heartbeat_at": stamp(at), "expires_at": stamp(at + TTL),
        "branch": execution_branch, "commit": commit, "pr": pr}
    return validate(result)


def checkpoint(state, at, identity, summary, next_action, commit=None, pr=None):
    held = owned(state, identity, at)
    if not isinstance(summary, str) or not 1 <= len(summary) <= 2000 or not isinstance(next_action, str) or not 1 <= len(next_action) <= 2000:
        raise ValueError("A concise checkpoint and next action are required")
    result = deepcopy(state)
    lease = result["lease"]
    if commit is not None:
        lease["commit"] = commit
    if pr is not None:
        lease["pr"] = pr
    lease["heartbeat_at"] = stamp(at)
    lease["expires_at"] = stamp(at + TTL)
    result["generation"] += 1
    previous = result["checkpoint"]
    # Retain source-bound evidence and one concise prior-work handoff. Replacing
    # the whole record during mechanical recovery loses interrupted work; nesting
    # full checkpoints would instead grow without bound on repeated wakes.
    keys = ["issue", "branch", "pr"]
    if previous and any(previous.get(key) != lease[key] for key in keys):
        previous["previous_checkpoint"] = {key: previous[key] for key in
            ["at", "issue", "role", "branch", "commit", "pr", "summary", "next_action"] if key in previous}
    previous.update(at=stamp(at), issue=held["issue"], role=held["role"],
        branch=lease["branch"], commit=lease["commit"], pr=lease["pr"],
        summary=summary, next_action=next_action)
    return validate(result)


def release(state, at, identity):
    owned(state, identity, at)
    result = deepcopy(state)
    result["generation"] += 1
    result["lease"] = None
    return validate(result)


PHASE_KEYS = frozenset(["at", "issue", "role", "branch", "commit", "pr", "summary", "next_action",
    "previous_checkpoint", "recovery", "circuit_breaker"])


def durable(checkpoint):
    """Source-bound records (migration, publication, ...) that outlive one phase."""
    return {key: value for key, value in checkpoint.items() if key not in PHASE_KEYS}


def interruption(state, held, at, next_action):
    """Bind an interrupted phase even if it died before its first checkpoint."""
    old = state["checkpoint"]
    keys = ["issue", "role", "branch", "commit", "pr"]
    same = all(old.get(key) == held[key] for key in keys) and old.get("at") is not None and time(old["at"]) >= time(held["acquired_at"])
    # A new phase record keeps durable records, as checkpoint() does; dropping
    # them would remove the canonical migration proof and fail closed forever.
    result = dict(old) if same else {**durable(old), **{key: held[key] for key in keys}}
    if not same:
        result["summary"] = "Interrupted before a phase checkpoint; inspect the preserved lease source, Issue, PR and native history."
        if old:
            result["previous_checkpoint"] = {key: old[key] for key in ["at", *keys, "summary", "next_action"] if key in old}
    result.update(at=stamp(at), next_action=next_action)
    return result


def recover(state, at, evidence):
    validate(state)
    if state["lease"] is None or valid(state, at):
        raise ValueError("Only an expired lease can be recovered")
    if not isinstance(evidence, dict) or not all(evidence.get(k) is True for k in ["issue", "branch", "pr", "ci", "commit_preserved"]):
        raise ValueError("Stale recovery requires native Issue, branch, PR, CI and preservation evidence")
    result = deepcopy(state)
    result["generation"] += 1
    result["checkpoint"] = interruption(state, state["lease"], at,
        "Recovered expired capacity lease; inspect preserved work before selecting new work.")
    result["checkpoint"]["recovery"] = evidence
    result["lease"] = None
    return validate(result)


def attempt(state, at, identity, approach, fingerprint):
    held = owned(state, identity, at)
    if not all(isinstance(s, str) and 1 <= len(s) <= 256 for s in [approach, fingerprint]):
        raise ValueError("Attempt requires bounded approach and observed progress fingerprint")
    result = deepcopy(state)
    key = str(held["issue"])
    old = result["attempts"].get(key, {})
    repeats = old.get("unchanged", 0) + 1 if old.get("approach") == approach and old.get("fingerprint") == fingerprint else 1
    result["attempts"][key] = {"approach": approach, "fingerprint": fingerprint, "unchanged": min(repeats, 3), "at": stamp(at)}
    result["generation"] += 1
    if repeats >= 3:
        result["lease"] = None
        result["checkpoint"] = interruption(state, held, at,
            "Circuit breaker: return to Triage, inspect evidence and choose a changed approach. Do not retry unchanged work.")
        result["checkpoint"]["circuit_breaker"] = result["attempts"][key]
    return validate(result)


# ---- Opt-in team mode (schema 3): per-Issue worker leases beside the singleton lease.

def is_team(state):
    return validate(state)["schema"] == 3


def scope(labels):
    """Areas come from server-read Issue labels. No area label or area:control is exclusive."""
    areas = sorted({name[5:] for name in labels if name.startswith("area:")})
    if any(not AREA.fullmatch(a) for a in areas) or len(areas) > 8:
        raise ValueError("Invalid or excessive area labels")
    return areas, not areas or "control" in areas


def live(state, at):
    return [w for w in state.get("workers", {}).values() if time(w["expires_at"]) > at]


def expired(state, at):
    return [w for w in state.get("workers", {}).values() if time(w["expires_at"]) <= at]


def leases(state):
    """Every lease record, serial and worker, for historical review binding."""
    return ([] if state["lease"] is None else [state["lease"]]) + list(state.get("workers", {}).values())


def admissible(state, at, areas, exclusive, issue):
    """Return None when a worker may start, else the refusal reason."""
    if any(w["issue"] == issue for w in state["workers"].values()):
        return "Issue already has a worker lease; release or recover it first"
    if expired(state, at) or state["lease"] is not None and not valid(state, at):
        return "Recover expired execution records before unrelated admission"
    serial = state["lease"]
    if serial is not None and (exclusive or serial["issue"] == issue or serial["role"] not in {"quality", "triage"}):
        return "Singleton delivery or activation must release before this work"
    running = live(state, at)
    if len(running) >= state["max_active_agents"]:
        return "No capacity: the worker cap is reached"
    if exclusive and running:
        return "Exclusive work needs every other worker to release"
    if any(w["exclusive"] for w in running):
        return "An exclusive worker holds the team"
    taken = {a for w in running for a in w["areas"]}
    if taken & set(areas):
        return "Overlapping area is already leased"
    return None


def enable_team(state, at, identity, issue, cap):
    """Reviewed schema-1 to schema-3 transition; keeps the serial lease and records proof."""
    held = owned(state, identity, at)
    if state["schema"] != 1 or held["role"] != "master" or held["issue"] != issue or not 2 <= cap <= MAX_TEAM:
        raise ValueError("Team mode needs a serial Master lease on the reviewed Issue and a bounded cap")
    note = state["checkpoint"]
    if any(note.get(k) != held[k] for k in ["issue", "role", "branch", "commit", "pr"]) or note.get("at") is None or time(note["at"]) < time(held["acquired_at"]):
        raise ValueError("Checkpoint the reviewed transition before enabling team mode")
    result = deepcopy(state)
    result.update(schema=3, max_active_agents=cap, workers={})
    result["generation"] += 1
    result["checkpoint"]["team"] = {"at": stamp(at), "issue": issue, "cap": cap}
    return validate(result)


def acquire_worker(state, at, identity, worker, role, issue, execution_branch, commit, pr, labels):
    validate(state)
    if state["schema"] != 3 or identity in state["workers"] or (state["lease"] or {}).get("id") == identity:
        raise ValueError("Team mode is not enabled or the lease identity is not unique")
    areas, exclusive = scope(labels)
    refusal = admissible(state, at, areas, exclusive, issue)
    if refusal:
        raise ValueError(refusal)
    result = deepcopy(state)
    result["generation"] += 1
    result["workers"][identity] = {"id": identity, "worker": worker, "role": role, "issue": issue,
        "acquired_at": stamp(at), "heartbeat_at": stamp(at), "expires_at": stamp(at + TTL),
        "branch": execution_branch, "commit": commit, "pr": pr, "areas": areas, "exclusive": exclusive, "checkpoint": None}
    return validate(result)


def holder(state, identity, at):
    """The serial or worker lease this identity owns while valid."""
    validate(state)
    if identity in state.get("workers", {}):
        return owned_worker(state, identity, at)
    return owned(state, identity, at)


def owned_worker(state, identity, at):
    validate(state)
    held = state.get("workers", {}).get(identity)
    if held is None or time(held["expires_at"]) <= at:
        raise ValueError("Worker lease is missing, expired or belongs to another worker; stop work")
    return held


def checkpoint_worker(state, at, identity, summary, next_action, commit=None, pr=None):
    held = owned_worker(state, identity, at)
    if not isinstance(summary, str) or not 1 <= len(summary) <= 2000 or not isinstance(next_action, str) or not 1 <= len(next_action) <= 2000:
        raise ValueError("A concise checkpoint and next action are required")
    result = deepcopy(state)
    mine = result["workers"][identity]
    if commit is not None:
        mine["commit"] = commit
    if pr is not None:
        mine["pr"] = pr
    mine["heartbeat_at"] = stamp(at)
    mine["expires_at"] = stamp(at + TTL)
    mine["checkpoint"] = {"at": stamp(at), "issue": held["issue"], "role": held["role"], "branch": mine["branch"],
        "commit": mine["commit"], "pr": mine["pr"], "summary": summary, "next_action": next_action}
    result["generation"] += 1
    return validate(result)


def preserve_worker(result, held, at, outcome, evidence=None):
    """Keep one bounded latest handoff per Issue before removing an execution record."""
    handoffs = result["checkpoint"].setdefault("worker_handoffs", {})
    key = str(held["issue"])
    if key not in handoffs and len(handoffs) >= 256:
        raise ValueError("Worker handoff capacity needs deliberate native-history maintenance")
    note = held["checkpoint"]
    if note is None:
        note = {"at": stamp(at), **{k: held[k] for k in ["issue", "role", "branch", "commit", "pr"]},
            "summary": "Interrupted before a phase checkpoint; original source and native work are preserved.",
            "next_action": "Inspect native Issue, branch/commits, PR and CI before resuming."}
    handoffs[key] = {**deepcopy(note), "lease_id": held["id"], "outcome": outcome, "recorded_at": stamp(at)}
    if evidence is not None:
        handoffs[key]["recovery"] = deepcopy(evidence)


def release_worker(state, at, identity):
    held = owned_worker(state, identity, at)
    note = held["checkpoint"]
    if note is None or any(note[k] != held[k] for k in ["issue", "role", "branch", "commit", "pr"]) or time(note["at"]) < time(held["acquired_at"]):
        raise ValueError("Checkpoint this work before releasing its lease")
    result = deepcopy(state)
    preserve_worker(result, held, at, "released")
    del result["workers"][identity]
    result["generation"] += 1
    return validate(result)


def recover_worker(state, at, identity, evidence):
    validate(state)
    held = state.get("workers", {}).get(identity)
    if held is None or time(held["expires_at"]) > at:
        raise ValueError("Only an expired worker lease can be recovered")
    if not isinstance(evidence, dict) or not all(evidence.get(k) is True for k in ["issue", "branch", "pr", "ci", "commit_preserved"]):
        raise ValueError("Stale recovery requires native Issue, branch, PR, CI and preservation evidence")
    result = deepcopy(state)
    preserve_worker(result, held, at, "recovered", evidence)
    del result["workers"][identity]
    result["generation"] += 1
    return validate(result)


def attempt_worker(state, at, identity, approach, fingerprint):
    held = owned_worker(state, identity, at)
    if not all(isinstance(s, str) and 1 <= len(s) <= 256 for s in [approach, fingerprint]):
        raise ValueError("Attempt requires bounded approach and observed progress fingerprint")
    result = deepcopy(state)
    key = str(held["issue"])
    old = result["attempts"].get(key, {})
    repeats = old.get("unchanged", 0) + 1 if old.get("approach") == approach and old.get("fingerprint") == fingerprint else 1
    result["attempts"][key] = {"approach": approach, "fingerprint": fingerprint, "unchanged": min(repeats, 3), "at": stamp(at)}
    result["generation"] += 1
    if repeats >= 3:
        # The breaker and source-bound handoff survive even before the first checkpoint.
        preserve_worker(result, held, at, "circuit-breaker")
        del result["workers"][identity]
        result["attempts"][key]["circuit_breaker"] = "Return to Triage with a changed approach."
    return validate(result)
