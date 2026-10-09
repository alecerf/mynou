"""Read-only board: free work, claims, whose turn each PR is and release cadence.

Run `python3 engineering/board.py --agent <name>` at the start of a session.
It never writes; agents post their own comment commands."""
import argparse
from datetime import datetime, timezone
import json
import sys

from github import APIError, GitHub
import protocol
import releases


def pull_request(api, pull, issues, actors, ttl, now):
    number, head = pull["number"], pull["head"]["sha"]
    comments = api.pages(f"issues/{number}/comments")
    paths = [item["filename"] for item in api.pages(f"pulls/{number}/files")]
    linked = protocol.linked_issues(pull.get("body"))
    issue_labels = set()
    for issue in issues:
        if issue["number"] in linked:
            issue_labels |= protocol.labels(issue)
    security = protocol.security_required(paths, issue_labels)
    owner = protocol.claim(comments, actors)
    if owner is not None:
        owner["lapsed"] = protocol.lapsed([pull["updated_at"], owner["since"]], now, ttl)
    ci = protocol.ci_state(api.pages(f"commits/{head}/check-runs", "check_runs"))
    return {"pr": number, "title": pull["title"], "issues": linked, "draft": pull["draft"], "head": head,
            "waiting_for": protocol.turn(comments, actors, head, pull["draft"], security, ci),
            "verdicts": protocol.verdicts(comments, actors, head), "security_required": security,
            "ci": ci, "owner": owner}


def board(api, now, agent=None):
    cfg = api.cfg
    actors, ttl = cfg["trusted_actors"], cfg["claim_ttl_minutes"]
    issues = [i for i in api.pages("issues?state=open&labels=agent-work") if "pull_request" not in i]
    pulls = {}
    for pull in sorted(api.pages("pulls?state=open"), key=lambda p: p["number"]):
        pulls[pull["number"]] = (pull, pull_request(api, pull, issues, actors, ttl, now))
    by_issue = {}
    for pull, entry in pulls.values():
        for number in entry["issues"]:
            by_issue.setdefault(number, (pull, entry))
    work, claims = [], []
    for issue in sorted(issues, key=protocol.rank):
        if protocol.blocked(issue):
            continue
        number = issue["number"]
        pull, entry = by_issue.get(number, (None, None))
        item = {"issue": number, "title": issue["title"],
                "labels": sorted(protocol.labels(issue)), "pr": pull and pull["number"]}
        owner = protocol.claim(api.pages(f"issues/{number}/comments"), actors)
        if owner is not None:
            activity = [issue["updated_at"], owner["since"]] + ([pull["updated_at"]] if pull else [])
            owner["lapsed"] = protocol.lapsed(activity, now, ttl)
            claims.append(dict(item, owner=owner))
            if not owner["lapsed"]:
                continue
        # A PR waiting for review, CI, merge or the owner is not implementation work.
        if entry is None or entry["waiting_for"] == "author":
            work.append(item)
    open_releases = [i["number"] for i in issues if "release" in protocol.labels(i)]
    planning = cfg["product_planning"]
    result = {"at": protocol.stamp(now), "agent": agent, "work": work, "claims": claims,
              "pull_requests": [entry for _, entry in pulls.values()],
              "release": releases.status(api, now, open_releases),
              "product_planning": {"issue": planning["issue"],
                                   "ready_proposals": protocol.ready_proposals(issues, planning["issue"]),
                                   "due": protocol.planning_due(issues, planning, now)}}
    if agent is not None:
        result["mine"] = {
            "issues": [c["issue"] for c in claims if c["owner"]["agent"] == agent],
            "pull_requests": [e["pr"] for _, e in pulls.values()
                              if e["owner"] is not None and e["owner"]["agent"] == agent]}
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agent", help="your agent name, to list the Issues and PRs you own")
    args = parser.parse_args()
    if args.agent is not None and protocol.parse("/assign " + args.agent) is None:
        raise ValueError("Agent names use 2-40 lowercase letters, digits or hyphens")
    print(json.dumps(board(GitHub(), datetime.now(timezone.utc), args.agent), indent=2))


if __name__ == "__main__":
    try:
        main()
    except (APIError, ValueError, RuntimeError, KeyError, TypeError) as error:
        print("Board stopped: " + str(error), file=sys.stderr)
        sys.exit(1)
