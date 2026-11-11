#!/bin/sh
# Installs Mynou on a Mac with Apple Silicon:
#
#   curl -fsSL https://raw.githubusercontent.com/alecerf/mynou/trunk/install.sh | sh
#
# It installs the latest release, or the version given as an argument
# (`| sh -s -- 0.29.0`) or in MYNOU_VERSION, into MYNOU_INSTALL_DIR, by default
# ~/.local/bin. Nothing is installed unless the release's SHA256SUMS carries a
# valid signature by the release key below and lists the executable's
# checksum. Running it again upgrades in place. It needs no sudo, and writes
# only to a temporary directory and the installation directory.
set -eu

REPO=https://github.com/alecerf/mynou
# The public half of the release key, the same as release-key.pub.pem.
RELEASE_KEY='-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE5Ng3l4dTm52UkC/fb4ybl03AfBW6
jN1Sd5j7lx+XwNhVxyjV+QgDD/lWQn0nOoWaZUT3IUt7rSdbVzN6OkOEVA==
-----END PUBLIC KEY-----'

tmp=
staged=

if [ -t 1 ] && [ -z "${NO_COLOR-}" ] && [ "${TERM-}" != dumb ]; then
  bold=$(printf '\033[1m') dim=$(printf '\033[2m') green=$(printf '\033[32m')
  yellow=$(printf '\033[33m') reset=$(printf '\033[0m')
else
  bold= dim= green= yellow= reset=
fi

ok() { printf '  %s✓%s %s\n' "$green" "$reset" "$1"; }

fail() {
  if [ -t 2 ] && [ -z "${NO_COLOR-}" ] && [ "${TERM-}" != dumb ]; then
    printf '\033[1;31mError:\033[0m %s\n' "$1" >&2
  else
    printf 'Error: %s\n' "$1" >&2
  fi
  exit 1
}

usage() {
  cat <<'EOF'
Installs Mynou on a Mac with Apple Silicon.

Usage: install.sh [VERSION]

  VERSION             a release such as 0.29.0; the latest one by default
                      (also read from MYNOU_VERSION)
  MYNOU_INSTALL_DIR   where to install mynou; ~/.local/bin by default
EOF
}

cleanup() {
  if [ -n "$staged" ]; then rm -f "$staged"; fi
  if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
}

fetch() {
  curl --fail --silent --show-error --location --retry 3 --connect-timeout 20 \
    --output "$2" "$1"
}

check_platform() {
  system=$(uname -s)
  [ "$system" = Darwin ] ||
    fail "Mynou runs only on macOS with Apple Silicon, not on $system."
  # 1 on Apple Silicon, also when this shell runs under Rosetta; Intel Macs
  # do not have it.
  [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ] ||
    fail "Mynou runs only on Macs with Apple Silicon (M1 or later); there is no Intel build."
}

