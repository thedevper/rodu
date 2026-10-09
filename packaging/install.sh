#!/bin/sh
# Installs the rodu binary on macOS without Homebrew:
#
#   curl -fsSL https://raw.githubusercontent.com/TheDevper/rodu/v<version>/packaging/install.sh | sh
#
# The URL names a release tag, so what runs is the reviewed script of that release.
#
# RODU_VERSION=0.2.0       a specific release instead of the latest
# RODU_BIN_DIR=~/bin       where to put rodu (default ~/.local/bin)
# RODU_DOWNLOAD_BASE=URL   where the release files are (for testing a local build)
set -eu

REPO="TheDevper/rodu"
BIN_DIR="${RODU_BIN_DIR:-$HOME/.local/bin}"

fail() {
  echo "rodu install: $*" >&2
  exit 1
}

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target="darwin-arm64" ;;
  Darwin-x86_64) target="darwin-x64" ;;
  *) fail "no build for $(uname -s) $(uname -m) yet (macOS and Windows are supported)" ;;
esac

version="${RODU_VERSION:-}"
if [ -z "$version" ]; then
  # /releases/latest redirects to /releases/tag/vX.Y.Z; no API token or JSON parsing needed.
  latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
    fail "could not reach GitHub"
  version="${latest##*/v}"
fi
version="${version#v}"
case "$version" in
  *[!0-9.]* | "" | .* | *. | *..*) fail "unexpected version '$version' (no release published yet?)" ;;
esac
base="${RODU_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download/v$version}"
archive="rodu-v$version-$target.tar.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading rodu $version ($target)"
curl -fsSL -o "$tmp/$archive" "$base/$archive" || fail "could not download $base/$archive"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || fail "could not download SHA256SUMS"
expected=$(awk -v a="$archive" '$2 == a { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "SHA256SUMS has no entry for $archive"
actual=$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)
[ "$expected" = "$actual" ] || fail "checksum mismatch for $archive"

notices="LICENSE NOTICE THIRD-PARTY-NOTICES.txt"
# shellcheck disable=SC2086 # the names have no spaces
tar -xzf "$tmp/$archive" -C "$tmp" rodu $notices
mkdir -p "$BIN_DIR"
# Replace by rename so a running rodu keeps its old file.
mv -f "$tmp/rodu" "$BIN_DIR/rodu"
chmod 755 "$BIN_DIR/rodu"
# The licences of Rodu and of the third-party code built into it travel with the binary.
share="${XDG_DATA_HOME:-$HOME/.local/share}/rodu"
mkdir -p "$share"
for n in $notices; do mv -f "$tmp/$n" "$share/$n"; done
echo "Installed $("$BIN_DIR/rodu" --version) to $BIN_DIR/rodu (licences in $share)"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    echo
    echo "$BIN_DIR is not on your PATH yet. Add it, then open a new terminal:"
    case "${SHELL:-}" in
      */fish) echo "  fish_add_path $BIN_DIR" ;;
      */bash) echo "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bash_profile" ;;
      *) echo "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc" ;;
    esac
    ;;
esac
echo
echo "Start: mkdir ~/rodu && cd ~/rodu && rodu init --name <you> --key <KEY>, then rodu web"
