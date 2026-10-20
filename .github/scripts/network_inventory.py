"""Source-policy inventory of direct torrent networking; Python stdlib only.

A lexical aid to call-graph review, not a proof: see docs/network-inventory.md.
"""
import argparse
from collections import Counter
import os
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[2]
INVENTORY = Path(__file__).with_name("network_inventory.tsv")
# Torrent modules and the inspection/orchestration callers of the design table.
SCOPE = ("src/torrent.rs", "src/torrent/", "src/pack/shared.rs", "src/pack/automatic.rs",
         "src/library/groups.rs", "src/irc/routing.rs", "src/engine.rs", "src/engine/",
         "src/vpn.rs", "src/vpn/")
CRATE_ROOTS = ("src/lib.rs", "src/main.rs")
SOCKETS = {"TcpStream", "TcpListener", "UdpSocket", "ToSocketAddrs"}
RESOLVERS = {"to_socket_addrs"}
HTTP = "HttpClient"
ITEMS = {"fn", "mod", "impl", "trait", "struct", "enum", "union"}
PUNCT3 = {"..=", "<<=", ">>=", "..."}
PUNCT2 = {"::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*=", "/=", "..", "<<", ">>"}


def tokens(text):
    """Identifiers and punctuation with line numbers; comments, literals and lifetimes dropped."""
    out, i, line, n = [], 0, 1, len(text)
    while i < n:
        c = text[i]
        if c == "\n":
            line += 1
            i += 1
        elif c.isspace():
            i += 1
        elif text.startswith("//", i):
            while i < n and text[i] != "\n":
                i += 1
        elif text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    line += text[i] == "\n"
                    i += 1
        elif c in "rb" and _raw_string(text, i):
            j = i + (2 if text.startswith("br", i) else 1)
            hashes = 0
            while text[j] == "#":
                hashes, j = hashes + 1, j + 1
            end = text.find('"' + "#" * hashes, j + 1)
            end = n if end < 0 else end + 1 + hashes
            line += text.count("\n", i, end)
            i = end
        elif c == '"' or (c == "b" and text.startswith('b"', i)):
            i += 1 if c == '"' else 2
            while i < n and text[i] != '"':
                if text[i] == "\\":
                    i += 1
                line += i < n and text[i] == "\n"
                i += 1
            i += 1
        elif c == "'" or (c == "b" and text.startswith("b'", i)):
            j = i + (1 if c == "'" else 2)
            if j < n and text[j] == "\\":
                end = text.find("'", j + 2)
                i = n if end < 0 else end + 1
            elif j + 1 < n and text[j + 1] == "'":
                i = j + 2
            else:
                # A lifetime or label: skip the quote; the name is not an item.
                i = j
                while i < n and (text[i].isalnum() or text[i] == "_"):
                    i += 1
        elif c.isalpha() or c == "_":
            j = i
            while j < n and (text[j].isalnum() or text[j] == "_"):
                j += 1
            word = text[i:j]
            if word == "r" and text.startswith("#", j) and j + 1 < n and (text[j + 1].isalpha() or text[j + 1] == "_"):
                i = j + 1
                continue
            out.append((word, line))
            i = j
        elif c.isdigit():
            while i < n and (text[i].isalnum() or text[i] in "_."):
                if text[i] == "." and not (i + 1 < n and text[i + 1].isdigit()):
                    break
                i += 1
        else:
            for size, table in ((3, PUNCT3), (2, PUNCT2)):
                if text[i:i + size] in table:
                    out.append((text[i:i + size], line))
                    i += size
                    break
            else:
                out.append((c, line))
                i += 1
    return out


def _raw_string(text, i):
    j = i + (2 if text.startswith("br", i) else 1)
    if text[i] == "b" and not text.startswith("br", i):
        return False
    if i and (text[i - 1].isalnum() or text[i - 1] == "_"):
        return False
    while j < len(text) and text[j] == "#":
        j += 1
    return j < len(text) and text[j] == '"'


def _attribute(toks, i):
    """Return (end index, words) of an attribute starting at `#`, or None."""
    j = i + 1
    if j < len(toks) and toks[j][0] == "!":
        j += 1
    if j >= len(toks) or toks[j][0] != "[":
        return None
    depth, k = 0, j
    while k < len(toks):
        depth += {"[": 1, "]": -1}.get(toks[k][0], 0)
        if depth == 0:
            return k + 1, [t for t, _ in toks[j + 1:k]]
        k += 1
    return len(toks), []


