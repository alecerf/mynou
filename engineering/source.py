"""Preserve meaningful local commits through GitHub Git Data when Git HTTPS fails.

This transports source, not build/release artifacts. No project validation runs.
"""
import argparse
import json
import subprocess

from github import APIError, GitHub
from control import read_state
import lease


def git(*args):
    return subprocess.check_output(["git", *args], text=True)


def publish(api, local_base, remote_base, execution_branch, identity):
    head, state = read_state(api)
    held = lease.owned(state, identity, lease.now())
    if held["branch"] != execution_branch:
        raise ValueError("Source branch disagrees with the held lease")
    local_base = git("rev-parse", local_base + "^{commit}").strip()
    commit = api.rest("GET", "git/commits/" + remote_base)
    if git("rev-parse", local_base + "^{tree}").strip() != commit["tree"]["sha"]:
        raise ValueError("Local and remote source baselines do not have identical trees")
    try:
        observed = api.ref(execution_branch)
    except APIError as error:
        if error.status != 404:
            raise
        observed = None
    if observed is not None and observed != remote_base:
        raise ValueError("Remote source advanced; reconstruct before transporting commits")
    commits = git("rev-list", "--reverse", local_base + "..HEAD").splitlines()
    if not 1 <= len(commits) <= 20:
        raise ValueError("Transport 1–20 meaningful commits per bounded source slice")
    parent = remote_base
    tree_sha = commit["tree"]["sha"]
    result = []
    for local in commits:
        current_head, current = read_state(api)
        lease.owned(current, identity, lease.now())
        if current_head != head:
            raise ValueError("Lease checkpoint changed during source transport; resume with fresh state")
        entries = []
        changed = git("diff-tree", "--no-commit-id", "--name-status", "-r", "--no-renames", local).splitlines()
        if len(changed) > 256:
            raise ValueError("Source commit exceeds 256 changed files")
        for change in changed:
            status, path = change.split("\t", 1)
            if status == "D":
                entries.append({"path": path, "mode": "100644", "type": "blob", "sha": None})
            else:
                row = git("ls-tree", local, "--", path).split("\t", 1)[0].split()
                if row[1] != "blob" or row[0] not in ("100644", "100755"):
                    raise ValueError("Only original regular source files can be transported")
                content = git("show", local + ":" + path)
                if len(content.encode()) > 2 * 1024 * 1024:
                    raise ValueError("Source file exceeds 2 MiB")
                entries.append({"path": path, "mode": row[0], "type": "blob", "content": content})
        tree = api.rest("POST", "git/trees", {"base_tree": tree_sha, "tree": entries})
        if tree["sha"] != git("rev-parse", local + "^{tree}").strip():
            raise ValueError("Source transport tree differs; refuse ref mutation")
        remote = api.rest("POST", "git/commits", {"message": git("show", "-s", "--format=%B", local), "tree": tree["sha"], "parents": [parent]})
        result.append({"local": local, "remote": remote["sha"], "tree": tree["sha"]})
        parent, tree_sha = remote["sha"], tree["sha"]
    lease.owned(read_state(api)[1], identity, lease.now())
    if observed is None:
        api.rest("POST", "git/refs", {"ref": "refs/heads/" + execution_branch, "sha": parent})
    else:
        if api.ref(execution_branch) != observed:
            raise ValueError("Remote source changed before update; stop without force")
        api.rest("PATCH", "git/refs/heads/" + execution_branch, {"sha": parent, "force": False})
    return {"branch": execution_branch, "source": parent, "commits": result}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--local-base", required=True)
    parser.add_argument("--remote-base", required=True)
    parser.add_argument("--branch", required=True)
    parser.add_argument("--lease", required=True)
    args = parser.parse_args()
    print(json.dumps(publish(GitHub(), args.local_base, args.remote_base, args.branch, args.lease), indent=2))
