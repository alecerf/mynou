"""Small bounded GitHub REST client for engineering tooling; Python std only."""
import json
import os
from pathlib import Path
import re
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
MAX_RESPONSE = 4 * 1024 * 1024
MAX_REQUESTS = 200
# Content-addressed reads whose answer can never change: one fetch per process.
IMMUTABLE = re.compile(r"/repos/[^/]+/[^/]+/(?:git/commits/[0-9a-f]{40}"
                       r"|compare/[0-9a-f]{40}\.\.\.[0-9a-f]{40}"
                       r"|contents/[^?#]+\?ref=[0-9a-f]{40})")


class APIError(RuntimeError):
    def __init__(self, status):
        self.status = status
        super().__init__(f"GitHub request failed with HTTP {status}; refresh state before retrying")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise APIError(code)


def config(root=ROOT):
    value = json.loads((root / ".github/engineering.json").read_text())
    if value.get("schema") != 2:
        raise ValueError("Unsupported engineering configuration schema")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value.get("repository") or ""):
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
        self.immutable = {}

    def consume_request(self):
        self.calls += 1
        if self.calls > MAX_REQUESTS:
            raise ValueError("Bounded GitHub request budget exhausted; narrow the query")

    def request(self, method, path, value=None):
        if not path.startswith("/") or path.startswith("//"):
            raise ValueError("GitHub paths must be absolute API paths")
        # Issues, comments, PRs and refs are always re-read; only commit-addressed
        # objects are reused, as fresh copies.
        cacheable = method == "GET" and IMMUTABLE.fullmatch(path) is not None
        if cacheable and path in self.immutable:
            return json.loads(self.immutable[path])
        self.consume_request()
        data = None if value is None else json.dumps(value).encode()
        request = urllib.request.Request("https://api.github.com" + path, data=data, method=method,
            headers={"Authorization": "Bearer " + self.token, "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28", "Content-Type": "application/json",
                "User-Agent": "mynou-engineering"})
        # Claims and verdicts must be current: ask caches to revalidate.
        if method == "GET":
            request.add_header("Cache-Control", "no-cache")
        try:
            with self.opener.open(request, timeout=30) as response:
                raw = response.read(MAX_RESPONSE + 1)
        except urllib.error.HTTPError as error:
            # Never print request headers, tokens, response data or hostile input.
            raise APIError(error.code) from None
        except urllib.error.URLError:
            raise RuntimeError("GitHub transport unavailable; retry later") from None
        if len(raw) > MAX_RESPONSE:
            raise ValueError("GitHub response exceeds the bounded size")
        if cacheable and raw:
            self.immutable[path] = raw
        return json.loads(raw) if raw else None

    def rest(self, method, path, value=None):
        root = "/repos/" + self.repo
        return self.request(method, root if path == "" else root + "/" + path, value)

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
        raise ValueError("GitHub pagination bound reached; narrow the query")