valid() { printf '%s\n' "$1" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+'; }

resolve_version() {
  case $1 in
    '' | latest)
      # GitHub redirects releases/latest to the tag of the latest release,
      # without the rate limit of its API.
      latest=$(curl --fail --silent --show-error --retry 3 --connect-timeout 20 \
        --output /dev/null --write-out '%{redirect_url}' "$REPO/releases/latest") ||
        fail "Cannot reach GitHub to find the latest release."
      version=${latest##*/tag/v}
      label="latest release"
      valid "$version" || fail "Cannot find the latest release at $REPO/releases."
      ;;
    *)
      version=${1#v}
      label="requested version"
      valid "$version" || fail "$1 is not a version such as 0.29.0; see $REPO/releases."
      ;;
  esac
}

main() {
  case ${1-} in
    -h | --help) usage; return ;;
  esac
  [ $# -le 1 ] || fail "Too many arguments; see install.sh --help."

  check_platform
  for tool in curl openssl shasum; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is missing; macOS normally ships it."
  done
  if [ -z "${MYNOU_INSTALL_DIR-}" ]; then
    [ -n "${HOME-}" ] || fail "HOME is not set; set MYNOU_INSTALL_DIR instead."
  fi
  dir=${MYNOU_INSTALL_DIR:-$HOME/.local/bin}
  dir=${dir%/}
  case $dir in
    /*) ;;
    *) fail "MYNOU_INSTALL_DIR must be an absolute path, not $dir." ;;
  esac
  target=$dir/mynou
  shown=$dir
  if [ -n "${HOME-}" ]; then
    case $dir in "$HOME"/*) shown="~${dir#"$HOME"}" ;; esac
  fi

  resolve_version "${1:-${MYNOU_VERSION-}}"
  asset=mynou-v$version-macos-arm64
  printf '\n%sInstalling Mynou %s%s %s(%s)%s\n\n' "$bold" "$version" "$reset" "$dim" "$label" "$reset"

  trap cleanup EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/mynou-install.XXXXXX") ||
    fail "Cannot create a temporary directory."
  base=$REPO/releases/download/v$version
  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" ||
    fail "Cannot download Mynou $version. Check the version at $REPO/releases."
  fetch "$base/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig" ||
    fail "Cannot download the signature of Mynou $version; only releases from 0.29.0 on are signed."
  fetch "$base/$asset" "$tmp/$asset" || fail "Cannot download $asset."
  ok "Downloaded $asset"

  printf '%s\n' "$RELEASE_KEY" > "$tmp/release-key.pub.pem"
  openssl dgst -sha256 -verify "$tmp/release-key.pub.pem" \
    -signature "$tmp/SHA256SUMS.sig" "$tmp/SHA256SUMS" >/dev/null 2>&1 ||
    fail "SHA256SUMS is not signed by Mynou's release key. Nothing was installed."
  ok "SHA256SUMS is signed by the release key"

  expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name { n++; sum = $1 } END { if (n == 1) print sum }' \
    "$tmp/SHA256SUMS")
  case $expected in
    *[!0-9a-f]* | '') fail "SHA256SUMS does not list $asset. Nothing was installed." ;;
  esac
  actual=$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')
  [ "${#expected}" -eq 64 ] && [ "$actual" = "$expected" ] ||
    fail "$asset does not match its checksum in SHA256SUMS. Nothing was installed."
  ok "The checksum of $asset matches"

  # Staged next to its final name, then renamed over it in one step: a running
  # Mynou keeps its executable, and an interrupted install leaves the old one.
  mkdir -p "$dir" || fail "Cannot create $dir."
  [ ! -d "$target" ] || fail "$target is a directory."
  [ -e "$target" ] && upgrade=1 || upgrade=
  staged=$(mktemp "$dir/.mynou.XXXXXX") || fail "Cannot write to $dir."
  cp "$tmp/$asset" "$staged"
  chmod 755 "$staged"
  installed=$("$staged" --version) ||
    fail "The downloaded mynou does not run on this Mac. Nothing was installed."
  mv -f "$staged" "$target"
  staged=
  ok "Installed $shown/mynou"

  printf '\n%s✓ %s%s\n' "$green$bold" "$installed" "$reset"
  if [ -n "$upgrade" ]; then
    printf '  Restart Mynou if it is running, to use this version.\n'
  fi

  case ${SHELL-} in
    */bash) shell=bash rc='~/.bash_profile' ;;
    *) shell=zsh rc='~/.zshrc' ;;
  esac
  case ":${PATH-}:" in
    *":$dir:"*)
      found=$(command -v mynou || true)
      if [ -n "$found" ] && [ "$found" != "$target" ]; then
        printf '\n%s!%s %s comes first on your PATH; remove it to use this one.\n' \
          "$yellow" "$reset" "$found"
      fi
      ;;
    *)
      line="export PATH=\"$dir:\$PATH\""
      if [ "$shown" != "$dir" ]; then
        line="export PATH=\"\$HOME${dir#"$HOME"}:\$PATH\""
      fi
      printf '\n%s!%s %s is not on your PATH. Add this line to %s, then open a new terminal:\n\n    %s\n' \
        "$yellow" "$reset" "$shown" "$rc" "$line"
      ;;
  esac
  if ! grep -qs 'mynou completion' "$HOME/${rc#\~/}"; then
    printf '\nFor tab completion of mynou commands, add this line to %s:\n\n    eval "$(mynou completion --shell %s)"\n' \
      "$rc" "$shell"
  fi

  cat <<EOF

${bold}Next steps${reset}
  mynou serve                      ${dim}start Mynou${reset}
  http://127.0.0.1:8787/           ${dim}open it in your browser and set it up${reset}
  mynou service --action install   ${dim}start it at login instead (stop mynou serve first)${reset}

Mynou keeps itself up to date from now on.
EOF
}

main "$@"