def _skip_item(toks, i):
    """End index of the item or statement starting at i (after its attributes)."""
    depth, keyword = 0, None
    k = i
    while k < len(toks):
        word = toks[k][0]
        if keyword is None and word in ITEMS | {"use", "let", "const", "static", "type", "extern"}:
            keyword = word
        if word in "([{":
            depth += 1
        elif word in ")]}":
            if depth == 0:
                return k
            depth -= 1
            if depth == 0 and word == "}" and keyword in ITEMS | {"extern"}:
                return k + 1
        elif depth == 0 and word in ";,":
            return k + 1
        k += 1
    return k


def strip_tests(toks):
    """Drop `#[cfg(test)]` and `#[test]` items; return kept tokens and test `mod x;` names."""
    kept, test_modules, i = [], [], 0
    while i < len(toks):
        if toks[i][0] == "#":
            attribute = _attribute(toks, i)
            if attribute:
                end, words = attribute
                if words in (["cfg", "(", "test", ")"], ["test"]):
                    j = end
                    while j < len(toks) and toks[j][0] == "#" and _attribute(toks, j):
                        j = _attribute(toks, j)[0]
                    stop = _skip_item(toks, j)
                    item = [t for t, _ in toks[j:stop]]
                    if "mod" in item and item[-1] == ";":
                        test_modules.append(item[item.index("mod") + 1])
                    i = stop
                    continue
        kept.append(toks[i])
        i += 1
    return kept, test_modules


def modules(root=ROOT):
    """Non-test source files reachable from the crate roots, following `mod` declarations."""
    pending = [root / path for path in CRATE_ROOTS if (root / path).is_file()]
    seen = set()
    while pending:
        path = pending.pop()
        if path in seen:
            continue
        seen.add(path)
        toks, test_modules = strip_tests(tokens(path.read_text()))
        owner = path.parent if path.name in {"lib.rs", "main.rs", "mod.rs"} else path.with_suffix("")
        inline, depth, location, k = [], 0, None, 0
        while k < len(toks):
            word = toks[k][0]
            attribute = _attribute(toks, k) if word == "#" else None
            if attribute:
                if attribute[1][:2] == ["path", "="]:
                    location = _path_attribute(path, toks[k][1])
                k = attribute[0]
                continue
            if word == "{":
                depth += 1
            elif word == "}":
                depth -= 1
                while inline and inline[-1][1] > depth:
                    inline.pop()
            elif word == "mod" and k + 2 < len(toks):
                name, after = toks[k + 1][0], toks[k + 2][0]
                if after == "{":
                    inline.append((name, depth + 1))
                elif after == ";" and name not in test_modules:
                    base = owner.joinpath(*[m for m, _ in inline])
                    if location:
                        target = Path(os.path.normpath(path.parent / location))
                    elif (base / f"{name}.rs").is_file():
                        target = base / f"{name}.rs"
                    else:
                        target = base / name / "mod.rs"
                    if target.is_file():
                        pending.append(target)
            if word in {";", "{", "}"}:
                location = None
            k += 1
    return sorted(seen)


def _path_attribute(path, line):
    text = path.read_text().splitlines()[line - 1]
    start = text.find('"')
    end = text.find('"', start + 1)
    return text[start + 1:end] if 0 <= start < end else None


def helpers(files, root=ROOT):
    """Out-of-scope functions whose signatures carry a physical socket, by qualified call form."""
    names, aliases = set(), {}
    for path in files:
        toks, _ = strip_tests(tokens(path.read_text()))
        aliases.update(_aliases(toks, aliases))
        if in_scope(path, root):
            continue
        module = path.parent.name if path.name == "mod.rs" else path.stem
        owners = []
        depth = 0
        for k, (word, _) in enumerate(toks):
            if word == "impl" and _item_impl(toks, k):
                owners.append([_impl_self(toks, k), None])
            elif word == "{":
                depth += 1
                if owners and owners[-1][1] is None:
                    owners[-1][1] = depth
            elif word == "}":
                if owners and owners[-1][1] == depth:
                    owners.pop()
                depth -= 1
            elif word == "fn" and _named(toks, k + 1):
                signature = {t for t, _ in toks[k:_signature_end(toks, k)]}
                if signature & (SOCKETS | set(aliases)):
                    owner = owners[-1][0] if owners and owners[-1][1] == depth else module
                    names.add((owner, toks[k + 1][0]))
    return names, aliases


