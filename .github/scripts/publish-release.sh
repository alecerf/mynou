#!/usr/bin/env bash
set -euo pipefail

release_dir="$1"
version="$2"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
tag="v$version"
cd "$release_dir"
sha256sum -c SHA256SUMS
[[ $(find . -maxdepth 1 -type f | wc -l) -eq 2 ]]
test -f "mynou-v$version-macos-arm64"
test ! -L "mynou-v$version-macos-arm64"
[[ $(wc -l < SHA256SUMS) -eq 1 ]]
[[ $(awk '{print $2}' SHA256SUMS) == "mynou-v$version-macos-arm64" ]]

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
# The release commit deleted the unreleased notes; they ship in this release.
git -C "$GITHUB_WORKSPACE" show "$GITHUB_SHA^:docs/releases/unreleased.md" >> "$notes"
printf '\n\n' >> "$notes"
cat >> "$notes" <<EOF
<!-- mynou-ci-release -->
Mynou $version uses Rust 1.99.0 and the standard library only: no Cargo dependencies, unsafe code, FFI, or external programs at runtime.

This release was built and published by [GitHub Actions]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID) after dependency, formatting, Clippy, test and native demonstration checks passed on macOS.

Release assets contain only the native macOS Apple Silicon executable and one \`SHA256SUMS\` manifest. Verify the manifest entry before installation; the [verification guide]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/validation.md#verify-release-assets) explains how. GitHub's native source downloads remain available.

Follow the [macOS guide]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/macos.md). No Apple Developer signature or notarization is supplied. Configure Plex and media sources before synchronization. [Protocol and format limits]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/limits.md) remain explicit.

Source commit: \`$GITHUB_SHA\`. Release downloads follow repository access settings.

See the [documentation]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/README.md#documentation) for guides, and the [limits]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/docs/limits.md#not-supported) for features Mynou does not provide.
EOF

gh release create "$tag" --repo "$GITHUB_REPOSITORY" --target "$GITHUB_SHA" \
  --title "Mynou $version" --notes-file "$notes" --draft
gh release upload "$tag" ./* --repo "$GITHUB_REPOSITORY"
expected_files=$(find . -maxdepth 1 -type f | wc -l)
uploaded_files=$(gh release view "$tag" --repo "$GITHUB_REPOSITORY" --json assets --jq '.assets|length')
[[ "$uploaded_files" -eq "$expected_files" ]]
gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false --latest
