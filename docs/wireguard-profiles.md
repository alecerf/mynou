# Offline WireGuard profiles

The Rust API `mynou::wireguard::profile::parse(&str)` validates the first
supported `.conf` subset and returns a secret-bearing `Profile`. Consuming it
with `into_tunnel_config()` produces the existing userspace runtime's
configuration. Both operations are offline: they perform no file access, DNS,
socket creation, tunnel startup or configuration changes.

This delivers [#279](https://github.com/alecerf/mynou/issues/279), a prerequisite
of [#222](https://github.com/alecerf/mynou/issues/222). There is no CLI/API import
or VPN setting yet. Torrent transport integration remains
[#221](https://github.com/alecerf/mynou/issues/221); lifecycle, private-file import
and persistence remain #222; isolation/race/resource evidence and the macOS
migration guide remain [#224](https://github.com/alecerf/mynou/issues/224).
Mynou torrent traffic is still unprotected. Existing VPN deployments must stay
in place. See the [design](wireguard-design.md) for the intended boundary.

## Accepted subset

Exactly one `[Interface]` section must precede exactly one `[Peer]` section.
Names are case sensitive. Fields may occur in any order within their section.
Comments start with `#`; LF and CRLF, ASCII spaces and tabs, blank lines and a
missing final newline are accepted. Quotes, BOMs, non-ASCII bytes, other control
characters and bare carriage returns are rejected, even in comments.

| Section | Field | Accepted value |
| --- | --- | --- |
| Interface | `PrivateKey` | Required, canonical standard base64 encoding of 32 bytes; all-zero key rejected |
| Interface | `Address` | Required, one usable IPv4 address with an explicit decimal CIDR prefix, 0–32 |
| Interface | `DNS` | Required, one to three distinct literal usable IPv4 addresses, comma separated |
| Interface | `MTU` | Optional, decimal 576–1424; defaults to 1424 |
| Peer | `PublicKey` | Required, canonical base64 encoding of 32 bytes; low-order keys and the local public identity rejected |
| Peer | `PresharedKey` | Optional, canonical base64 encoding of 32 bytes; an all-zero PSK has the protocol's no-PSK meaning |
| Peer | `AllowedIPs` | Required, exactly `0.0.0.0/0` |
| Peer | `Endpoint` | Required, literal usable IPv4 address and decimal UDP port 1–65535 |
| Peer | `PersistentKeepalive` | Optional, decimal 0–65535 seconds; absent or zero disables it |

Usable addresses exclude `0.0.0.0/8`, loopback, multicast and reserved class E
(including the limited broadcast address). Private IPv4 addresses are accepted.
The interface prefix is retained by `address_prefix()`; it does not change the
full IPv4 route from `AllowedIPs` or create a host route. Inner DNS uses the
explicit resolvers; no host resolver is consulted. Endpoint hostnames and IPv6
are rejected, so conversion cannot construct `Endpoint::Name`.

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

This parser accepts in-memory text only. Future durable import must use the
existing private regular-file, permission and link/race checks before reading
or saving secrets; it must redact CLI/API output. Passing validation is not
permission to create a protected engine before the remaining delivery gates.

## Validation

Native unit fixtures cover supported settings and conversion, defaults and
exact bounds, comments and line endings, all missing/duplicate fields,
unsupported directives/sections, malformed addresses/routes/resolvers/ports,
key encoding and low-order/self peers, numeric bounds and redacted diagnostics.
They use synthetic values and RFC 7748 test material, without provider accounts
or network access. The protocol tests continue to use the independent vectors
from `.github/scripts/crypto_fixtures.py`; see [local validation](validation.md#local-validation).
