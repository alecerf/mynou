#!/usr/bin/env bash
# Signs and publishes a release that CI left as a draft. Runs on the
# maintainer's Mac: the private release key never reaches GitHub.
#   .github/scripts/sign-release.sh VERSION
set -euo pipefail

version="$1"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
tag="v$version"
name="mynou-v$version-macos-arm64"
repo="${MYNOU_RELEASE_REPO:-alecerf/mynou}"
key="${MYNOU_RELEASE_KEY:-$HOME/.mynou-release/release-key.pem}"
public="$(cd "$(dirname "$0")/../.." && pwd)/docs/release-key.pub.pem"
test -f "$key"

[[ $(gh release view "$tag" --repo "$repo" --json isDraft --jq .isDraft) == true ]] || {
  printf 'Release %s is not a draft; published releases are never re-signed.\n' "$tag" >&2
  exit 1
}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
gh release download "$tag" --repo "$repo" --dir "$work" --pattern SHA256SUMS --pattern "$name"
cd "$work"
[[ $(wc -l < SHA256SUMS) -eq 1 ]]
[[ $(awk '{print $2}' SHA256SUMS) == "$name" ]]
shasum -a 256 -c SHA256SUMS
# Never run the downloaded executable here: this Mac holds the release key.
grep -aqF "$version" "$name"

openssl dgst -sha256 -sign "$key" -out SHA256SUMS.sig SHA256SUMS
openssl dgst -sha256 -verify "$public" -signature SHA256SUMS.sig SHA256SUMS
gh release upload "$tag" SHA256SUMS.sig --repo "$repo"
gh release edit "$tag" --repo "$repo" --draft=false --latest
printf 'Published %s.\n' "$tag"
