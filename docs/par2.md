# Native PAR2 inspection and bounded recovery

`mynou par2-inspect FILE` reads one regular file containing a complete,
contiguous PAR2 core packet set and prints its checked metadata as JSON. It
does not open the described media files or create configuration, journals or
output files. Recovery bytes are hashed in bounded chunks rather than retained.

Supported core packets are Main, File Description, Input File Slice Checksum,
Recovery Slice and Creator. The reader verifies each packet MD5, the Main
recovery-set identifier, file identifiers derived from unpadded names, exact
metadata joins, checksum counts and recovery lengths. Identical duplicate packets
are accepted; conflicting duplicates, orphan metadata and mixed sets are rejected.
Main order is preserved. Offsets and SHA-256 digests describe the captured source
and recovery slices; they do not verify the contents of the described files.

MD5 is implemented originally from RFC 1321 solely for format compatibility.
It is not authentication and never grants permission to download, repair, import
or publish media. Packet integrity does not establish content or parity correctness.
The JSON explicitly reports `content_verified: false` and `repair_supported: false`.

This strict foundation requires a complete Main/Creator/description set in one
source and checksums for each recoverable file. Names and creator text are bounded
printable ASCII with exact zero padding; file names must pass portable relative
path checks and cannot alias by case or form file/ancestor conflicts. Unknown
standard packet types are rejected. Nonstandard packets are checksum-verified,
streamed within metadata bounds and counted as ignored. Unicode names, optional
standard semantics, damaged-packet resynchronization and volume merging are not
supported by this increment.

Default hard bounds are 256 MiB of source, 4,096 packets, 1,024 files, 32,768
input checksum slices, 1 MiB per slice, 8 MiB metadata and 64 MiB recovery bytes.
A declared file is limited to 4 GiB and total declared files to 64 GiB. Applications
can supply smaller validated `par2::Limits` and a cancellation flag through the
library API. Cancellation is checked between synchronous reads; it cannot interrupt
an arbitrary caller-provided blocking reader. Input length changes fail closed.
The file API rejects symbolic-link inputs and, on Unix, checks opened device/inode
identity against the captured metadata. Inspection is read-only, not a snapshot
guarantee for a file concurrently modified in place.

Original CI-only fixtures cover RFC vectors, packet corruption, semantic joins,
padding/identity, duplicates, paths, bounds, cancellation, streaming recovery
payloads and read-only CLI behavior. Actual verification is recorded in the linked
PR and GitHub Actions; this document does not claim unpublished test results.

The 0.22.12 library increment adds original GF(2^16) recovery with independently
checked arithmetic and parity fixtures. `par2::gf16` supplies bounded multiplication,
inversion, powers, coefficient assignment and caller-owned scaled buffers.
It follows the authors' [PAR2 2.0 Recovery Slice specification](https://parchive.sourceforge.net/docs/specifications/parity-volume-spec/article-spec.html),
with polynomial 0x1100b and primitive input constants in Main/file/slice order.
Tables are generated once using std `OnceLock`; no third-party data or code is
embedded. Scalar buffers contain little-endian words; each operation is limited
to 1 MiB and checks cancellation between 64 KiB chunks. Mid-operation cancellation
can leave a partial caller-owned sum, which must be discarded. Arithmetic does
not verify packet identity, parity, protected content or repaired output. The
existing inspector continues to report `repair_supported: false`: inspection
does not recover or verify described files. Fixed specification constants and an
independently authored polynomial long-division oracle run only in CI; no results
are inferred.

The library adds `Set::recover_single` and its cancellable
variant. They accept the captured PAR2 source and immutable caller-provided
protected bytes, and return a caller-owned `Vec<u8>` only after verification.
Short input represents a truncated prefix; empty input represents a missing
file. Exactly one described recoverable file is supported, with at most 256
slices, eight damaged or missing slices, 16 MiB of content and 1 MiB per slice.
`RecoveryLimits` may tighten the 32 MiB additional-memory and 128 Mi field
operation ceilings. Captured metadata and caller-owned inputs are excluded from
the additional-memory budget. Memory/work bounds are checked before parity
buffers and output are allocated. Missing slices require consecutive recovery
exponents starting at zero; unsupported row selections fail closed.

