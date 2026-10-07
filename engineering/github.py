"""Small bounded GitHub REST/GraphQL administration client; Python std only."""
import base64
import json
import os
from pathlib import Path
import re
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
MAX_RESPONSE = 4 * 1024 * 1024


class APIError(RuntimeError):
    def __init__(self, status):
        self.status = status
        super().__init__(f"GitHub request failed with HTTP {status}; refresh state before retrying")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise APIError(code)


def config():
    value = json.loads((ROOT / ".github/engineering.json").read_text())
    if value.get("schema") != 1 or value.get("max_active_agents") != 1:
        raise ValueError("Unsupported organization schema or concurrency")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value["repository"]):
        raise ValueError("Invalid GitHub repository")
    return value


class GitHub:
    def __init__(self, cfg=None):
        self.cfg = cfg or config()
        self.repo = self.cfg["repository"]
        self.token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        if not self.token:
            raise ValueError("An authorized GitHub token is unavailable")
        self.opener = urllib.request.build_opener(NoRedirect())
        self.calls = 0

    def request(self, method, path, value=None):
        if not path.startswith("/") or path.startswith("//"):
            raise ValueError("GitHub paths must be absolute API paths")
        self.calls += 1
        if self.calls > 150:
            raise ValueError("Bounded GitHub request budget exhausted; checkpoint and resume")
        data = None if value is None else json.dumps(value).encode()
        request = urllib.request.Request("https://api.github.com" + path, data=data, method=method,
            headers={"Authorization": "Bearer " + self.token, "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28", "Content-Type": "application/json",
                "User-Agent": "mynou-engineering"})
        try:
            with self.opener.open(request, timeout=30) as response:
                raw = response.read(MAX_RESPONSE + 1)
        except urllib.error.HTTPError as error:
            # Never print request headers, tokens, response data or hostile input.
            raise APIError(error.code) from None
        except urllib.error.URLError:
            raise RuntimeError("GitHub transport unavailable; preserve state and retry later") from None
        if len(raw) > MAX_RESPONSE:
            raise ValueError("GitHub response exceeds the bounded size")
        return json.loads(raw) if raw else None

    def rest(self, method, path, value=None):
        return self.request(method, "/repos/" + self.repo + "/" + path, value)

    def pages(self, path, key=None):
        result = []
        for page in range(1, 11):
            separator = "&" if "?" in path else "?"
            value = self.rest("GET", path + separator + f"per_page=100&page={page}")
            items = value[key] if key else value
            if not isinstance(items, list):
                raise ValueError("Unexpected paginated GitHub response")
            result.extend(items)
            if len(items) < 100:
                return result
        raise ValueError("GitHub pagination bound reached; narrow the work scope")

    def graphql(self, query, variables=None):
        value = self.request("POST", "/graphql", {"query": query, "variables": variables or {}})
        if value.get("errors"):
            raise RuntimeError("GitHub GraphQL operation unavailable; do not assume it succeeded")
        return value["data"]

    def file(self, path, ref):
        value = self.rest("GET", "contents/" + urllib.parse.quote(path, safe="/")
            + "?ref=" + urllib.parse.quote(ref, safe=""))
        if value.get("encoding") != "base64":
            raise ValueError("GitHub file content is not inline base64")
        raw = base64.b64decode(value["content"])
        if len(raw) > 64 * 1024:
            raise ValueError("Control file exceeds 64 KiB")
        return json.loads(raw)

    def ref(self, branch):
        return self.rest("GET", "git/ref/heads/" + urllib.parse.quote(branch, safe="/"))["object"]["sha"]

    def cas_file(self, branch, expected, path, value, message):
        # A sibling commit cannot replace a concurrently advanced ref without force.
        # GitHub's non-force fast-forward update is the atomic arbitration point.
        if self.ref(branch) != expected:
            raise RuntimeError("Execution lease changed; stop and reconstruct state")
        content = json.dumps(value, indent=2) + "\n"
        if len(content.encode()) > 64 * 1024:
            raise ValueError("Control state exceeds 64 KiB")
        tree = self.rest("POST", "git/trees", {"tree": [
            {"path": path, "mode": "100644", "type": "blob", "content": content}]})
        commit = self.rest("POST", "git/commits", {"message": message, "tree": tree["sha"], "parents": [expected]})
        try:
            self.rest("PATCH", "git/refs/heads/" + urllib.parse.quote(branch, safe="/"),
                {"sha": commit["sha"], "force": False})
        except APIError as error:
            if error.status in (409, 422):
                raise RuntimeError("Execution lease CAS lost; stop without force or blind retry") from None
            raise
        return commit["sha"]
