#!/bin/sh
# Installs the legio binary for this Mac or Linux machine.
#
#   curl -fsSL https://raw.githubusercontent.com/jrobertojunior/legio-cli/master/install.sh | sh
#
# LEGIO_VERSION=v0.1.0  installs that tag. The default is the latest release.
# LEGIO_DIR=/usr/local/bin  installs there. The default is ~/.local/bin.
set -eu

REPO=jrobertojunior/legio-cli
VERSION=${LEGIO_VERSION:-latest}
DIR=${LEGIO_DIR:-$HOME/.local/bin}

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Darwin-arm64 | Darwin-x86_64 | Linux-x86_64 | Linux-aarch64) ;;
  Linux-arm64) arch=aarch64 ;;
  *) echo "legio: no build for $os $arch" >&2; exit 1 ;;
esac
asset="legio-$os-$arch.tar.gz"

if [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if ! curl -fsSL "$base/$asset" -o "$tmp/$asset"; then
  echo "legio: cannot download $base/$asset" >&2
  exit 1
fi

# The release lists the SHA-256 of each tarball. A file that does not
# match is not installed.
if curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"; then
  want=$(awk -v a="$asset" '$2 == a || $2 == "*" a { print $1 }' "$tmp/SHA256SUMS")
  if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "$tmp/$asset" | awk '{ print $1 }')
  else
    got=$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')
  fi
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    echo "legio: $asset does not match SHA256SUMS. Nothing was installed." >&2
    exit 1
  fi
else
  echo "legio: cannot download SHA256SUMS. Nothing was installed." >&2
  exit 1
fi

mkdir -p "$DIR"
tar -xzf "$tmp/$asset" -C "$DIR"
chmod +x "$DIR/legio"
echo "legio: installed $("$DIR/legio" --version 2>/dev/null || echo legio) in $DIR"
echo "legio: update it later with: legio update"

case ":$PATH:" in
  *":$DIR:"*)
    # An older legio earlier in PATH (from cargo install, for example)
    # would run in place of this one.
    found=$(command -v legio 2>/dev/null || true)
    if [ -n "$found" ] && [ "$found" != "$DIR/legio" ]; then
      echo "legio: WARNING: 'legio' runs $found, not $DIR/legio." >&2
      echo "legio: remove $found, or put $DIR first in your PATH." >&2
    fi
    ;;
  *) echo "legio: add $DIR to your PATH, for example: export PATH=\"$DIR:\$PATH\"" ;;
esac
