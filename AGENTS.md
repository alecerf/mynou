# Mynou development rules

- Use Rust and its standard library exclusively. No Cargo dependencies of any
  kind, including development and build dependencies.
- Do not copy third-party code, use FFI or `unsafe`, invoke external programs at
  runtime, or fall back to the former Go implementation. The Rust toolchain is a
  build tool.
- Write bounded parsers, explicit errors, verified persistent data, and narrowly
  scoped side effects. Network operations use the standard library.
- Write all source comments, diagnostics, documentation, workflow messages, and
  release notes in English. User-provided media titles remain unchanged.
- Never run tests or lint locally. Make meaningful commits as the work proceeds,
  push the completed changes, and inspect GitHub Actions. Continue only after the
  required checks are green. If a check fails, fix it, commit, push, and inspect
  the next run. Formatting edits are allowed; validation belongs in CI.
- CI checks the offline Cargo graph, formatting, Clippy, tests, and release
  builds. The graph must contain exactly one package, `mynou`, with no dependencies.
- GitHub Actions alone creates release tags and publishes release assets after
  successful validation. Do not create releases or upload release artifacts
  manually. Keep release creation tied to the validated commit.
- Automated network tests use local peers and synthetic media. Read-only HTTPS
  requests to official services may diagnose TLS interoperability. Do not
  download public torrents in tests.
- Never claim an unimplemented feature, an unmeasured performance result, or a
  passing CI run that has not completed.
- Preserve data from the Go releases separately. Their state is not implicitly
  supported by the Rust persistence format.
