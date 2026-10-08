"""Authenticated comment-to-GitData administration; no AI or proposed code execution."""
from datetime import timedelta
import json
import os
from pathlib import Path
import re
import sys

import control
from github import APIError, GitHub, ROOT
import lease

MARKER = "<!-- mynou-control-command:v1 -->"
RECEIPT = "<!-- mynou-control-receipt:v1 -->"
FENCE = chr(96) * 3
PROBE_REF = "refs/notes/mynou-control-transport-probe"
MAX_BODY = 6000
MAX_EVENT = 512 * 1024
COMMANDS = {
    "acquire": {"worker", "role", "issue", "branch", "commit", "pr"},
    "checkpoint": {"worker", "lease", "summary", "next_action", "commit", "pr"},
    "release": {"worker", "lease"},
    "recover": set(),
    "attempt": {"worker", "lease", "approach", "fingerprint"},
    "probe-notes": {"worker", "lease", "expected_probe_sha"},
}


class Rejected(ValueError):
    """Only fixed diagnostic codes can reach public workflow logs."""


def require(condition, code):
    if not condition:
        raise Rejected(code)


def exact_keys(value, keys):
    require(isinstance(value, dict) and set(value) == keys, "invalid-fields")


def integer(value):
    return type(value) is int and 0 < value < 10**16


def sha(value):
    return isinstance(value, str) and lease.SHA.fullmatch(value) is not None


def identity(value):
    return isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.:-]{0,159}", value) is not None


def pairs(values):
    result = {}
    for key, value in values:
        require(key not in result, "duplicate-json-key")
        result[key] = value
    return result


def payload(body):
    require(isinstance(body, str) and len(body.encode("utf-8")) <= MAX_BODY, "invalid-body-size")
    match = re.fullmatch(re.escape(MARKER) + r"\n" + FENCE + r"json\n(.+)\n" + FENCE, body, re.DOTALL)
    require(match is not None, "invalid-command-envelope")
    try:
        value = json.loads(match[1], object_pairs_hook=pairs,
            parse_constant=lambda _: (_ for _ in ()).throw(Rejected("invalid-json-constant")))
    except (json.JSONDecodeError, RecursionError):
        raise Rejected("invalid-json") from None
    exact_keys(value, {"schema", "command", "expected_sha", "args"})
    require(type(value["schema"]) is int and value["schema"] == 1, "invalid-command-schema")
    command = value["command"]
    require(isinstance(command, str) and command in COMMANDS, "unsupported-command")
    require(sha(value["expected_sha"]), "invalid-expected-sha")
    args = value["args"]
    exact_keys(args, COMMANDS[command])
    if command not in {"acquire", "recover"}:
        require(identity(args["lease"]), "invalid-lease-identity")
    if "worker" in args:
        require(identity(args["worker"]), "invalid-worker-identity")
    if command == "acquire":
        require(identity(args["role"]) and integer(args["issue"]), "invalid-role-or-issue")
        lease.branch(args["branch"])
        require(sha(args["commit"]) and (args["pr"] is None or integer(args["pr"])), "invalid-source-scope")
    if command == "checkpoint":
        require(sha(args["commit"]) and (args["pr"] is None or integer(args["pr"])), "invalid-source-scope")
        require(all(isinstance(args[k], str) and 1 <= len(args[k]) <= 2000 for k in ["summary", "next_action"]), "invalid-checkpoint-text")
    if command == "attempt":
        require(all(isinstance(args[k], str) and 1 <= len(args[k]) <= 256 for k in ["approach", "fingerprint"]), "invalid-attempt-text")
    if command == "probe-notes":
        require(args["expected_probe_sha"] is None or sha(args["expected_probe_sha"]), "invalid-probe-sha")
    return value


