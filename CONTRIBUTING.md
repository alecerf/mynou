# Contributing

Thanks for helping with Mynou. Everyone taking part follows the
[code of conduct](CODE_OF_CONDUCT.md).

## Start with an issue

Open an [issue form](https://github.com/alecerf/mynou/issues/new/choose):
**Something is broken** for a bug, **Add or improve something** for an idea.
Say what happens or what should become better; engineering details are
optional. Leave out tokens, private paths and media names.

Found a vulnerability? Do not open a public issue: follow
[SECURITY.md](SECURITY.md).

Opening issues and pull requests can be limited to collaborators. If GitHub
does not let you open one, the project is not taking outside contributions at
the moment; forking is always fine (see [LICENSE.md](LICENSE.md)).

## Ground rules for code

- **Rust 1.99.1.** `rust-toolchain.toml` pins it with Clippy and rustfmt, so
  rustup selects it for you.
- **Standard library only.** Zero Cargo dependencies, no FFI and no external
  program at runtime. CI fails if the dependency graph holds anything but
  `mynou`.
- **`unsafe` only where the standard library offers no safe way**, such as
  processor intrinsics. `unsafe_code` is denied in `Cargo.toml`; the one
  module that needs it allows it explicitly.
- **macOS on Apple Silicon** is the only platform, and CI runs there.

## Check your change

Run what CI runs:

```sh
cargo fmt --all --check
cargo clippy --all-targets --offline --locked -- -D warnings
cargo test --offline --locked
python3 -B .github/scripts/network_inventory.py
```

The last one reviews direct network calls in the torrent, VPN, indexer and IRC
code. When it reports a new one, send it through the transport context, or
update the reviewed inventory with `--write` and say why in the pull request.

For the tests:

- Set `TMPDIR` to a short physical directory, such as `/private/tmp`. macOS's
  default temporary directory goes through a symbolic link, which Mynou's
  storage refuses, and some tests bind Unix sockets, whose paths must stay
  short.
- The cryptography, WireGuard and network stack tests read reference vectors:
  generate them with `python3 .github/scripts/crypto_fixtures.py <file>` and
  set `MYNOU_CRYPTO_FIXTURES=<file>`.
- Integration tests are modules of one `integration` test binary: declare a
  new file under `tests/` with `mod` in `tests/main.rs`.
- If you change the CI scripts or workflows, also run
  `python3 -m unittest discover -s .github/scripts/tests -p 'test_*.py'`.

## Open a pull request

- Branch from `trunk` and fill in the pull request template: the problem,
  `Closes #<issue>`, your decisions and limits, and how you checked the change.
- Pull requests are rebase-merged into `trunk`, so every commit lands as it
  is: give each one a short imperative subject, like `Fix …` or `Add …`. Keep
  the branch up to date with `trunk`; branch protection requires it.
- Add each user-facing change to `UNRELEASED.md` (create it if needed) under a
  `## <Category>: <Title>` heading, with Added, Changed, Deprecated, Removed,
  Fixed or Security as the category, followed by a short text.
- Leave the version alone: only release pull requests change it.
