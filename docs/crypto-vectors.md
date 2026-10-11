# WireGuard primitive reference evidence

Issue #169 supplies primitives for the future transport in #111. It does not
implement a WireGuard session, profile, network stack or protected connector.
Existing TLS AEAD behavior remains covered by its RFC 8439 tests.

## Implementation provenance

The BLAKE2s implementation is independently authored from the equations and
normative constants in [RFC 7693 sections 2 and 3](https://www.rfc-editor.org/rfc/rfc7693).
It does not copy or translate the reference source appendices. Only sequential
hashing is supported. A streaming state holds one block and eight hash words;
output length is a hash parameter, including WireGuard's keyed 16-byte MAC.
A shortened 32-byte digest is a different operation and is not used as that MAC.

HMAC follows [RFC 2104](https://www.rfc-editor.org/rfc/rfc2104), with a 64-byte
BLAKE2s block and a 32-byte digest. HKDF follows the extract/expand construction
in [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869); its output is bounded by
255 digest blocks. These functions are distinct from BLAKE2s keyed hashing.

HChaCha20 and XChaCha20-Poly1305 follow
[draft-irtf-cfrg-xchacha-03](https://www.ietf.org/archive/id/draft-irtf-cfrg-xchacha-03.txt),
an Internet-Draft, not an RFC. HChaCha selects words 0..3 and 12..15 after the
existing ChaCha double rounds, without feed-forward. XChaCha uses the first
16 nonce bytes to derive its key and a 96-bit nonce consisting of four zero
bytes followed by the remaining eight bytes. The original RFC 8439 AEAD checks
the tag before decrypting and supplies the counter/output bounds.

Callers must choose nonces that never repeat under a key. The primitive does
not generate nonces or manage keys. None of this code has received an independent
cryptographic audit, guarantees optimizer-level constant-time execution, or
guarantees zeroization of secret material.

## Independent CI reference data

GitHub Actions generates synthetic JSON fixtures with
`.github/scripts/crypto_fixtures.py`, using only Python's standard-library
`hashlib.blake2s` and `hmac`. The generator does not import, call or inspect
Mynou's implementation. Python is CI tooling, never a product runtime helper.
The Rust tests require the fixture path supplied by CI, bound their input to
1 MiB, check its schema and exact matrix sizes, and never silently skip it.

- 144 BLAKE2s cases cover unkeyed/keyed hashes, keys up to 32 bytes, output
  sizes 1/16/20/32, empty/key-only input and block boundaries. Each result is
  also checked across seven streaming chunk sizes with intervening empty updates.
- 42 HMAC cases include empty keys/messages and keys shorter than, equal to,
  and longer than the block size.
- 96 HKDF cases cover empty salt/input, long salt normalization, partial and
  multiple digest blocks, zero output and the maximum 8160-byte output.
- Fixed public BLAKE2s digests anchor the reference; the RFC's `abc` value also
  anchors the generator. The HChaCha section 2.2.1 and XChaCha AEAD appendix
  A.3.1 vectors are fixed public expected data, not a same-code round trip.
- XChaCha tests reject modified ciphertext/tag bytes, every modified nonce
  byte, wrong associated data/key and short tags. Empty and partial-block
  tests exercise the inherited AEAD path. Counter overflow, invalid parameters
  and excessive HKDF output fail explicitly without huge allocations.

All fixture keys and messages are synthetic. No credentials or real network
traffic are required. Tests and validation run only in GitHub Actions, following
the repository protocol. This evidence covers primitives; independent WireGuard
interoperability and observation of protected socket/resolver attempts remain
separate delivery gates for #111.
