"""CI-only deterministic checks for organization contracts and obvious secrets."""
import ast
import json
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parent.parent
TOKEN = re.compile(rb"(?<![A-Za-z0-9])(?:gh[psoru]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{60,255}|sk-(?:proj-)?[A-Za-z0-9_-]{40,255})(?![A-Za-z0-9])")
ROLES = {"master", "triage", "rust", "web", "ux", "security", "qa", "quality", "product"}


def check_config(cfg):
    assert cfg["schema"] == 2
    assert re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", cfg["repository"])
    assert cfg["default_branch"] == "trunk"
    actors = cfg["trusted_actors"]
    assert isinstance(actors, dict) and actors, "Commands need at least one trusted account"
    for login, identity in actors.items():
        assert re.fullmatch(r"[A-Za-z0-9-]{1,39}", login) and type(identity) is int and identity > 0
    assert type(cfg["claim_ttl_minutes"]) is int and 30 <= cfg["claim_ttl_minutes"] <= 1440
    release = cfg["release_policy"]
    assert type(release["minimum_interval_days"]) is int and 1 <= release["minimum_interval_days"] <= 90
    assert release["urgent_label"] == "release-now"
    planning = cfg["product_planning"]
    assert type(planning["issue"]) is int and planning["issue"] > 0
    assert planning["ready_minimum"] == 3 and planning["ready_maximum"] == 5
    assert type(planning["review_interval_hours"]) is int and 1 <= planning["review_interval_hours"] <= 168


def check_workflows(root):
    workflows = root / ".github/workflows"
    for path in sorted(workflows.glob("*.yml")):
        assert "pull_request_target" not in path.read_text(), "Never run PR code with base privileges: " + path.name
    ci = (workflows / "ci.yml").read_text()
    # Trunk commits are validated; only a version without a tag is published.
    assert "git/ref/tags/v$VERSION" in ci and "HTTP 404" in ci
    assert "needs.package.outputs.publish == 'true'" in ci
    policy = (workflows / "release-policy.yml").read_text()
    assert "engineering/releases.py check-pr" in policy and ": write" not in policy


def check(root=ROOT):
    check_config(json.loads((root / ".github/engineering.json").read_text()))
    catalog = json.loads((root / "engineering/roles.json").read_text())["roles"]
    assert ROLES.issubset(catalog)
    for role, path in catalog.items():
        assert re.fullmatch(r"[a-z][a-z0-9-]{0,47}", role)
        assert path == f".agents/skills/mynou-{role}/SKILL.md"
        text = (root / path).read_text()
        assert text.startswith("---\nname: mynou-" + role + "\ndescription: ")
        assert "\n---\n" in text and len(text) < 8000
    instructions = (root / "AGENTS.md").read_text()
    assert len(instructions.splitlines()) <= 65
    assert "/assign" in instructions and "No local tests" in instructions
    assert "zero Cargo dependencies" in instructions and "CI" in instructions
    cargo = tomllib.loads((root / "Cargo.toml").read_text())
    assert not any(cargo.get(k) for k in ["dependencies", "dev-dependencies", "build-dependencies"])
    for target in cargo.get("target", {}).values():
        assert not any(target.get(k) for k in ["dependencies", "dev-dependencies", "build-dependencies"])
    for path in (root / "engineering").rglob("*.py"):
        ast.parse(path.read_text(), filename=str(path))
    tracked = subprocess.check_output(["git", "ls-files", "-z"], cwd=root).split(b"\0")
    for name in filter(None, tracked):
        path = root / name.decode()
        if path.is_file() and path.stat().st_size <= 2 * 1024 * 1024:
            assert TOKEN.search(path.read_bytes()) is None, "Credential-pattern finding in " + name.decode()
    check_workflows(root)
    print(f"Organization policy passed: {len(catalog)} Skills; comment coordination; weekly releases; std-only tooling; no high-confidence token findings.")


if __name__ == "__main__":
    check()