Recovery rechecks source length/SHA-256 before and after the operation, and binds
each used packet header, payload SHA-256 and packet MD5 to the captured metadata.
Zero-padded slice MD5/CRC32, full-file MD5 and first-16-KiB MD5 are verified before
return. Failed or cancelled partial output is discarded. Checksums do not prove
authenticity, and these checks cannot guarantee a mutable reader remains stable
after return. Cancellation is checked between bounded synchronous read, hash
and field-operation chunks, not within a blocking caller `Read`.

No CLI repair, output path selection, filesystem write, overwrite, ownership
journal or library admission is added. [Issue #6](https://github.com/alecerf/mynou/issues/6)
and [PR #8](https://github.com/alecerf/mynou/pull/8) contain the authoritative
CI, logically separate Security/QA review and publication evidence. A proposed
version in source does not establish publication.

Call the library with the same PAR2 source captured during inspection and the
protected file bytes; successful recovery does not modify either input:

```rust
use mynou::par2::{Limits, RecoveryLimits, Set};
use std::io::Cursor;

fn recover_bytes(par2_bytes: &[u8], protected_bytes: &[u8]) -> mynou::Result<Vec<u8>> {
    let mut source = Cursor::new(par2_bytes);
    let captured = Set::read(&mut source, Limits::default())?;
    captured.recover_single(&mut source, protected_bytes, RecoveryLimits::default())
}
```

Ownership-bound multi-file recovery,
volume merging and library admission require subsequent explicit work. Existing
Usenet ownership, requester, destination, media-analysis and Plex gates remain
required for any future admission.

## Proposed multi-file memory recovery in 0.22.13

`Set::recover_files` and `recover_files_cancellable` accept one `RecoveryInput`
for every captured recoverable File ID. Construct each input with
`RecoveryInput::new(*description.id(), protected_bytes)`. Caller order is irrelevant;
empty bytes explicitly represent a missing file, and short bytes a truncated
prefix. Unknown, duplicate, nonrecoverable, omitted or oversized inputs fail.
Nonrecoverable descriptions remain captured metadata and do not consume recovery
coefficients. Empty recoverable files require an input but contribute no slices.

The shared engine uses a single global slice map in captured Main/file/slice order.
It returns `RecoveredFile` values in Main order only after every slice and every
file passes integrity checks. `id()` identifies the captured File ID; `bytes()`
borrows the output and `into_bytes()` transfers its caller-owned Vec. The existing
single-file API keeps its exactly-one-described-file subset and delegates to this
same engine.

`MultiRecoveryLimits` defaults to eight recoverable files and the existing
`RecoveryLimits`. Its `recovery.max_file_bytes` limits **combined** described
content to 16 MiB; slice count, erasures, working memory and field operations are
likewise aggregate: 256 slices, eight erasures, 1 MiB slices, 32 MiB additional
memory and 134217728 field operations. Applications may tighten these bounds.
All output buffers, scratch, residuals and bounded mapping/matrix/table overhead
are covered; captured Set metadata and caller-owned inputs remain excluded.
Source/packet reread binding, padded-slice/full-file integrity, cooperative
cancellation and discarded partial outputs apply to the entire operation.

This is original safe Rust std-only memory recovery, not ownership authorization,
filesystem repair, media admission or volume merging. Inspection still reports
`content_verified: false` and `repair_supported: false`. Original multi-file
fixtures and all previous tests run only in Actions. [Issue #9](https://github.com/alecerf/mynou/issues/9)
and [PR #10](https://github.com/alecerf/mynou/pull/10) retain actual CI and final
review/publication state; no result is inferred from this proposed source.
