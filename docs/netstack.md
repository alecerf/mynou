# Offline inner IPv4/IPv6/UDP codecs, virtual UDP sockets and DNS stub client

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

`IpConfig` names the inner IPv4 address and the allowed source network (the peer's
`AllowedIPs`; `0.0.0.0/0` for the default route), and optionally the same for
IPv6 (see [IPv6 codec](#ipv6-codec)). `parse` accepts a packet only if:

- the version is 4 (IPv6 is its own explicit error unless the stack has an IPv6
  address, see below), the header length is 20 to 32
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

## IPv6 codec

[#372](https://github.com/alecerf/mynou/issues/372) adds IPv6 beside IPv4.
`IpConfig::with_ipv6` names the inner IPv6 address (never unspecified, loopback,
multicast, link-local or IPv4-mapped/-compatible) and the allowed IPv6 source
network (`::/0` for the default route). Without it, every IPv6 packet stays a
`PacketError::Ipv6`. With it, a packet whose version is 6 is accepted only if:

- the 40-byte fixed header exists and the payload length fits the input and the
  1424-byte budget; bytes after it are discarded like IPv4 padding;
- the next header is the expected transport. There is no extension header
  processing: a fragment header (44) is `Fragment`, hop-by-hop, routing,
  destination options, AH, ESP, mobility, HIP, shim6 and the experimental values
  are `Extension`, anything else is `Protocol`. Nothing is reassembled;
- the hop limit is nonzero; traffic class and flow label are ignored;
- the destination is our IPv6 address, and the source is not this host,
  unspecified, loopback, multicast, link-local or IPv4-mapped/-compatible (so an
  IPv4 address cannot bypass the IPv4 rules) and is inside the allowed network.

The builder emits the fixed header only, with traffic class and flow label zero
and hop limit 64, for an `mtu` of at least 1280 (RFC 8200's minimum link MTU), so
an IPv6 stack needs a configured MTU of 1280 or more. It never fragments: an IPv6
host may not, and routers never do. `IpConfig::source` gives the local address of
a remote's family; a family the stack lacks has none, and sending to it fails.

## UDP codec

`udp::parse` requires the UDP length to equal the IP payload exactly, nonzero
ports, and a correct pseudo-header checksum (the IPv4 one, or the IPv6 one of
RFC 8200 section 8.1). A zero checksum field means none was computed, which
RFC 768 allows over IPv4, so it is accepted there; over IPv6 the checksum is
mandatory and a zero field is a checksum error. The builder always computes one
and sends a computed zero as `0xFFFF`. `max_payload(mtu)` is `mtu - 28` over
IPv4; IPv6 carries `mtu - 48`. `build` returns a complete inner packet of at most
`mtu` bytes in the family of its addresses.

## Virtual UDP sockets

`UdpSockets` is keyed by local port, and one socket serves both families: the
caller passes the local address of the remote's family. Every call takes the caller's captured
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

`DnsClient` asks only the configured inner resolver addresses (one to three,
never unroutable; IPv6 ones only when the stack has an IPv6 address) over virtual
UDP or UDP over IPv6, port 53, for A records (`start`) or AAAA records
(`start_record`). There is no host resolver, search list or cache. A validated
truncated UDP reply retries over the shared virtual TCP stack, to the same
resolver and port 53.

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
- Only records of the asked type, and CNAMEs on the asked name's chain (at most 8
  names), are used. An AAAA record answering an A question, an A record answering
  an AAAA one, or any other record type makes the datagram invalid, and a record
  for an unrelated owner is ignored. The question must repeat the asked type and
  class IN. Unspecified, loopback, multicast and class E IPv4 answers, and
  unspecified, loopback, multicast, link-local and IPv4-mapped/-compatible IPv6
  answers, are dropped; at most 8 addresses are kept and a TTL with the top bit
  set is zero.
- The answer TTL is the smallest used record TTL, capped by `max_ttl` (one hour by
  default). NXDOMAIN, other server failures and an empty answer are explicit
  `Failure` values. The same parser validates both transports, with a 4096-byte
  message ceiling and the same ID, question, record and compression checks.
- At most 16 queries are pending (1 to 64 by configuration). Attempts are spaced
  by `retry_interval` (2 s by default), rotate through the resolvers and give up
  with `Failure::Timeout` after 3 attempts. `poll` returns packets to send, finished
  queries and the next deadline, so scripted time is fully deterministic.
- Validated ICMP unreachable advice for a query's last datagram ends only that
  attempt: the next `poll` sends the next attempt at once, to the next resolver,
  within the same attempt budget. If it answers the final attempt, the query
  fails with `Failure::Unreachable(code)` instead of waiting for `Timeout`.
- TCP uses a two-byte network-order length prefix, accepts split prefixes and
  bodies, and rejects lengths above 4096 before reading a body or allocating from
  the advertised length.
  It retains one fixed 4098-byte framing buffer per TCP query; at most one
  response and two bounded reads are processed per poll. Invalid IDs/questions
  and malformed frames are discarded and counted, preserving the pending query.
  Zero-length frames are discarded; EOF before a complete response fails with
  `Aborted`; an excessive length fails with `TooLarge`. TC over TCP fails with
  `Truncated`, without a loop.
- The flow's own outcome keeps its meaning: an RST from the resolver, to the SYN
  or later, fails with `Refused`; validated ICMP unreachable advice for the flow
  fails with `Unreachable(code)`, as over UDP; exhausted TCP retransmissions fail
  with `Timeout`. `Aborted` is kept for local faults and early EOF.
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
Unsupported messages, echo, unmatched or unsolicited advice are dropped, as are
messages with nonzero unused bytes 4–5, including RFC 4884 extension lengths.

With an IPv6 address, ICMPv6 (next header 58) is accepted the same way: the
outer IPv6 header passes the data checks and the checksum covers the IPv6
pseudo-header (RFC 4443). Destination unreachable (type 1) with codes 0–6 and an
all-zero unused field, and packet too big (type 2, code 0), must quote our
canonical IPv6 header (version 6, traffic class and flow label zero), our source,
a permitted remote and eight transport bytes. Unreachable codes are translated to
the closest ICMPv4 ones, so callers see one meaning: no route and beyond scope
become 0, address unreachable 1, port unreachable 3, administratively prohibited,
failed policy and reject route 13; packet too big becomes code 4 with its MTU.
An echo request (type 128) for our address gets one echo reply with the same
identifier, sequence and data (RFC 4443 section 4.2); nothing else is answered.

Connected UDP sockets match the remote tuple, source address, IP length and
all eight bytes of their **last sent** UDP header, including its checksum. Each
socket retains one such fingerprint, one MTU and one pending unreachable code;
there is no destination cache or growing error queue. An unreachable is consumed
once as `SocketError::Unreachable(code)` by `recv`. Closing the socket or
connecting it to a different remote resets this state; connecting again to the
same remote keeps it, including a reduced MTU. Unconnected UDP sockets ignore
ICMP; older datagrams are deliberately ineligible. A fingerprint does not authenticate the router and is not cryptographic
protection against an observer or an attacker who guesses the packet correctly.

Fragmentation-needed (code 4) and packet too big can only lower the connected
socket's MTU when the quoted datagram exceeded the proposed MTU. The floor is 576
bytes over IPv4 and 1280 over IPv6 (RFC 8201: a smaller value is raised to it). Zero
legacy MTUs, nonreductions and irrelevant small-packet quotes are ignored;
subsequent oversized sends return `Build(TooLarge)`. Advice consumes the UDP
fingerprint so replay cannot keep affecting it. TCP validation and resegmentation
are described in [the TCP guide](/docs/netstack-tcp.md).

**Decision:** refuse fragments instead of reassembling them. Every emitted IP
packet sets DF and every incoming offset/MF combination is rejected, without
retaining fragment bytes. This removes overlap, reassembly timeout and exhaustion
state; bounded TCP resegmentation and explicit UDP size errors cover the chosen
576–1424-byte path budget. Paths below that floor and UDP requiring fragmentation
remain unsupported. IPv6 fragment headers are rejected likewise. Without an IPv6
address, IPv6 is explicitly rejected; no IPv6 path outside the tunnel exists.

## Evidence and limits

CI generates synthetic reference vectors with Python's standard library, as for the
[cookies](/docs/crypto-vectors.md): RFC 1071 checksums (anchored to the RFC's worked
example and a published IPv4 header), IPv4 and UDP packets including options,
padding, the zero-checksum rules and the 1424-byte budget, DNS queries and 27 DNS
replies with compression, CNAME chains, TTL rules, AAAA, loops, forward pointers,
reserved labels and overruns. For IPv6 it adds UDP and TCP packets over the
RFC 8200 pseudo-header (including the largest payloads), an ICMPv6 echo request
and its expected reply, and an AAAA response, plus an A record answering an AAAA
question. The Rust tests also cover each header and port
field, every single-bit header flip, spoofed sources, ports, ids and questions,
queue and table exhaustion, generation and clock fences and scripted retry and
timeout. The generator and the Rust code are by the same authors from the same
RFCs, so the vectors catch implementation slips, not misreadings shared by both.
Additional deterministic cases cover ICMP header/quote truncation and corruption,
forged tuples and sequences, stale advice, the MTU floor and resegmented delivery,
DNS failover on ICMP unreachable,
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
