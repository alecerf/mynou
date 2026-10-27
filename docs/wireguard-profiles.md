# Offline WireGuard profiles

The Rust API `mynou::wireguard::profile::parse(&str)` validates the first
supported `.conf` subset and returns a secret-bearing `Profile`. Consuming it
with `into_tunnel_config()` produces the existing userspace runtime's
configuration. Both operations are offline: they perform no file access, DNS,
socket creation, tunnel startup or configuration changes.

The offline parser (#279) and private-file reader (#281) are reused by the
[operator import and lifecycle controls](wireguard-operator.md) of #222. The
parser itself still performs no networking or persistence. Complete
isolation/race/resource evidence is [recorded here](wireguard-acceptance.md);
the [macOS migration guide](wireguard-macos-migration.md) covers deployment.

## Accepted subset

Exactly one `[Interface]` section must precede exactly one `[Peer]` section.
Names are case sensitive. Fields may occur in any order within their section.
Comments start with `#`; LF and CRLF, ASCII spaces and tabs, blank lines and a
missing final newline are accepted. Quotes, BOMs, non-ASCII bytes, other control
characters and bare carriage returns are rejected, even in comments.

| Section | Field | Accepted value |
| --- | --- | --- |
| Interface | `PrivateKey` | Required, canonical standard base64 encoding of 32 bytes; all-zero key rejected |
| Interface | `Address` | Required, one usable IPv4 address with an explicit decimal CIDR prefix, 0–32, optionally followed by one unicast IPv6 address with a prefix of 0–128 |
| Interface | `DNS` | Required, one to three distinct literal addresses, comma separated, at least one of them a usable IPv4 address |
| Interface | `MTU` | Optional, decimal 576–1424; defaults to 1424 |
| Peer | `PublicKey` | Required, canonical base64 encoding of 32 bytes; low-order keys and the local public identity rejected |
| Peer | `PresharedKey` | Optional, canonical base64 encoding of 32 bytes; an all-zero PSK has the protocol's no-PSK meaning |
| Peer | `AllowedIPs` | Required, exactly `0.0.0.0/0`, or `0.0.0.0/0` with `::/0` |
| Peer | `Endpoint` | Required, literal usable IPv4 address and decimal UDP port 1–65535 |
| Peer | `PersistentKeepalive` | Optional, decimal 0–65535 seconds; absent or zero disables it |

Usable addresses exclude `0.0.0.0/8`, loopback, multicast and reserved class E
(including the limited broadcast address). Private IPv4 addresses are accepted.
The interface prefix is retained by `address_prefix()`; it does not change the
full IPv4 route from `AllowedIPs` or create a host route. Inner DNS uses the
explicit resolvers; no host resolver is consulted. Endpoint hostnames and IPv6
endpoints are rejected; the tunnel runtime itself accepts only a literal IPv4 endpoint.

The tunnel carries IPv6 too when the profile has an IPv6 address, the `::/0`
route and an MTU of at least 1280: the IPv6 address becomes the inner IPv6
source, every IPv6 source is accepted from the peer, and the IPv6 resolvers are
asked after the IPv4 ones in the profile order. Otherwise the IPv6 entries are
only kept in the stored profile, and so is a link-local or other address the
stack cannot own, rather than refusing a profile. No IPv6 traffic ever falls
back to the host network. `Profile::ipv6` reports the result. The NAT-PMP
gateway is the first IPv4 resolver.

All fields are unique: repeated `DNS` or `AllowedIPs` lines are rejected too.
Unknown fields, additional peers/addresses, partial routes, `ListenPort`,
`SaveConfig`, `Table`, `FwMark`, hooks (`PreUp`, `PostUp`, `PreDown`, `PostDown`)
and scripts are unsupported. Nothing executes a directive. Numbers use unsigned
decimal digits only, without signs, separators, units or implicit bases. Base64
requires exactly 44 characters with one final `=` and zero unused padding bits;
whitespace inside keys, URL-safe alphabet substitutions and key-file references
are rejected.

## Bounds and confidentiality

The complete input is at most 16,384 bytes, including comments and line endings;
at most 256 lines and 1,024 bytes per line, excluding LF/CRLF. Oversize input is
rejected before field parsing. There are nine possible fields and at most three
DNS servers. Key decoding uses fixed arrays. Peer-key validation performs bounded
offline X25519 work with the existing crypto implementation.

`Profile` has private fields and no `Debug`, `Clone` or serialization support.
`ProfileError` provides a one-based line number (zero for whole-profile or
missing-field errors) and a typed reason. `Display` and `Debug` never copy input
values, unknown names, endpoints or credentials. Only parser-owned known field
names appear in errors. Do not log the caller's original profile text or extract
and log keys from the resulting `TunnelConfig`. Safe Rust does not guarantee
key zeroization after disposal; no constant-time or audited-crypto claim is made.

The parser accepts in-memory text. The separate read-only API below loads private
files; the operator layer separately implements durable import and API controls.
Passing validation alone does not establish a protected connection.

## Read a private file

`mynou::wireguard::profile::read_private_file(&Path)` delivers the file-reading
prerequisite [#281](https://github.com/alecerf/mynou/issues/281) and returns the
same secret-bearing `Profile`. It accepts an existing regular file with mode
`0400` or `0600`, one hard link, and no symbolic links in its path. Parent (`..`)
components are rejected so ancestor checks cannot hide a symbolic link. Use a
physical path (for example `/private/tmp`, rather than macOS's `/tmp` alias).
The function leaves source bytes and permissions untouched and writes no
configuration, backups or temporary files. It performs no network activity.

Loading uses the repository's `O_NOFOLLOW | O_NONBLOCK` open flags, then checks
the descriptor against the inspected device/inode before reading. Descriptor
and path checks before and after reading compare mode, link count, owner/group,
size and modification/change timestamps. A read consumes at most 16,385 bytes
(the parser limit plus one detection byte); initially oversized files fail
before reading. Invalid UTF-8 fails explicitly, and parser rules remain intact.

`ImportError` has typed file, change, size, encoding and parser reasons. Its
`Display`, `Debug` and error chain retain no paths, input bytes or OS error
payloads. File errors deliberately do not reproduce the underlying OS message;
parser failures keep their redacted `ProfileError` line and reason. Callers must
also avoid logging the original path/text or extracting secret fields.

These safe-std checks detect observed substitutions and changes; they are not
an atomic filesystem snapshot or an OS sandbox. Keep the file and its parent
directories under trusted control. A process able to mutate them can race
ancestor traversal, restore a path between checks or modify a file after the
last check. The API does not establish current-user ownership or guarantee
memory zeroization. It does not persist credentials, enable protection or
supply operator lifecycle fences; those belong to the separate operator layer.

## Validation

Native unit fixtures cover supported settings and conversion, defaults and
exact bounds, comments and line endings, all missing/duplicate fields,
unsupported directives/sections, malformed addresses/routes/resolvers/ports,
key encoding and low-order/self peers, numeric bounds and redacted diagnostics.
They use synthetic values and RFC 7748 test material, without provider accounts
or network access. The protocol tests continue to use the independent vectors
from `.github/scripts/crypto_fixtures.py`; see [local validation](validation.md#local-validation).

Private-file fixtures additionally cover read-only/read-write private modes,
unchanged source bytes/metadata, unsafe modes, hard/symbolic links and ancestor
links, non-regular/missing paths, deterministic replacements and content/mode/link
changes around open/read, exact file-size bounds, encoding and error redaction.
