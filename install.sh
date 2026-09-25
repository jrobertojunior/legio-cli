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
  url="https://github.com/$REPO/releases/latest/download/$asset"
else
  url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# curl works when the repository is public. gh works when it is private.
if ! curl -fsSL "$url" -o "$tmp/$asset" 2>/dev/null; then
  if command -v gh >/dev/null 2>&1; then
    if [ "$VERSION" = latest ]; then tag=; else tag=$VERSION; fi
    gh release download $tag -R "$REPO" -p "$asset" -D "$tmp"
  else
    echo "legio: cannot download $url" >&2
    echo "legio: the repository may be private. Install gh, run 'gh auth login', and try again." >&2
    exit 1
  fi
fi

mkdir -p "$DIR"
tar -xzf "$tmp/$asset" -C "$DIR"
chmod +x "$DIR/legio"
echo "legio: installed $("$DIR/legio" --version 2>/dev/null || echo legio) in $DIR"

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "legio: add $DIR to your PATH, for example: export PATH=\"$DIR:\$PATH\"" ;;
esac
