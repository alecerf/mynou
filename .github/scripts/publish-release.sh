#!/usr/bin/env bash
set -euo pipefail

release_dir="$1"
version="$2"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
tag="v$version"
cd "$release_dir"
sha256sum -c SHA256SUMS
for checksum in *.sha256; do sha256sum -c "$checksum"; done

# Published versions are immutable. A new version belongs in Cargo.toml.
if existing=$(gh release view "$tag" --repo "$GITHUB_REPOSITORY" --json isDraft,body --jq '(.isDraft|tostring)+"\n"+.body' 2>/dev/null); then
  if [[ "${existing%%$'\n'*}" == false ]]; then
    printf 'Version %s is already published.\n' "$version"
    exit 0
  fi
  if [[ "$existing" != *'<!-- mynou-ci-release -->'* ]]; then
    printf 'Refusing to replace a draft that was not created by Mynou CI.\n' >&2
    exit 1
  fi
  gh release delete "$tag" --repo "$GITHUB_REPOSITORY" --yes --cleanup-tag
fi

notes="$RUNNER_TEMP/mynou-release-notes.md"
cat > "$notes" <<EOF
<!-- mynou-ci-release -->
Mynou $version uses Rust 1.99.0 and the standard library only: no Cargo dependencies, unsafe code, FFI, or external programs at runtime.

This release was built and published by [GitHub Actions]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID) after dependency, formatting, Clippy, test, native demonstration and Docker checks passed.

The source ZIP includes the complete project and a static Linux x86_64 binary. The standalone binary and Docker image archive are also supplied separately. Verify downloads with \`sha256sum -c SHA256SUMS\`.

Load the Docker image with \`docker load -i mynou-v$version-linux-amd64-image.tar.gz\`, then follow the [Docker deployment guide]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/deployment.md). Configure your Plex server and media sources before enabling synchronization. [Protocol and format limits]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/limits.md) remain explicit.

Source commit: \`$GITHUB_SHA\`. This repository is private; downloads require an authorized GitHub account.
EOF

gh release create "$tag" --repo "$GITHUB_REPOSITORY" --target "$GITHUB_SHA" \
  --title "Mynou $version" --notes-file "$notes" --draft
gh release upload "$tag" ./* --repo "$GITHUB_REPOSITORY"
expected_files=$(find . -maxdepth 1 -type f | wc -l)
uploaded_files=$(gh release view "$tag" --repo "$GITHUB_REPOSITORY" --json assets --jq '.assets|length')
[[ "$uploaded_files" -eq "$expected_files" ]]
gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false --latest
