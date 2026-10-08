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
buffers and output are allocated. The initial 0.22.12 subset required consecutive recovery
exponents starting at zero. The proposed 0.22.26 increment selects a bounded
independent basis from available captured exponents, as described below.

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

## Multi-file memory recovery published in 0.22.13

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
review/publication state. Actions published v0.22.13 at 6085368588c8077af53d15bc4f07d752e670e445
in run 37648617664 with 846 Rust tests and seven immutable assets.

## Read-only protected-content verification published in 0.22.14

`mynou par2-verify FILE --root DIRECTORY` opens only recoverable protected names
under an explicitly selected existing root. It prints checked JSON with expected
and observed lengths, presence, local/global Main-order damaged-slice indices,
padded MD5/CRC32 and whole-file/first-16-KiB integrity. Clean content exits zero;
damage or missing files print the diagnosis and exit nonzero. Invalid format,
paths, input identity or bounds fail without a partial report. Empty-file presence
is explicit: missing zero-byte files are not silently declared present.

The library `Set::verify_files` / `verify_files_cancellable` uses the same exact-ID
`RecoveryInput` bindings as recovery. Caller order is irrelevant; unknown,
duplicate, omitted, nonrecoverable or oversized inputs fail. Immutable byte inputs
cannot establish filesystem presence (`present: null`); valid empty bytes can
verify an empty declared file. Reports follow Main order, excluding nonrecoverable
files. Whole-file disagreement fails content verification even if every slice
matches. Diagnosis can report all 256 damaged slices; the recovery erasure and
field-operation bounds do not imply that reported damage can be reconstructed.
No recovery arithmetic or parity correctness is certified.

Use `MultiRecoveryLimits` to tighten eight files, 16 MiB combined declared/read
bytes, 256 slices and 1 MiB slices. Verification scratch/report overhead must fit
`max_working_bytes`; the directory adapter additionally budgets its internally
captured input buffers and a conservative 12 MiB path/metadata/reread allowance.
Each path is limited to 8 KiB and 128 components. The captured Set is excluded,
as for memory recovery. Source hashing/readback and protected reads operate in
64 KiB chunks. The cancellable directory API checks between chunks and OS calls;
cancellation cannot interrupt a blocking OS operation.

The adapter rejects symbolic-link source/root/selected ancestors and nonregular
inputs, compares opened/path type and length (plus device/inode on Unix), rechecks
the original PAR2 source and re-reads protected handles against captured bytes.
Missing leaves/parents remain missing at final fencing. It creates no directories,
configuration, journals or files. Safe Rust std has no general directory-relative
open capability or atomic filesystem snapshot: hostile concurrent mutations may
still evade observations. `snapshot_guaranteed`, `parity_verified`,
`ownership_verified` and `repair_supported` remain false. Checksums authenticate
no owner and grant no repair, acquisition, import or library permission.

Original CI fixtures and final separate Security/QA establish actual delivery in
[Issue #13](https://github.com/alecerf/mynou/issues/13). Filesystem repair and
ownership-bound admission remain subsequent explicit work.

Actions published v0.22.14 at `7910156c6cce0f74a9ab530b289c3734750bc994`
in run 37668451522: all five jobs succeeded, 858 Rust tests passed and seven
immutable bot assets were published. Issue #13 comment 6045313189 records proof.

## Proposed private persistent recovery in 0.22.15

`usenet::par2_workspace::RecoveryWorkspace::create` takes a captured `queue::Owner`,
immutable `Set`/PAR2 source, exact `RecoveryInput` values, `MultiRecoveryLimits`
and a cooperative cancellation flag. Every file is reconstructed and fully
verified before any directory is created. Only a new directory under an existing
private parent is accepted. Output paths are internal File-ID names; original
protected names are metadata, never write destinations. Nothing is overwritten.

The workspace holds a standard exclusive `File` lock. Verified output files are
synchronized and read back before the separately tagged descriptor commits the
complete owner/source/Main-order inventory and all captured limits. The descriptor
is at most 16 KiB; an oversized descriptor is rejected before filesystem writes.
The caller retains borrowed immutable metadata/source, avoiding a metadata copy.
Reconstruction buffers are dropped before readback. Combined content, slice
scratch and 1 MiB persistence overhead must fit the captured working budget.
Existing eight-file/16 MiB/256-slice/eight-erasure/1 MiB-slice/32 MiB-working and
128 Mi field-operation ceilings remain; stricter policies stay immutable on reopen.

`open` requires the same owner, source, metadata and policy. `verified_files`
rechecks every output through original padded slice, full-file and prefix checks
and its retained SHA-256 before returning any private paths. Descriptor checks,
bounded exact directory inventory, regular-file/private-mode/single-link checks
and parent/root/opened-file identity fences detect corruption, replacement and
extra/missing content. All bytes remain unchanged on verification failure.
Cancellation checks at most 64 KiB read/write/hash chunks and before descriptor
publication; it cannot interrupt a blocking OS operation. Once the synchronized
descriptor commits, later cancellation cannot undo the publication.

The supported filesystem is Unix with a caller-controlled private parent.
Other platforms fail closed before creating a workspace. Kernel file locking
releases after worker death; there is no permanent boolean lock. Interrupted
staging without a complete descriptor stays private and unpublished, never
implicitly adopted. A synchronization error after atomic descriptor rename means
durability is uncertain and requires checked reopening, never a success claim.
Safe std paths do not offer an atomic hostile filesystem snapshot or general
directory-relative capabilities.

A captured owner identifier is not proof of current approval. This library API
grants no automatic queue activation, existing-file repair, overwrite, download,
media/import/Plex or library permission. Those gates remain a subsequent integration.
Original CI-only fixtures and separately leased Security/QA establish delivery in
[Issue #15](https://github.com/alecerf/mynou/issues/15).

## Available independent parity rows — proposed 0.22.26

Single-file, multi-file and owner-bound workspace recovery can now use nonzero
or nonconsecutive available recovery exponents. The reader retains unique
exponents0–65534 in ascending order. Recovery builds a small normalized basis
over the actual erased global Main/file/slice columns, skips dependent candidates,
and stops when it has enough independent rows. For example, rows0 and21845 are
dependent for erased columns0 and2; a later independent row can complete the
basis. Packet order never changes that selection.

The chosen exponents drive both inversion and parity residuals. A rank-deficient
set fails without output. A mathematically incorrect selected row fails the
existing final integrity checks; there is no combinatorial retry across alternative
bases and no certification of unused parity. Source identity and packet reread
binding, padded slice MD5/CRC32, prefix/full-file checks, immutable inputs and
cooperative cancellation remain required.

All existing hard bounds remain. In addition to residual/reconstruction and
inversion work, each examined candidate is conservatively precharged
`m*m + 2*m + 1` field operations for at most `m=8` erased slices, including
coefficients, elimination, normalization and inversion. Dependent candidates
consume that budget too. Selection uses a fixed eight-row basis; it completes
before parity buffers and reconstructed output are allocated. A tight captured
policy can reject this extra work; limits are never raised implicitly.

This extends the existing recovery APIs and private workspace. It does not merge
PAR2 volumes, repair existing files, grant queue/library permission or establish
hostile-filesystem snapshot guarantees. Original independent polynomial-oracle
fixtures cover shifted/nonconsecutive rows, rank deficiency, skipped dependencies,
global multi-file order, eight erasures, field-work bounds, corruption, cancellation
and checked private reopening. Actual CI and separate Security/QA evidence belong
to Issue #36 and its linked PR; this proposed scope is not publication evidence.