def _named(toks, k):
    return k < len(toks) and (toks[k][0][0].isalpha() or toks[k][0][0] == "_")


def _item_impl(toks, k):
    """`impl` opening an item, not `-> impl Trait` or `x: impl Trait`."""
    return not k or toks[k - 1][0] in {"}", ";", "{", "]", "pub", "unsafe", "default", ")"}


def _signature_end(toks, k):
    """Index of the `{` or `;` ending the item at k; `[u8; 20]` and `(..)` are nested."""
    nest = 0
    while k < len(toks):
        word = toks[k][0]
        nest += {"(": 1, "[": 1, ")": -1, "]": -1}.get(word, 0)
        if nest == 0 and word in {"{", ";"}:
            return k
        k += 1
    return k


def _impl_self(toks, k):
    """Last path segment of the implemented type: `impl Drop for a::Client<T>` -> Client."""
    words, depth, j = [], 0, k + 1
    while j < len(toks) and toks[j][0] not in {"{", ";", "where"}:
        word = toks[j][0]
        depth += {"<": 1, ">": -1}.get(word, 0)
        if depth == 0 and word == "for":
            words = []
        elif depth == 0 and (word[0].isalpha() or word[0] == "_") and word not in {"dyn", "mut", "const"}:
            words.append(word)
        j += 1
    return words[-1] if words else "impl"


def _aliases(toks, known):
    """`use X as Y` and `type Y = ...` names that stand for a socket or HttpClient."""
    found, watched = {}, SOCKETS | {HTTP}
    for k, (word, _) in enumerate(toks):
        if word == "as" and 0 < k < len(toks) - 1:
            source = toks[k - 1][0]
            canonical = source if source in watched else known.get(source) or found.get(source)
            if canonical:
                found[toks[k + 1][0]] = canonical
        elif word == "type" and k + 2 < len(toks) and toks[k + 2][0] in {"=", "<"}:
            j = k + 2
            while j < len(toks) and toks[j][0] != ";":
                source = toks[j][0]
                canonical = source if source in watched else known.get(source) or found.get(source)
                if canonical:
                    found[toks[k + 1][0]] = canonical
                    break
                j += 1
    return found


def in_scope(path, root=ROOT):
    relative = path.relative_to(root).as_posix()
    return any(relative == entry or (entry.endswith("/") and relative.startswith(entry)) for entry in SCOPE)


def findings(path, helper_names, aliases, root=ROOT):
    """Counter of (file, enclosing item, construct) plus the first line of each key."""
    toks, _ = strip_tests(tokens(path.read_text()))
    relative = path.relative_to(root).as_posix()
    counts, lines = Counter(), {}
    scopes, pending, nest = [], None, 0
    qualified = {owner for owner, _ in helper_names}

    def record(construct, line):
        # A signature belongs to its function; impl names already qualify methods.
        frames = scopes + ([pending] if pending and pending[1] == "fn" else [])
        names = [name for name, kind in frames if name and kind != "impl"]
        names = names or [name for name, kind in frames if name][-1:]
        key = (relative, "::".join(names) or "<module>", construct)
        counts[key] += 1
        lines.setdefault(key, line)

    for k, (word, line) in enumerate(toks):
        before = toks[k - 1][0] if k else ""
        after = toks[k + 1][0] if k + 1 < len(toks) else ""
        nest += {"(": 1, "[": 1, ")": -1, "]": -1}.get(word, 0)
        if word == "fn" and _named(toks, k + 1):
            owner = next((name for name, kind in reversed(scopes) if kind == "impl"), None)
            pending = (after if any(kind == "fn" for _, kind in scopes) or not owner else f"{owner}::{after}", "fn")
        elif word == "impl" and _item_impl(toks, k):
            pending = (_impl_self(toks, k), "impl")
        elif word in {"struct", "enum", "trait", "mod", "union"} and after and before != "::":
            pending = (after, word)
        elif word == "{":
            scopes.append(pending if pending else (None, None))
            pending = None
            continue
        elif word == "}":
            if scopes:
                scopes.pop()
            continue
        elif word == ";" and nest == 0:
            pending = None
            continue
        if before == "." or before == "fn":
            if word in RESOLVERS:
                record(word, line)
            continue
        canonical = word if word in SOCKETS | {HTTP} else aliases.get(word)
        if canonical in SOCKETS and not _defining_alias(toks, k):
            record(canonical, line)
        elif canonical == HTTP and after == "::" and k + 2 < len(toks):
            method = toks[k + 2][0]
            if method == "default" or (method == "new" and not _chain_has(toks, k + 3, "with_connector")):
                record(f"{HTTP}::{method}", line)
        elif word in RESOLVERS:
            record(word, line)
        elif word == "net" and after == "::" and k + 2 < len(toks) and toks[k + 2][0] == "*":
            record("std::net::*", line)
        elif word in qualified and after == "::" and k + 2 < len(toks) and (word, toks[k + 2][0]) in helper_names:
            record(f"{word}::{toks[k + 2][0]}", line)
    return counts, lines