def authenticate(api, event, at, actor, triggering_actor):
    cfg = api.cfg
    settings = cfg.get("control_commands")
    exact_keys(settings, {"issue", "issue_id", "repository_id", "trusted_actors", "max_age_seconds"})
    actors = settings["trusted_actors"]
    require(isinstance(actors, dict) and bool(actors) and all(identity(k) and integer(v) for k, v in actors.items()), "invalid-trusted-actors")
    require(set(actors).issubset(cfg["trusted_reviewers"]), "unreviewed-command-identity")
    require(integer(settings["issue"]) and integer(settings["issue_id"]) and integer(settings["repository_id"]), "invalid-command-target")
    require(type(settings["max_age_seconds"]) is int and 30 <= settings["max_age_seconds"] <= 600, "invalid-command-age-policy")
    require(cfg["repository"] == "alecerf/mynou" and event.get("repository", {}).get("full_name") == cfg["repository"]
        and event["repository"].get("id") == settings["repository_id"], "wrong-repository")
    require(event.get("action") == "created" and actor in actors and triggering_actor in actors, "untrusted-event-actor")
    origin = event.get("comment", {})
    user = origin.get("user", {})
    require(user.get("type") == "User" and user.get("login") == actor and type(user.get("id")) is int
        and user["id"] == actors[actor], "untrusted-comment-author")
    sender = event.get("sender", {})
    require(sender.get("login") == actor and sender.get("id") == actors[actor], "untrusted-event-sender")
    issue = event.get("issue", {})
    require(issue.get("number") == settings["issue"] and issue.get("id") == settings["issue_id"]
        and "pull_request" not in issue, "wrong-command-issue")
    comment_id = origin.get("id")
    require(integer(comment_id), "invalid-comment-identity")
    native = api.rest("GET", f"issues/comments/{comment_id}")
    require(native.get("id") == comment_id and native.get("issue_url") ==
        f"https://api.github.com/repos/{cfg['repository']}/issues/{settings['issue']}", "wrong-native-comment")
    require(native.get("user") == user and native.get("body") == origin.get("body")
        and native.get("created_at") == origin.get("created_at")
        and native.get("updated_at") == native.get("created_at")
        and origin.get("updated_at") == origin.get("created_at"), "edited-or-replaced-comment")
    require(native.get("author_association") in {"OWNER", "MEMBER", "COLLABORATOR"}, "untrusted-association")
    age = at - lease.time(native["created_at"])
    require(timedelta(0) <= age <= timedelta(seconds=settings["max_age_seconds"]), "stale-command")
    mailbox = api.rest("GET", f"issues/{settings['issue']}")
    require(mailbox.get("id") == settings["issue_id"] and mailbox.get("number") == settings["issue"]
        and "pull_request" not in mailbox and "agent-work" in control.labels(mailbox), "invalid-command-mailbox")
    return comment_id, payload(native["body"])


def source_scope(api, execution_branch, commit, pr_number, issue_number):
    require(api.ref(execution_branch) == commit, "source-head-changed")
    native = api.rest("GET", "git/commits/" + commit)
    require(native.get("sha") == commit, "source-commit-unavailable")
    if pr_number is not None:
        pr = api.rest("GET", f"pulls/{pr_number}")
        require(pr.get("number") == pr_number and pr["head"]["repo"]["full_name"] == api.cfg["repository"]
            and pr["base"]["ref"] == api.cfg["default_branch"], "wrong-source-pr")
        source = pr["head"]["sha"] == commit and pr["head"]["ref"] == execution_branch
        merged = pr.get("merged_at") and pr.get("merge_commit_sha") == commit and execution_branch == api.cfg["default_branch"]
        require(source or merged, "pr-source-changed")
        require(re.search(rf"\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s+#0*{issue_number}\b",
            pr.get("body") or "", re.IGNORECASE) is not None, "unlinked-source-pr")


def transition(api, state, value, at, comment_id):
    args, command = value["args"], value["command"]
    if command == "recover":
        return control.recover_native(api, state, at)
    if command == "acquire":
        require(state["lease"] is None, "lease-not-free")
        roles = json.loads((ROOT / "engineering/roles.json").read_text())["roles"]
        require(args["role"] in roles, "unregistered-role")
        require(args["role"] not in {"qa", "security"} or args["pr"] is not None, "review-requires-pr")
        issue = api.rest("GET", f"issues/{args['issue']}")
        require(issue.get("number") == args["issue"] and issue.get("state") == "open"
            and "agent-work" in control.labels(issue) and "pull_request" not in issue, "unmanaged-work-issue")
        blockers = api.pages(f"issues/{args['issue']}/dependencies/blocked_by")
        require(args["role"] in {"master", "triage"} or not any(i["state"] == "open" for i in blockers), "open-native-blocker")
        source_scope(api, args["branch"], args["commit"], args["pr"], args["issue"])
        return lease.acquire(state, at, f"comment-{comment_id}", args["worker"], args["role"],
            args["issue"], args["branch"], args["commit"], args["pr"])
    held = lease.owned(state, args["lease"], at)
    require(held["worker"] == args["worker"], "wrong-lease-worker")
    if command == "checkpoint":
        source_scope(api, held["branch"], args["commit"], args["pr"] or held["pr"], held["issue"])
        return lease.checkpoint(state, at, args["lease"], args["summary"], args["next_action"], args["commit"], args["pr"])
    if command == "release":
        cp = state["checkpoint"]
        require(all(cp.get(k) == held[k] for k in ["issue", "role", "branch", "commit", "pr"])
            and lease.time(cp.get("at")) >= lease.time(held["acquired_at"]), "missing-current-checkpoint")
        return lease.release(state, at, args["lease"])
    if command == "attempt":
        return lease.attempt(state, at, args["lease"], args["approach"], args["fingerprint"])
    require(held["role"] in {"master", "quality", "security"}, "probe-role-forbidden")
    return None


