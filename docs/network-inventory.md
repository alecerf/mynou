# Torrent network inventory

[`network_inventory.py`](../.github/scripts/network_inventory.py) is the
source-policy part of the "Complete network inventory" gate in the
[WireGuard design](wireguard-design.md#verification-gates-before-protected-delivery).
It lists every place where torrent code still reaches the ordinary network
directly. CI fails when that list changes without review.

The script uses only the Python standard library. The `validate` job of Mynou CI
runs it on every product change. The Repository checks workflow runs its
fixtures whenever the tooling changes. To run it locally:

```sh
python3 -B .github/scripts/network_inventory.py
```

## Scope

The script scans these files:

- the torrent modules: `src/torrent.rs` and `src/torrent/`;
- the inspection and orchestration callers named in the design table:
  `src/pack/shared.rs`, `src/pack/automatic.rs`, `src/library/groups.rs`,
  `src/irc/routing.rs`, `src/engine.rs` and `src/engine/`.

It finds source files by following `mod` declarations, including
`#[path = ...]`, from `src/lib.rs` and `src/main.rs`. It skips items marked
exactly `#[cfg(test)]` or `#[test]`, and the files of `#[cfg(test)] mod name;`
declarations. Other conditions, such as `cfg(not(test))` or
`cfg(any(test, ...))`, are still scanned.

Plex, catalog, indexer, IRC session, management and tunnel code are not in the
scope. That traffic stays outside the tunnel.

## Reported constructs

- The physical socket types `TcpStream`, `TcpListener` and `UdpSocket`, wherever
  they are named: imports, fields, signatures and calls.
- Host name resolution: `ToSocketAddrs` and `.to_socket_addrs()`.
- Default HTTP construction: `HttpClient::new` without
  `.with_connector(...)` in the same expression, and `HttpClient::default`.
- `std::net::*` glob imports. A glob would hide the names above.
- Aliases. `use X as Y` and `type Y = ...` names that refer to the constructs
  above are followed across the whole crate, and their uses are reported under
  the original name.
- Socket helpers defined outside the scope. A function anywhere else in the crate
  whose signature names a physical socket is reported when the scope calls it as
  `Type::function` or `module::function`. Examples are `net::blocking`,
  `DeadlineStream::new` and `TlsStream::connect`.

Each finding is keyed by file, enclosing item (for example
`Peer::connect_with_stop`; `<module>` for imports) and construct. The script
counts the findings and compares them with the reviewed list in
[`network_inventory.tsv`](../.github/scripts/network_inventory.tsv):

- a new key, or a higher count, fails as `new direct site`;
- a missing key, or a lower count, fails as `stale inventory entry`.

A conversion in #221 must therefore remove its entries in the same change. A
reviewer sees a new direct site as a diff of that file. After converting a site
or deliberately accepting one, regenerate the file with `--write` and review the
diff. The list is not an allowlist to grow freely: the protected transport needs
it to reach zero for every site the design requires to move.

## Limits

This is a lexical check, a help for call-graph review. It does not prove
properties of arbitrary Rust programs. It does not detect:

- macros that expand to network code;
- trait methods, generic code or function pointers that reach a socket without
  naming one of the reported constructs in the scope;
- method calls on values whose type is not written in the scope, other than
  `.to_socket_addrs()`;
- `Default::default()` for an inferred `HttpClient`;
- helpers whose signature does not name a socket, such as a function that opens
  a connection internally and returns bytes. Such helpers are found through the
  public torrent entry points they serve, for example `inspect_metadata`.

These gaps are why the design also requires runtime isolation evidence:
attempted-socket and name-lookup observation in #224. The fixtures in
[`test_network_inventory.py`](../.github/scripts/tests/test_network_inventory.py)
show the check failing on each reported construct, on aliases, on a glob, on a
new site, on a higher count and on a stale entry. They also show that comments,
literals, connector-backed clients and test-only code are ignored.
