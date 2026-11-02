"""Resolve one executable library test harness from a successful Cargo JSON log."""
import argparse
import json
import os
from pathlib import Path


def library_test_executable(build):
    candidates = []
    finished = False
    with Path(build).open() as log:
        for line in log:
            row = json.loads(line)
            if row.get("reason") == "build-finished":
                if row.get("success") is not True:
                    raise ValueError("Cargo build did not succeed")
                finished = True
            if (row.get("reason") == "compiler-artifact" and row.get("executable")
                    and row.get("target", {}).get("kind") == ["lib"]
                    and row.get("profile", {}).get("test") is True):
                candidates.append(row["executable"])
    if not finished:
        raise ValueError("missing successful Cargo build-finished row")
    if len(candidates) != 1:
        raise ValueError("expected one compiled library test harness")
    executable = Path(candidates[0]).resolve()
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError(f"library test harness is not an executable file: {executable}")
    return str(executable)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("build_json", type=Path)
    args = parser.parse_args()
    try:
        print(library_test_executable(args.build_json))
    except (OSError, ValueError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
