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
  # Keep the pushed tag: only the unfinished draft is replaced.
  gh release delete "$tag" --repo "$GITHUB_REPOSITORY" --yes
fi

notes="$RUNNER_TEMP/mynou-release-notes.md"
: > "$notes"
# The release commit deleted the unreleased notes; the changelog is generated from them.
git -C "$GITHUB_WORKSPACE" show "$GITHUB_SHA^:UNRELEASED.md" \
  | env -u GH_TOKEN python3 "$GITHUB_WORKSPACE/.github/scripts/releases.py" notes --tag "$tag" >> "$notes"
if previous=$(gh api "repos/$GITHUB_REPOSITORY/releases/latest" --jq .tag_name 2> /dev/null); then
  printf '\n**Full changelog**: %s/%s/compare/%s...%s\n' "$GITHUB_SERVER_URL" "$GITHUB_REPOSITORY" "$previous" "$tag" >> "$notes"
fi
printf '\n' >> "$notes"
cat >> "$notes" <<EOF
<!-- mynou-ci-release -->
Mynou $version uses Rust 1.99.1 and the standard library only: no Cargo dependencies, FFI, or external programs at runtime, and no unsafe code outside the SHA instructions.

This release was built by [GitHub Actions]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID) from a trunk commit that only raised the version of a tree whose dependency, formatting, Clippy, test and native start checks passed on macOS in its pull request, then signed and published with the release key, which never reaches GitHub.

Release assets contain only the native macOS Apple Silicon executable, one \`SHA256SUMS\` manifest and its \`SHA256SUMS.sig\` signature by the release key, made outside GitHub. Verify the signature and the manifest entry before installation; the [quick start]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/README.md#quick-start) explains how. GitHub's native source downloads remain available.

Follow the [README]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$tag/README.md). No Apple Developer signature or notarization is supplied. Configure Plex and media sources before synchronization.

Source commit: \`$GITHUB_SHA\`. Release downloads follow repository access settings.
EOF

# The release names the pushed tag, which must still point at this commit;
# it is never created or moved here.
[[ $(gh api "repos/$GITHUB_REPOSITORY/commits/$tag" --jq .sha) == "$GITHUB_SHA" ]]
gh release create "$tag" --repo "$GITHUB_REPOSITORY" --verify-tag \
  --title "Mynou $version" --notes-file "$notes" --draft
gh release upload "$tag" ./* --repo "$GITHUB_REPOSITORY"
expected_files=$(find . -maxdepth 1 -type f | wc -l)
uploaded_files=$(gh release view "$tag" --repo "$GITHUB_REPOSITORY" --json assets --jq '.assets|length')
[[ "$uploaded_files" -eq "$expected_files" ]]
# The draft is published by .github/scripts/sign-release.sh on the
# maintainer's Mac, once the release key signed SHA256SUMS.
printf 'Draft %s is ready to sign.\n' "$tag"
