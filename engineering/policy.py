"""CI-only deterministic checks for organization contracts and obvious secrets."""
import ast
import json
from pathlib import Path
import re
import subprocess
import tomllib

from github import control_reference

ROOT = Path(__file__).resolve().parent.parent
TOKEN = re.compile(rb"(?<![A-Za-z0-9])(?:gh[psoru]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{60,255}|sk-(?:proj-)?[A-Za-z0-9_-]{40,255})(?![A-Za-z0-9])")


def check_execution(cfg):
    """Validate policy without confusing continuous work with a permanent lease."""
    assert cfg["execution_mode"] == "continuous"
    assert "slice_minutes" not in cfg, "An artificial wake deadline is obsolete"
    assert cfg["max_active_agents"] == 1
    assert cfg["lease_minutes"] == 45
    assert type(cfg["heartbeat_minutes"]) is int and 1 <= cfg["heartbeat_minutes"] <= 15
    assert cfg["no_progress_limit"] == 3


def check_product(cfg, catalog):
    planning = cfg["product_planning"]
    assert "product" in catalog
    assert type(planning["issue"]) is int and planning["issue"] > 0
    assert type(planning["ready_minimum"]) is int and planning["ready_minimum"] == 3
    assert type(planning["ready_maximum"]) is int and planning["ready_maximum"] == 5


def check(root=ROOT):
    cfg = json.loads((root / ".github/engineering.json").read_text())
    assert cfg["schema"] == 1
    check_execution(cfg)
    control_reference(cfg)
    from migration import settings
    settings(cfg)
    catalog = json.loads((root / "engineering/roles.json").read_text())["roles"]
    assert {"master", "triage", "rust", "web", "ux", "security", "qa", "quality"}.issubset(catalog)
    check_product(cfg, catalog)
    for role, path in catalog.items():
        assert re.fullmatch(r"[a-z][a-z0-9-]{0,47}", role)
        assert path == f".agents/skills/mynou-{role}/SKILL.md"
        text = (root / path).read_text()
        assert text.startswith("---\nname: mynou-" + role + "\ndescription: ")
        assert "\n---\n" in text and len(text) < 8000
    instructions = (root / "AGENTS.md").read_text()
    assert len(instructions.splitlines()) <= 65
    assert "MAX_ACTIVE_AGENTS = 1" in instructions and "No local tests" in instructions
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
    privileged = (root / ".github/workflows/engineering-delivery.yml").read_text()
    assert "ref: trunk" in privileged and "persist-credentials: false" in privileged
    assert "pull_request_target" not in privileged
    print(f"Organization policy passed: {len(catalog)} Skills; serialized lease; std-only tooling; no high-confidence token findings.")


if __name__ == "__main__":
    check()