def probe_notes(api, value, at, comment_id):
    """Write one inert marker; it cannot contain or grant an engineering lease."""
    expected = value["args"]["expected_probe_sha"]
    path = "git/ref/" + PROBE_REF.removeprefix("refs/")
    native = None
    try:
        native = api.rest("GET", path)
    except APIError as error:
        if error.status != 404:
            raise
    if native is None:
        require(expected is None, "probe-ref-missing")
        parent = value["expected_sha"]
    else:
        require(native.get("ref") == PROBE_REF and native.get("object", {}).get("type") == "commit"
            and sha(native["object"].get("sha")) and native["object"]["sha"] == expected, "probe-ref-changed")
        parent = expected
    content = json.dumps({"schema": 1, "kind": "inert-control-transport-probe",
        "control_sha": value["expected_sha"], "comment_id": comment_id, "at": lease.stamp(at)}, indent=2) + "\n"
    tree = api.rest("POST", "git/trees", {"tree": [{"path": "probe.json", "mode": "100644", "type": "blob", "content": content}]})
    commit = api.rest("POST", "git/commits", {"message": "Inert control transport proof", "tree": tree["sha"], "parents": [parent]})
    require(sha(commit.get("sha")), "invalid-probe-commit")
    head, state = control.read_state(api)
    require(head == value["expected_sha"], "control-changed-before-probe-write")
    held = lease.owned(state, value["args"]["lease"], lease.now())
    require(held["worker"] == value["args"]["worker"], "probe-ownership-lost")
    if native is None:
        result = api.rest("POST", "git/refs", {"ref": PROBE_REF, "sha": commit["sha"]})
    else:
        result = api.rest("PATCH", "git/refs/" + PROBE_REF.removeprefix("refs/"), {"sha": commit["sha"], "force": False})
    require(result.get("ref") == PROBE_REF and result.get("object", {}).get("type") == "commit"
        and result["object"].get("sha") == commit["sha"], "probe-response-disagrees")
    head, state = control.read_state(api)
    require(head == value["expected_sha"] and lease.owned(state, held["id"], lease.now())["worker"] == held["worker"],
        "control-changed-after-probe-write")
    return commit["sha"]


def execute(api, event, at, actor, triggering_actor):
    comment_id, value = authenticate(api, event, at, actor, triggering_actor)
    # Admission before any domain work: exact native head, never an Issue pointer.
    head, state = control.read_state(api)
    require(head == value["expected_sha"], "stale-expected-head")
    next_state = transition(api, state, value, at, comment_id)
    if value["command"] == "probe-notes":
        proof = probe_notes(api, value, at, comment_id)
        result = {"schema": 1, "command_id": comment_id, "command": "probe-notes",
            "control_sha": head, "probe_ref": PROBE_REF, "probe_sha": proof}
    else:
        # Fresh ownership/lifetime fence immediately before commit construction.
        fresh_head, fresh = control.read_state(api)
        require(fresh_head == head, "ownership-head-changed")
        if value["command"] not in {"acquire", "recover"}:
            lease.owned(fresh, value["args"]["lease"], lease.now())
        sha_result = control.save(api, head, next_state, f"Control comment {comment_id}: {value['command']}")
        actual_head, actual_state = control.read_state(api)
        require(actual_head == sha_result and actual_state == next_state, "control-write-unconfirmed")
        result = {"schema": 1, "command_id": comment_id, "command": value["command"],
            "control_sha": sha_result, "lease_id": None if next_state["lease"] is None else next_state["lease"]["id"]}
    # Receipt loss never rolls back committed progress. Read Git before retrying.
    api.rest("POST", f"issues/{api.cfg['control_commands']['issue']}/comments",
        {"body": RECEIPT + "\n" + FENCE + "json\n" + json.dumps(result, indent=2) + "\n" + FENCE})
    return result


def main():
    require(os.environ.get("GITHUB_EVENT_NAME") == "issue_comment", "wrong-event-kind")
    path = Path(os.environ["GITHUB_EVENT_PATH"])
    require(path.stat().st_size <= MAX_EVENT, "event-too-large")
    event = json.loads(path.read_text())
    result = execute(GitHub(), event, lease.now(), os.environ["GITHUB_ACTOR"], os.environ["GITHUB_TRIGGERING_ACTOR"])
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except Rejected as error:
        print("Control command rejected: " + str(error), file=sys.stderr)
        sys.exit(1)
    except (APIError, ValueError, RuntimeError, KeyError, TypeError, OSError, RecursionError):
        print("Control command stopped; inspect canonical Git state and native run before another attempt.", file=sys.stderr)
        sys.exit(1)
