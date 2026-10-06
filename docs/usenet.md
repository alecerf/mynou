# Native Usenet stages

v0.22.0 provides original bounded NZB and yEnc format primitives. It passed all
five CI jobs in run 37434792979 with 639 Rust tests and seven published assets.
The active v0.22.1 native NNTP increment needs its own complete CI. All code uses
Rust std only.
These primitives establish format and accidental-corruption checks; full Usenet
search, NNTP acquisition, durable transfer management and recovery follow in
separate releases. No external decoder, downloader, archive tool or repair helper
is invoked.

## NZB inspection

```sh
mynou nzb-inspect original.nzb
```

Inspection reads a bounded regular local file without configuration, network
access, journal changes or payload downloads. The JSON report contains the
SHA-256 of the exact NZB bytes, file and segment counts, advertised byte total
and `content_verified: false`. Subjects, posters, groups and message IDs remain
private. A metadata digest authenticates those bytes alone; it does not establish
article contents or a film's identity.

The original XML parser is shared with RSS/Torznab. Input must be UTF-8 within
8 MiB, with at most 100,000 nodes, depth 64 and 64 attributes per element.
Comments, CDATA and standard/numeric entities are supported; DTDs, external
entities and internal processing instructions are rejected without fetching
anything. NZB accepts an unprefixed root and the optional standard
`http://www.newzbin.com/DTD/2003/nzb` default namespace. Unknown structures and
attributes fail instead of being inferred. A bounded optional head contains
meta entries; file entries contain exactly groups and segments.

At most 1,024 files and 32,768 total articles are accepted. Each file has 1–32
distinct ASCII newsgroups and contiguous, distinct segment numbers starting at
one. Message IDs must be bounded bare ASCII `local@domain` values, without angle
brackets, whitespace, quotes, backslashes or command delimiters. A message ID
cannot be reused across files. Advertised article sizes are positive and at
most 16 MiB each. These are untrusted metadata claims.

## yEnc and complete-file integrity

`usenet::yenc::decode` validates one article and returns an immutable verified
part. Headers are bounded UTF-8, payload remains binary, and line endings must be
CRLF. Header fields are explicit and distinct. A bounded preamble is allowed;
trailing payload or a second encoded block is rejected. Filenames are plain
bounded basenames, with no traversal, separators, stream suffixes, controls or
ambiguous trailing characters. Single parts require a matching whole-file CRC32.
Multipart articles require explicit part/total, inclusive byte range, matching
footer identity and matching part CRC32. The final part also requires the
whole-file CRC; earlier parts cannot claim it.

Decoded parts are limited to 16 MiB, with at most 32,768 declared parts and
one TiB of declared file size. The original CRC32 uses a constant lookup table
and supports incremental checks. CRC32 detects accidental corruption; it is not
cryptographic authentication and does not prove semantic media identity.

`usenet::yenc::assemble` accepts out-of-order verified parts, sorts by part number
and requires one consistent filename, total size and part count, exactly one of
every numbered part, exact contiguous byte coverage and the final whole-file
CRC. Overlaps, gaps, missing or conflicting parts fail. Its returned immutable
file contains only successfully verified bytes. This initial in-memory assembly
is limited to 64 MiB. Larger disk-backed assembly, verified restart recovery and
controlled import remain subsequent stages.

UUEncode, Base64/MIME article decoding, RAR/ZIP extraction and PAR2 repair are
outside this release's explicit format contract. Durable acquisition and Newznab
search are not yet connected. Original synthetic format fixtures cover corruption,
structure limits, escaped bytes, multipart ordering/coverage, CRC differences,
unsafe paths and the read-only CLI. Validation runs only in GitHub Actions.

## Native NNTP transport and guarded probes in 0.22.1

Configure up to eight explicit providers, with unique bounded lowercase IDs:

```json
{
  "usenet": {
    "servers": [
      {
        "id": "primary",
        "host": "nntp.example.test",
        "port": 563,
        "tls": true,
        "username_env": "MYNOU_NNTP_USERNAME",
        "password_env": "MYNOU_NNTP_PASSWORD",
        "timeout_ms": 10000,
        "max_article_bytes": 16777216
      }
    ]
  }
}
```

Providers default to verified implicit TLS on port 563. Cleartext is accepted
only on literal loopback addresses for original fixtures, with port 119 by
default. There is no STARTTLS mode, plaintext fallback or posting. Hostnames and
settings are strictly bounded. Username/password environment names must be set
together or both omitted; missing/empty/invalid actual credentials fail before a
connection. Values retain bytes without trimming, are ASCII within 4096 bytes,
and reject controls; usernames additionally reject spaces.

One operation has an absolute 100–30000 ms budget, covering connect, verified
TLS, greeting, AUTHINFO and body reading. Synchronous DNS can exceed that budget;
late resolution is rejected before connecting. Greeting must be 200 or 201;
AUTHINFO USER accepts 281 directly or 381 followed by PASS and 281. Any failure
closes without authentication fallback. Status lines are bounded ASCII within
512 bytes; raw provider messages never become errors or reports.

`usenet::nntp::body` reads one explicitly addressed bare message ID using BODY.
It requires 222 with that exact echoed identity and a numeric article number.
Article lines are limited to 65536 bytes, CRLF endings are required, leading
dots are unstuffed and the terminal dot is mandatory. Truncation, invalid dots,
NUL bytes, wrong identities and oversized bodies fail. Body transport alone does
not verify yEnc CRC or create library work; the caller must still run integrity
and admission gates. A provider allows one active native operation; another
fails busy without duplicate connections. Health is shared through configuration
clones and records bounded transport counters/fixed errors in memory alone.

Default body limit is 16 MiB, configurable up to the yEnc worst-case bounded
encoded article limit (four times its 16 MiB decoded-part cap, plus headers and
preamble). The yEnc encoded-input bound now includes CRLF/escaping overhead even
with the smallest valid line width; decoded and assembly limits stay unchanged.

```sh
mynou usenet --config mynou.json
mynou usenet-probe primary --config mynou.json
mynou usenet-probe primary --apply --plan-id REVIEWED_ID --config mynou.json
```

Protected routes are `GET /api/usenet` and
`POST /api/usenet/SERVER_ID/probe` with optional apply and plan_id. The browser
**Usenet** page reviews the same probe. Preview does not connect or write.
Application requires the running service and a guard binding immutable provider
settings, a fresh service identity and its attempt count. Attempts invalidate
older reviews, including failed probes; restart creates a new identity and stale
guards fail. Guards contain no endpoint, credential name/value or raw status.
Browser reviews also bind session/provider/guard, expire after ten minutes and
require origin/CSRF checks.

A probe checks greeting/authentication and QUIT (205), never BODY or another
article command. It cannot create a download or library job. Probe health is
explicitly ephemeral; durable transfer management/recovery follows in 0.22.2.
There is no exposed article-download API or automatic NNTP polling in this stage.
