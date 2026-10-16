# Offline inner IPv4/UDP codec, virtual UDP sockets and DNS stub client

[#192](https://github.com/alecerf/mynou/issues/192) adds the first userspace IP
stack layer described in [the design](/docs/wireguard-design.md). It only sees
inner IP datagrams: `Session::receive` of [the transport](/docs/wireguard-transport.md)
returns authenticated padded bytes, and this layer is what must validate them
before any use. There is no host socket, engine, profile or torrent
caller in this layer, so **this is not routing, host DNS or VPN protection, and it is not
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
- the reserved flag, more-fragments and fragment offsets are zero; there is no reassembly;
- the options are only End-of-Options or No-Operation padding, at most 12 bytes;
- the UDP entry point requires protocol 17 (TCP and ICMP have separate entry points), the destination is the configured address, and the
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
resolver, search list or cache. A validated truncated UDP reply retries over the
shared virtual TCP stack, to the same resolver and port 53.

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
  default). NXDOMAIN, other server failures and an empty answer are explicit
  `Failure` values. The same parser validates both transports, with a 4096-byte
  message ceiling and the same ID, question, record and compression checks.
- At most 16 queries are pending (1 to 64 by configuration). Attempts are spaced
  by `retry_interval` (2 s by default), rotate through the resolvers and give up
  with `Failure::Timeout` after 3 attempts. `poll` returns packets to send, finished
  queries and the next deadline, so scripted time is fully deterministic.
- TCP uses a two-byte network-order length prefix, accepts split prefixes and
  bodies, and rejects lengths above 4096 before reading a body or allocating from
  the advertised length.
  It retains one fixed 4098-byte framing buffer per TCP query; at most one
  response and two bounded reads are processed per poll. Invalid IDs/questions
  and malformed frames are discarded and counted, preserving the pending query.
  Zero-length frames are discarded; EOF/reset fails with `Aborted`; an excessive
  length fails with `TooLarge`. TC over TCP fails with `Truncated`, without a loop.
- Each fallback uses one flow from the shared `TcpConfig::flows` limit and keeps
  its UDP socket until completion. The existing pending-query cap also bounds
  framing state (at most 64 buffers), and a full TCP table reports `Aborted`.
  Fallback has one fixed deadline of `retry_interval * attempts` from the TC reply;
  partial bytes and malformed frames never extend it. There is no TCP resolver
  rotation or connection pooling. Completion, timeout and cancellation release
  both transports. Poll DNS before TCP; `poll` and `cancel` take the caller's TCP
  stack as well as its UDP table.
- A generation change, a clock that moves backwards or `revoke` irreversibly
  refuses every later call. Revoke the socket table and TCP stack too: the caller
  owns them and must fence all three together.

## ICMP advice and fragment policy

`ingest_icmp` accepts only ICMPv4 destination-unreachable (type 3, codes 0–15).
The outer IPv4 header passes the same source-network, destination, checksum,
length and fragment checks as data. The complete ICMP checksum must verify.
The embedded quote must contain our canonical option-free, DF-set header and
at least eight transport bytes, with a valid quoted IP checksum, our local
source and a permitted unicast remote. No allocation uses the quoted IP length.
Unsupported messages, echo, unmatched or unsolicited advice are dropped.

Connected UDP sockets match the remote tuple, source address, IP length and
all eight bytes of their **last sent** UDP header, including its checksum. Each
socket retains one such fingerprint, one MTU and one pending unreachable code;
there is no destination cache or growing error queue. An unreachable is consumed
once as `SocketError::Unreachable(code)` by `recv`. Closing/reconnecting resets
this state. Unconnected UDP sockets ignore ICMP; older datagrams are deliberately
ineligible. A fingerprint does not authenticate the router and is not cryptographic
protection against an observer or an attacker who guesses the packet correctly.

Fragmentation-needed (code 4) can only lower the connected socket's MTU when
the quoted datagram exceeded the proposed MTU. The floor is 576 bytes. Zero
legacy MTUs, nonreductions and irrelevant small-packet quotes are ignored;
subsequent oversized sends return `Build(TooLarge)`. Advice consumes the UDP
fingerprint so replay cannot keep affecting it. TCP validation and resegmentation
are described in [the TCP guide](/docs/netstack-tcp.md).

**Decision:** refuse fragments instead of reassembling them. Every emitted IP
packet sets DF and every incoming offset/MF combination is rejected, without
retaining fragment bytes. This removes overlap, reassembly timeout and exhaustion
state; bounded TCP resegmentation and explicit UDP size errors cover the chosen
576–1424-byte path budget. Paths below that floor and UDP requiring fragmentation
remain unsupported. IPv6 is explicitly rejected; no IPv6 fallback is enabled.

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
Additional deterministic cases cover ICMP header/quote truncation and corruption,
forged tuples and sequences, stale advice, the MTU floor and resegmented delivery,
all 65,536 IPv4 flag/offset patterns, split TCP DNS framing, invalid frames,
oversize prefixes, timeout, EOF, cancellation and shared-table exhaustion.
An encrypted loopback tunnel test exercises truncated UDP DNS through virtual TCP.
No independent stack or resolver is exercised.

## Remaining obligations

The driver that feeds decrypted session bytes in, encrypts what comes out, owns
the clock and generations and revokes every piece together is now
[the tunnel runtime](/docs/tunnel.md), which dispatches validated ICMP advice too.
Still missing: a DNS cache if wanted; the
transport, HTTP/TLS and torrent integration; profile controls; and the isolation
evidence listed in [the design](/docs/wireguard-design.md). Nothing here changes
user-visible behavior.
