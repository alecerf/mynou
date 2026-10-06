# Native Usenet stages

v0.22.0 provides original bounded NZB and yEnc format primitives. It passed all
five CI jobs in run 37434792979 with 639 Rust tests and seven published assets.
Native NNTP passed complete v0.22.1 CI with 652 Rust tests and seven assets.
The active v0.22.2 disk workspace increment needs its own complete CI. All code
uses Rust std only.
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
explicitly ephemeral; durable transfer management follows after the disk workspace
increment.
There is no exposed article-download API or automatic NNTP polling in this stage.

## Checked private workspaces in 0.22.2

`usenet::workspace::Workspace` creates a new private single-file workspace from
original NZB bytes, file index, immutable provider binding digest and explicit
positive file-size limit up to one TiB. Original NZB bytes must be retained by the
caller and supplied again on reopening. A new directory is required; existing
storage is never adopted by creation. Clean paths, 0700 directories, 0600 regular
files and an exclusive native process-owner lock protect the workspace.

A checked MYNOUW01 descriptor binds exact NZB byte identity, file index, provider,
part count and file-size cap. Each MYNOUP01 receipt binds that workspace and the
expected article identity to immutable verified yEnc metadata/data. Frames have
bounded header/data lengths and SHA-256 checks. Accepting a part requires the
exact original article ID and number from the NZB inventory. Receipt publication
uses exclusive private temporaries, file synchronization, atomic rename and
directory synchronization. Repeated identical parts are idempotent; conflicting
or unexpected existing receipts are preserved and rejected.

Reopening verifies immutable inputs, every retained receipt frame, article
binding, range/size/header constraints and part CRC before reuse. Missing receipts
remain missing; corruption, unsupported formats, public permissions, symlinks or
shared hardlinks fail closed. No receipt is considered verified merely because a
path or file size exists. Read-only opening requires existing checked storage and
never creates, repairs, assembles or publishes anything.

Complete assembly requires exactly numbered parts, consistent names/size/count,
contiguous coverage and the final whole-file CRC. It reads one bounded part at a
time and writes in 64 KiB chunks, updating CRC32 and SHA-256. The output may exceed
the 64 MiB in-memory assembler cap while remaining inside its captured file-size
limit. Decoded names select only basenames inside the workspace's private output
subdirectory; source/library files are never assembly destinations. Existing
foreign outputs are preserved and rejected.

After verifying/synchronizing the complete temporary output, the descriptor
records its name, length, CRC, SHA-256 and owned temporary identity as `prepared`.
Only then can it be atomically published, synchronized and marked `ready`.
Writable reopening verifies prepared temporary or published bytes and completes
that transition without reacquisition. Read-only reopening checks the same proof
and leaves the prepared state/files unchanged. Complete outputs are rechecked on
reopening. Uncertain persistence suppresses available output and requires reopening.

Public workspace reports expose binding/phase/part counts/readiness, without
subjects, article IDs, decoded names or provider details. The available file is a
previously verified private path; an importer must retain ordinary captured
quality/ownership gates and verify content immediately before use.

This is a library/storage increment. It does not yet start background downloads,
expose an article-download API, implement a transfer queue or admit library work.
Native queue/management, ordinary Engine acquisition and Newznab follow separately.
Original fixtures include restart, conflict preservation, private/link rejection,
CRC/frame corruption, prepared publication windows and a 65 MiB streamed file;
v0.22.2 passed complete CI with 663 Rust tests across 57 harnesses.

## Durable private staging in 0.22.3

Configure native downloads explicitly; omitting this section starts no queue:

```json
{
  "usenet": {
    "servers": [{"id": "primary", "host": "nntp.example.test", "tls": true,
      "username_env": "MYNOU_NNTP_USER", "password_env": "MYNOU_NNTP_PASSWORD"}],
    "downloads": {"enabled": true, "state_dir": "state/usenet", "max_active": 2,
      "max_attempts": 3, "max_file_bytes": 68719476736}
  }
}
```

Resolve state_dir relative to the configuration file. It must be separate from
job/torrent state, download data and library roots. Private directories, checked
MYNOUU01 queue snapshots, immutable MYNOUN01 source blobs and an exclusive process
owner protect storage. The queue retains at most 256 file transfers, 64 distinct
NZB sources, 64 MiB of source bytes and 32768 selected article references. Original
NZB bytes are captured once per source and reparsed/rehashed on restart. Selected
file indices, provider bindings, size limits and attempt budgets are immutable.
Changed provider settings require a new server ID; removed providers pause active
work without fallback. Disabled downloads perform no acquisition or storage
initialization.

```sh
mynou usenet-enqueue original.nzb --server primary --file-index 0 --config mynou.json
mynou usenet-enqueue original.nzb --server primary --file-index 0 \
  --apply --plan-id REVIEWED_ID --config mynou.json
mynou usenet-queue --config mynou.json
mynou usenet-control TRANSFER_ID --action pause --config mynou.json
mynou usenet-control TRANSFER_ID --action pause --apply --plan-id REVIEWED_ID \
  --config mynou.json
```

The protected API provides GET/POST /api/usenet/queue and
POST /api/usenet/queue/TRANSFER_ID/control. Enqueue JSON supplies UTF-8 `nzb` text,
`server_id`, a zero-based `file_index`, and optional `apply`/`plan_id`; controls
supply `action` (pause, resume, cancel or retry), apply and plan_id. Existing
one-MiB HTTP request-body bounds also apply to NZB JSON. The library/parser accepts
up to eight MiB of NZB bytes; this does not enlarge the HTTP transport bound.
Offline CLI reviews never create or repair storage; application requires the
running writable service. Guards bind exact source/action, the complete queue
snapshot and a fresh service identity. Browser control reviews additionally bind
session/transfer/action/guard, expire after ten minutes and require origin/CSRF.
The Usenet page exposes progress and controls; enqueue uses CLI/API in this stage.

Each selected part consumes a persisted reservation before NNTP BODY. Missing or
invalid articles use bounded backoff and at most the captured 1–10 attempts per
part; retry/resume never reset the budget. Verified receipts are reused after
restart, including publication preceding a queue update. Paused/cancelled/stopped
results cannot publish queue completion. Live slots remain occupied until their
workers return, including after cancellation. At most two operations share the
queue; NNTP retains one operation per provider and its absolute timeout. Shutdown
joins workers; synchronous standard-library DNS still cannot be interrupted.

Complete file coverage, CRC and checked output proofs precede the durable complete
state. Private file access rechecks receipts and output immediately. Public
reports contain IDs, states, counts and fixed errors, without raw NZB subjects,
article IDs, decoded names, paths or provider credentials. Cancel preserves private
bytes and cannot implicitly revive a transfer. Corruption/unsupported storage and
uncertain durability fail closed. Incomplete workspace construction is preserved
and rejected; there is no implicit cleanup of unknown data. File-size caps are
per transfer, not a global disk-space quota. Peak article/part buffers follow
configured NNTP and decoded-part bounds; the worker count bounds concurrent use.

This queue stages explicitly selected raw yEnc files. It does not create requester
or canonical library jobs, fetch NZB URLs, search Newznab, extract archives or
repair PAR2. Ordinary Engine admission and native Newznab follow, preserving
quality, numbering, requester approval/quota, ownership, import and Plex gates.
The 0.22.3 source requires its own complete CI publication.
