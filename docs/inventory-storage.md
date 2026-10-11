# Inventory storage foundation

The `mynou::inventory::Index` Rust API is the storage foundation for
[growing inventories](https://github.com/alecerf/mynou/issues/106). It is not
connected to the service yet. Existing request, series and requester limits
remain unchanged; there is no migration command or new CLI/API endpoint.

## Ownership and format

`Index::create(path)` requires a new directory whose parent already exists.
It never overwrites an existing directory. `Index::open(path, read_only)`
accepts this format only, creates nothing and changes no permissions. The API
holds an exclusive lock on `.lock` for its lifetime, including read-only use.
Creation sets directories to 0700 and files to 0600. Opening requires private
directories and regular private files, rejects symbolic links and hard-linked
state files, and checks an opened file against the inspected inode. This is
the existing private-directory trust model, not protection against an actor
who can rewrite the owner's directory while it is in use.

`identity.bin` holds a random 256-bit store identity and checksum. `tree.bin`
contains 4 KiB checksummed pages. Page zero holds the identity, sequence,
root, allocation counter and record count; remaining pages hold ordered keys
and either value references or child references. Nodes have at most 32 items;
child levels strictly decrease and child upper keys must match their parents.
Offsets/counts use checked arithmetic, supported offsets fit a signed 64-bit
file offset, and there is no configured total inventory cap.

Keys are 64-byte ordered binary values. Callers must define table/range
prefixes and identity encodings. Values have at most 1 MiB. An object is
addressed by its full key under `objects/XX/REMAINING_HEX`; reads do not list
the object directory. Its envelope binds the key, length and checksum, and
the tree records its content digest. Checksums detect corruption, not malicious
rewrites by somebody who can replace state and its checksums.

The API validates the root at open and each referenced page/value on access;
opening does not audit the entire inventory. Corruption on an untouched path
is reported when that path is read, never returned as an empty/absent result.

## Transaction and recovery

One transaction contains at most 1,024 distinct key changes, 4,096 dirty pages
and 64 MiB of WAL payload. These are operation bounds, not inventory bounds.
Primary and secondary keys can change together. Preparation verifies existing
records, stages tree changes in memory and validates the resulting key changes.

1. Create `transaction.tmp`, write the complete checksummed redo and synchronize
   the file. This unpublished staging artifact has never applied any data.
2. Rename it to `transaction.bin` and synchronize the store directory. Only
   after this publication is durable may any record or tree page change.
3. Write private temporary objects, synchronize them and rename them to their
   exact key paths; apply deletions. Synchronize every affected object directory
   and `objects`. Apply page images and the root, then synchronize the tree and
   store directory.
4. Remove the published WAL and synchronize the store directory before
   acknowledging or preparing another transaction.

Any error after staging begins blocks **all reads and writes** until reopen.
The caller must inspect recovered state rather than automatically retrying:
a failed commit response can mean a committed transaction. Validation failures
before staging leave the existing store usable and unchanged.

Writable open reads and validates the entire bounded published WAL before any
repair. Its store identity, old/new metadata, sequence, page images, actions
and resulting key references must agree. A valid redo may replace torn root,
page or object content only for the images/keys it covers. Links, wrong types
and public permissions remain errors. Complete corruption, truncated published
WALs, cross-store WALs and stale roots are errors, with no repair. Replaying a
valid transaction is idempotent. Only unpublished staging may be discarded.

Read-only open never repairs, creates, deletes or changes permissions. A pending
transaction or unpublished staging reports that writable recovery is required.
An interrupted initial creation is an explicit error, not a reason to initialize
over an existing directory. Legacy journal/snapshot directories are never
accepted as empty instances of this format.

## Pagination and bounds

Queries use an inclusive start, exclusive end and exclusive last-key cursor.
The tree prunes preceding subtrees; deletions prune empty branches and collapse
unary roots. There are no sibling links or empty-leaf scans. A page emits at
most 200 rows and 4 MiB of value bytes; a byte-limited page resumes after its
last **emitted** key. `total` counts the whole index, not just the requested
range. `next` is absent when no further row is found.

Each page reports the current sequence. Passing that sequence on later pages
rejects intervening mutations and asks the caller to restart. Omitting it gives
a live key scan: insertions before the cursor need not appear. This is not MVCC
or a retained snapshot.

Memory and work depend on tree height, the bounded transaction and the requested
page, rather than retaining all record identities in RAM. The implementation
keeps at most 4,096 staged nodes plus at most one extra split path during
preparation. Overlay validation may retain two copies of the completed staged
map. Action/value buffers, WAL encoding/decoding buffers and response values
are also bounded. The height bound is 63; page allocation checks the file-offset
representation. These are source-level bounds, not measured RSS or throughput.

Unreachable old page slots and interrupted temporary object files are not
reclaimed by this first increment. Repeated churn can therefore consume extra
disk space. Compaction/reclamation must be designed and verified before the
integration claims scalable routine operation. There is no automatic cleanup,
backup, downgrade or data migration in this API. A backup is a stopped-store
copy, including any recovery artifacts; a partial copy is not a backup.

## Remaining delivery

Issue #106 still needs recoverable legacy migration; series, episode and
numbering integration; job/history/requester and ownership indexes; paginated
management views; fair indexed scheduling; resource/backlog diagnostics; and
increasing-size evidence. Do not use this foundation as evidence that the
service's current inventory caps have been removed. Validation uses original
synthetic fixtures in GitHub Actions on the supported macOS target only.
