# Performance measurements

The project includes a standard-library-only benchmark without a measurement
framework. CI can run it with the release example binary:

```sh
cargo run --release --offline --locked --example benchmark
cargo run --release --offline --locked --example benchmark -- examples/demo.mp4 10000
```

These commands describe the CI measurement procedure, not permission to run
local tests or lint. Development validation belongs in GitHub Actions.

The benchmark returns JSON with the iteration count, elapsed time, mean analysis
time, analyses per second, and SHA-1/SHA-256 throughput in MiB/s.
`std::hint::black_box` preserves measured work. Media is analyzed 32 times before
timing, so media results concern a **warm cache**. Hashes use a deterministic
1 MiB block, repeated 32 times each.

The release profile uses thin LTO and one code-generation unit. Results depend on
CPU, concurrent load, storage, and compiler. They do not describe public-torrent
throughput, Plex latency, cold disks, or every possible metadata size.

Container analysis skips audio/video blocks with `seek` instead of copying the
media into memory. A test constructs a sparse MP4 with an `mdat` block larger than
4 GiB and checks extraction of its final metadata. It verifies the read strategy,
not decoding performance.

## Historical 0.6.0 measurement — October 3, 2026

The following results were measured before the CI-only development policy was
adopted. They concern Mynou **0.6.0**, not a new measurement of 0.6.1. The release
example was built with Rust 1.99.0 in a shared Linux x86_64 environment reporting
two logical processors.

| Measurement | Result |
| --- | --- |
| Analyzed file | Included demo MP4, 125,671 bytes |
| Warm-cache analyses | 10,000 |
| Analysis elapsed time | 0.08982 s |
| Mean analysis time | 8.98 µs |
| Analyses per second | Approximately 111,333 |
| SHA-256, 32 × 1 MiB | 177.14 MiB/s |
| SHA-1, 32 × 1 MiB | 250.74 MiB/s |

This measures the demo and hashing functions, rather than end-to-end performance
of a real library. The short timing interval also exposes load variations; it
does not justify a general comparison with other software. No claim of perfect
code or universal performance follows from one environment.

The exact historical JSON is preserved in
[benchmark-results.json](benchmark-results.json). Current CI evidence is described
in [validation.md](validation.md).
