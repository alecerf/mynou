# Offline inner IPv4/UDP codec, virtual UDP sockets and DNS stub client

[#192](https://github.com/alecerf/mynou/issues/192) adds the first userspace IP
stack layer described in [the design](/docs/wireguard-design.md). It only sees
inner IP datagrams: `Session::receive` of [the transport](/docs/wireguard-transport.md)
returns authenticated padded bytes, and this layer is what must validate them
before any use. There is no socket, tunnel coupling, engine, profile or torrent
caller, so **this is not routing, host DNS, TCP or VPN protection, and it is not
independent interoperability evidence**. Parent #111 stays open; virtual TCP is its
own layer, described in [the TCP guide](/docs/netstack-tcp.md). The code lives in
`src/netstack.rs` and `src/netstack/`: pure, std only, with no clock reads, threads
or I/O.

## IPv4 codec

`Ipv4Config` names the inner address and the allowed source network (the peer's
`AllowedIPs`; `0.0.0.0/0` for the default route). `parse` accepts a packet only if:

- the version is 4 (IPv6 is its own explicit error), the header length is 20 to 32
  bytes and the bytes exist, and the total length fits the input and 1424 bytes;
- the header checksum verifies and TTL is nonzero;
- more-fragments and fragment offsets are zero, since there is no reassembly;
- the options are only End-of-Options or No-Operation padding, at most 12 bytes;
- the protocol is UDP (17), the destination is the configured address, and the
  source is not this host, unspecified, loopback, multicast, broadcast or class E
  and is inside the allowed network.

Bytes after the total length (transport padding) are discarded and never
delivered. The builder emits a 20-byte header with DF set, identification zero
(RFC 6864) and TTL 64, for an `mtu` between 576 and 1424 (the offline session
payload budget, asserted equal in a test). It fails with `TooLarge`; it never
fragments or truncates, and refuses unroutable endpoints.

## UDP codec

`udp::parse` requires the UDP length to equal the IP payload exactly, nonzero
ports, and a correct pseudo-header checksum. A zero checksum field means none was
computed, which RFC 768 allows over IPv4, so it is accepted; the builder always
computes one and sends a computed zero as `0xFFFF`. `max_payload(mtu)` is
`mtu - 28`, and `build` returns a complete inner packet of at most `mtu` bytes.

## Virtual UDP sockets

`UdpSockets` is keyed by local port. Every call takes the caller's captured
generation: a different one irreversibly revokes the table and drops all sockets
and queued datagrams, as does `revoke`. A `Handle` carries a serial, so a closed
handle stays dead even after its port is bound again. Ephemeral ports (49152 to
65535) are drawn from the injectable `Entropy` with at most 32 collision retries;
an entropy failure fails the bind and never picks a predictable port.

| Bound | Default | Exhaustion |
| --- | --- | --- |
| Sockets | 64 | `TableFull` |
| Queued datagrams per socket | 16 | `Delivery::QueueFull`, datagram dropped |
| Queued datagrams in total | 256 | `Delivery::QueueFull`, datagram dropped |

`connect` restricts a socket to one remote: other sources are `Filtered` before
they can occupy its queue. `ingest` parses and validates IPv4 then UDP, and
delivers; every rejection is reported as an explicit value, never a panic.

## DNS stub client

`DnsClient` asks only the configured inner resolver IPv4 addresses (one to three,
never unroutable) over virtual UDP, port 53, for A records. There is no host
resolver, search list, cache or TCP retry.

- `start` validates the name (ASCII letters, digits, `-` and `_`; labels up to 63
  bytes; 253 bytes in all; one trailing dot; no IDNA), draws the transaction id
  and a fresh source port from `Entropy`, and returns the first packet.
- Each query owns one connected socket. The client also checks that a reply comes
  from a configured resolver at port 53, that the id matches, QR is set, the
  opcode is zero and the question is exactly the one asked (case-insensitively).
  Anything else is dropped and counted by `rejected()`; a spoof never completes or
  fails a query.
- Names are read with strictly backward compression pointers (at most 16), so a
  loop is impossible; labels and records are length checked, reserved label types
  and names over 255 wire bytes are refused. At most 32 answer records are read.
- Only A records, and CNAMEs on the asked name's chain (at most 8 names), are used.
  An AAAA or any other record type makes the datagram invalid, and a record for an
  unrelated owner is ignored. Unspecified, loopback, multicast and class E answers
  are dropped, at most 8 addresses are kept and a TTL with the top bit set is zero.
- The answer TTL is the smallest used record TTL, capped by `max_ttl` (one hour by
  default). A truncated reply is reported as `Failure::Truncated`: the retry needs
  the virtual TCP of [its guide](/docs/netstack-tcp.md), which is not wired to DNS
  yet. NXDOMAIN, other server failures and an empty answer are explicit `Failure`
  values.
- At most 16 queries are pending (1 to 64 by configuration). Attempts are spaced
  by `retry_interval` (2 s by default), rotate through the resolvers and give up
  with `Failure::Timeout` after 3 attempts. `poll` returns packets to send, finished
  queries and the next deadline, so scripted time is fully deterministic.
- A generation change, a clock that moves backwards or `revoke` irreversibly
  refuses every later call. Revoke the socket table too: it owns the sockets.

## Evidence and limits

CI generates synthetic reference vectors with Python's standard library, as for the
[cookies](/docs/crypto-vectors.md): RFC 1071 checksums (anchored to the RFC's worked
example and a published IPv4 header), IPv4 and UDP packets including options,
padding, the zero-checksum rules and the 1424-byte budget, DNS queries and 27 DNS
replies with compression, CNAME chains, TTL rules, AAAA, loops, forward pointers,
reserved labels and overruns. The Rust tests also cover each header and port
field, every single-bit header flip, spoofed sources, ports, ids and questions,
queue and table exhaustion, generation and clock fences and scripted retry and
timeout. The generator and the Rust code are by the same authors from the same
RFCs, so the vectors catch implementation slips, not misreadings shared by both.
No independent stack or resolver is exercised.

## Remaining obligations

The driver that feeds decrypted session bytes in, encrypts what comes out, owns
the clock and generations and revokes every piece together is now
[the tunnel runtime](/docs/tunnel.md). Still missing: ICMP and path-MTU
handling; DNS over the virtual TCP; a DNS cache if wanted; the
transport, HTTP/TLS and torrent integration; profile controls; and the isolation
evidence listed in [the design](/docs/wireguard-design.md). Nothing here changes
user-visible behavior.
