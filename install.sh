#!/bin/sh
# Installs the chitchat binary from GitHub releases.
#
#   curl -fsSL https://raw.githubusercontent.com/cyoab/chitchat/main/install.sh | sh
#
# Options (environment variables):
#   CHITCHAT_VERSION       release tag to install, e.g. v0.2.0 (default: latest)
#   CHITCHAT_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   CHITCHAT_DOWNLOAD_BASE download from here instead of GitHub (mirrors, testing)
#
# Then, in each project: chitchat init

set -eu

REPO="cyoab/chitchat"
VERSION="${CHITCHAT_VERSION:-latest}"
INSTALL_DIR="${CHITCHAT_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() {
    printf 'chitchat install: %s\n' "$*" >&2
    exit 1
}

case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux) os="unknown-linux-musl" ;;
    *) fail "unsupported OS $(uname -s); build from source: cargo install --git https://github.com/$REPO" ;;
esac
case "$(uname -m)" in
    arm64 | aarch64) arch="aarch64" ;;
    x86_64 | amd64) arch="x86_64" ;;
    *) fail "unsupported CPU $(uname -m); build from source: cargo install --git https://github.com/$REPO" ;;
esac
asset="chitchat-$arch-$os.tar.gz"

if [ -n "${CHITCHAT_DOWNLOAD_BASE:-}" ]; then
    base="$CHITCHAT_DOWNLOAD_BASE"
elif [ "$VERSION" = "latest" ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$VERSION"
fi

if command -v curl > /dev/null 2>&1; then
    fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget > /dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    fail "need curl or wget"
fi

if command -v sha256sum > /dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum > /dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    fail "need sha256sum or shasum to verify the download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $asset ($VERSION)..."
fetch "$base/$asset" "$tmp/$asset" || fail "download failed: $base/$asset"
fetch "$base/$asset.sha256" "$tmp/$asset.sha256" || fail "download failed: $base/$asset.sha256"

expected="$(cut -d ' ' -f 1 < "$tmp/$asset.sha256")"
actual="$(sha256 "$tmp/$asset")"
[ "$expected" = "$actual" ] || fail "checksum mismatch for $asset (expected $expected, got $actual)"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
# Write next to the target and rename, so a running chitchat is never half-overwritten.
cp "$tmp/chitchat" "$INSTALL_DIR/.chitchat.new"
chmod 755 "$INSTALL_DIR/.chitchat.new"
mv -f "$INSTALL_DIR/.chitchat.new" "$INSTALL_DIR/chitchat"

say "Installed $("$INSTALL_DIR/chitchat" --version) to $INSTALL_DIR/chitchat"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        say ""
        say "$INSTALL_DIR is not on your PATH. Add this to your shell profile:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
say ""
say "Next, in each project you want agents to share:"
say "  cd path/to/project && chitchat init"