def _defining_alias(toks, k):
    """True for Y in `X as Y` / `type Y =`; the source X is the recorded site."""
    before = toks[k - 1][0] if k else ""
    return before == "as" or (before == "type" and k + 1 < len(toks) and toks[k + 1][0] in {"=", "<"})


def _chain_has(toks, k, name):
    depth = 0
    while k < len(toks):
        word = toks[k][0]
        if word in "([{":
            depth += 1
        elif word in ")]}":
            depth -= 1
            if depth < 0:
                return False
        elif depth == 0 and word in {";", ","}:
            return False
        elif word == name and depth == 0:
            return True
        k += 1
    return False


def scan(root=ROOT):
    root = root.resolve()
    files = modules(root)
    helper_names, aliases = helpers(files, root)
    counts, lines = Counter(), {}
    for path in files:
        if in_scope(path, root):
            found, first = findings(path, helper_names, aliases, root)
            counts.update(found)
            for key, line in first.items():
                lines.setdefault(key, line)
    return counts, lines


def read_inventory(path):
    entries = Counter()
    for raw in path.read_text().splitlines():
        if raw and not raw.startswith("#"):
            file, item, construct, count = raw.split("\t")
            entries[(file, item, construct)] = int(count)
    return entries


def render(counts):
    lines = ["# Direct torrent network sites; regenerate with network_inventory.py --write.",
             "# file\titem\tconstruct\tcount"]
    lines += ["\t".join((*key, str(counts[key]))) for key in sorted(counts)]
    return "\n".join(lines) + "\n"


def compare(counts, expected):
    """Messages for new/increased and stale/decreased sites; empty when they match."""
    problems = []
    for key in sorted(set(counts) | set(expected)):
        found, allowed = counts.get(key, 0), expected.get(key, 0)
        if found > allowed:
            problems.append(f"new direct site: {' | '.join(key)} ({allowed} -> {found})")
        elif found < allowed:
            problems.append(f"stale inventory entry: {' | '.join(key)} ({allowed} -> {found})")
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--inventory", type=Path, default=INVENTORY)
    parser.add_argument("--write", action="store_true", help="replace the inventory with the current sites")
    args = parser.parse_args(argv)
    counts, lines = scan(args.root.resolve())
    if args.write:
        args.inventory.write_text(render(counts))
        return 0
    problems = compare(counts, read_inventory(args.inventory))
    # After the migration, updating the inventory must not authorize a new
    # ordinary-network site elsewhere in the torrent boundary.
    for key in sorted(counts):
        file, item, _ = key
        if file != "src/torrent/transport.rs" or not (
            item in {"<module>", "Direct", "DirectListener"}
            or item.startswith(("Direct::", "DirectListener::"))
        ):
            problems.append(f"direct site outside Direct: {' | '.join(key)} ({counts[key]})")
    for problem in problems:
        key = tuple(problem.split(": ", 1)[1].rsplit(" (", 1)[0].split(" | "))
        where = f"{key[0]}:{lines[key]}: " if key in lines else ""
        print(f"{where}{problem}", file=sys.stderr)
    if problems:
        print("Convert the site through the transport context, or update the reviewed "
              "inventory with --write (docs/network-inventory.md).", file=sys.stderr)
        return 1
    print(f"{sum(counts.values())} inventoried direct sites in {len({k[0] for k in counts})} files match.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
