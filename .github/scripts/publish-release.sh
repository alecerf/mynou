#!/usr/bin/env bash
set -euo pipefail

release_dir="$1"
version="$2"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
tag="v$version"
cd "$release_dir"
sha256sum -c SHA256SUMS
[[ "$MYNOU_CONTAINER_REF" =~ ^ghcr\.io/alecerf/mynou@sha256:[0-9a-f]{64}$ ]]
[[ $(find . -maxdepth 1 -type f | wc -l) -eq 4 ]]
for suffix in linux-x86_64 macos-arm64 macos-x86_64; do
  test -f "mynou-v$version-$suffix"
  test ! -L "mynou-v$version-$suffix"
done
[[ $(wc -l < SHA256SUMS) -eq 3 ]]

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
: > "$notes"
changes="$GITHUB_WORKSPACE/docs/releases/$version.md"
if [[ -f "$changes" ]]; then
  cat "$changes" >> "$notes"
  printf '\n\n' >> "$notes"
fi
cat >> "$notes" <<EOF
<!-- mynou-ci-release -->
Mynou $version uses Rust 1.99.0 and the standard library only: no Cargo dependencies, unsafe code, FFI, or external programs at runtime.

This release was built and published by [GitHub Actions]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID) after dependency, formatting, Clippy, test, native demonstration and Docker checks passed.

Release assets contain only the static Linux x86_64 executable, native macOS Apple Silicon/Intel executables and one \`SHA256SUMS\` manifest. Verify the selected file's manifest entry before installation; the [verification guide]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/validation.md#verify-release-assets) explains partial downloads. Custom source ZIPs, image archives and duplicate checksum files are no longer published. GitHub's native source downloads remain available.

The checked Linux amd64 container is published separately to private GitHub Container Registry. Its verified content-addressed reference is:

\`$MYNOU_CONTAINER_REF\`

Authenticate to GHCR with package read access and run \`docker pull $MYNOU_CONTAINER_REF\`. Use this digest to pin deployment: registry tags can be changed by authorized writers. Follow the [Docker deployment guide]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/deployment.md). Container publication re-pulls the digest, verifies the checked executable and runs the isolated local demonstration before creating this release.

For macOS, follow the [native macOS guide]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/macos.md). No Apple Developer signature or notarization is supplied. Configure Plex and media sources before synchronization. [Protocol and format limits]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/limits.md) remain explicit.

Source commit: \`$GITHUB_SHA\`. This repository is private; downloads require an authorized GitHub account.

See the [release roadmap]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/roadmap.md) for implemented milestones and remaining feature gaps.
EOF

gh release create "$tag" --repo "$GITHUB_REPOSITORY" --target "$GITHUB_SHA" \
  --title "Mynou $version" --notes-file "$notes" --draft
gh release upload "$tag" ./* --repo "$GITHUB_REPOSITORY"
expected_files=$(find . -maxdepth 1 -type f | wc -l)
uploaded_files=$(gh release view "$tag" --repo "$GITHUB_REPOSITORY" --json assets --jq '.assets|length')
[[ "$uploaded_files" -eq "$expected_files" ]]
gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false --latest
